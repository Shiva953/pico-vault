use core::panic;
use std::{collections::HashMap, vec};
use tokio::net::{TcpListener, TcpStream};
use anyhow::{Error, Result};
use std::sync::Arc;
use resp::Value;
use std::env;
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, Mutex};

mod resp;

struct ServerInfo {
    role: String,
    master_replid: String,
    master_repl_offset: i32
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mut replica_info: Option<String> = None;

    let default_port = String::from("6379");
    let mut port_wrap = None;
    let port_pos = args.iter().position(|s| s=="--port");
    if let Some(pos) = port_pos{
        port_wrap = Some(args[pos + 1].clone());
    }
    let port = port_wrap.unwrap_or(default_port);
    let address = format!("127.0.0.1:{}", port);
    let mut role = "master".to_string();
    let replica_flag_position = args.iter().position(|s| s == "--replicaof");

    if let Some(pos) = replica_flag_position {
        replica_info = Some(args[pos + 1].clone());
        role = "slave".to_string();
    }
    let mut master_ip = None ;
    let mut master_port = None;
    
    let server_info = Arc::new(Mutex::new(ServerInfo{role: role.clone().to_string(), master_replid: "8371b4fb1155b71f4a04d3e1bc3e18c4a990aeeb".to_string(), master_repl_offset: 0}));
   
    let listener = TcpListener::bind(address.clone()).await.unwrap(); //listening for connections in the MASTER server
    println!("Listening on port {port}");
    let (sender, mut receiver) = broadcast::channel::<String>(100);

    if role == "slave" {
        let info = replica_info.unwrap();
        let parts: Vec<&str> = info.split_whitespace().collect();
        master_ip = Some(parts[0].to_string());
        master_port = Some(parts[1].to_string());
        println!("{:?}", format!("{}:{}", &master_ip.clone().unwrap(), &master_port.clone().unwrap()));
        // a new thread for slave -> master connection
        tokio::spawn( async move{
            match TcpStream::connect(format!("{}:{}", master_ip.unwrap().clone(), master_port.unwrap().clone())).await { //slave server connecting to master
                Ok(sockt) => {
                    println!("Slave Connected to master");
                    let mut handler = resp::RespHandler::new(sockt);
                    
                    //establishing "handshake"
                    // Send PING
                    handler.write_value(Value::BulkString("ping".to_string())).await;
                    // let res = sockt.read(&mut [0; 512]).await.unwrap();
                    // let response = timeout(Duration::from_secs(5), sockt.read(&mut [0; 512])).await;
                    let response = handler.read_value().await.unwrap();
                    println!("{:?}", response.unwrap().serialize().as_str());
                    println!("OK");
                    
        
                    // Send REPLCONF listening-port <PORT>
                    handler.write_value(Value::Array(vec![
                        Value::BulkString("REPLCONF".to_string()),
                        Value::BulkString("listening-port".to_string()),
                        Value::BulkString(port.to_string()),
                    ])).await;
                    let response = handler.read_value().await.unwrap();
                    
                    println!("Received: {:?}", response.unwrap().serialize().as_str());
        
                    // Send REPLCONF capa psync2
                    handler.write_value(Value::Array(vec![
                        Value::BulkString("REPLCONF".to_string()),
                        Value::BulkString("capa".to_string()),
                        Value::BulkString("psync2".to_string()),
                    ])).await;
                    let response = handler.read_value().await.unwrap();
                    println!("Received: {:?}", response.unwrap().serialize().as_str());
        
                    // Send PSYNC ? -1
                    handler.write_value(Value::Array(vec![
                        Value::BulkString("PSYNC".to_string()),
                        Value::BulkString("?".to_string()),
                        Value::BulkString("-1".to_string()),
                    ])).await;
                    let response = handler.read_value().await.unwrap();
                    println!("Received: {:?}", response.unwrap().serialize().as_str());

                    let received = receiver.recv().await;
                    match received {
                        Ok(val) => {
                            println!("Received in channel: {:?}", val);
                        }
                        Err(e) => {println!("Couldn't get the sender value!");}
                    }
                }
                Err(e) => {
                    println!("Failed to connect to master: {}", e);
                }
            }
        });
    }
    
        // for the command replication from master to replica, open up a new channel that sends command from client-master Thread1 -> master-replica Thread2 
        //creating a global k-v store, which gets updated each time a client(prev/new) adds a new (k,v)
        let kv_store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::<String,String>::new()));
        //HANDLING CONCURRENT CLIENTS, NEW THREAD FOR EACH CLIENTi/SERVER stream
        loop{ //INSTEAD OF USING for stream in listener.incoming() and synchronously iterating over each stream, we are asynchronously iterating over each stream till the data from the buffer ends
        let stream = listener.accept().await; // listener.accept().await ASYNCHRONOUSLY WAITS FOR A NEW CONNECTION, INSTEAD OF AN SYNCHRONOUS ITERATOR LIKE listener.incoming() which takes each new connection and puts inside it


        let mut kv_store: Arc<Mutex<HashMap<String, String>>> = Arc::clone(&kv_store);
        let mut server_store: Arc<Mutex<ServerInfo>> = Arc::clone(&server_info);

        let sender = sender.clone();
        
        match stream { 
            Ok((stream, _)) => {
                //SPAWNING A NEW THREAD FOR EACH CLIENT REQ->S
                //tried using threadpool and pool.execute, turns out each thread in it was unable to handle ASYNC read/write tasks
                //the below spawns a new ASYNC THREAD for each new client request to the redis server
                tokio::spawn(async move{
                    handle_conn(stream, &mut kv_store, &mut server_store, &sender).await;
                });
                
            }
            Err(e) => {
                println!("{e}");
            }
        }
    }
}

async fn handle_conn(stream: TcpStream, kv_store: &mut Arc<tokio::sync::Mutex<HashMap<String, String>>>, server_store: &mut Arc<tokio::sync::Mutex<ServerInfo>>, sender: &Sender<String>) {
    let mut handler = resp::RespHandler::new(stream);
    loop{
        let value = handler.read_value().await.unwrap(); //ALL PARSING HAPPENS IN THS FUNCTION 
        
        let res = if let Some(v) = value.clone() {
            //this kinda assumes that whatever value must be coming must be a command
            let (command, args) = extract_command(v).await.unwrap();
            //rdb transfer
            // After receiving a response to the last command, the tester will expect to receive an empty RDB file from your server.
            match command.as_str().to_lowercase().as_str() {
                "ping" => Value::SimpleString("PONG".to_string()),
                "echo" => args.first().unwrap().clone(),
                "set" => {
                    store_key_value(args, kv_store).await.unwrap_or(Value::SimpleString("Can only have 2 arguments!".to_string()))
                },
                "get" => {
                    get_value_from_key(args, kv_store).await.unwrap_or(Value::SimpleString("Can have only 1 key as argument!".to_string()))
                }, //by default, consider a input string as bulk string
                "incr" => {
                    incr_command(args, kv_store).await.unwrap()
                },
                "info" => {get_info(unpack_bulk_str(args[0].clone()).await.unwrap(), server_store).await},
                "replconf" => Value::SimpleString("OK".to_string()),
                "psync" => {
                    // send an empty RDB file
                    Value::Array(vec![Value::SimpleString(format!("FULL RESYNC {} 0", server_store.lock().await.master_replid)), Value::BulkString(String::from_utf8_lossy(&hex::decode("524544495330303131fa0972656469732d76657205372e322e30fa0a72656469732d62697473c040fa056374696d65c26d08bc65fa08757365642d6d656dc2b0c41000fa08616f662d62617365c000fff06e3bfec0ff5aa2").unwrap()).to_string())])
                },
                c => panic!("Cannot handle command {}", c),
            }
        } else {
            break;
        };
        handler.write_value(res).await;
        // println!("{:?}", extract_command(value.clone().unwrap()).unwrap().0);
        match value.clone() {
            Some(x) => {
                match extract_command(x).await{
                    Ok(y) => {
                        match sender.send(y.0){
                            Ok(val) => {println!("{val}");},
                            Err(e) => { println!("Unable to Send Value, No receiving end!"); }
                        };
                    },
                    Err(e) => {println!("{:?}", e)}
                }
            },
            None => {println!("Command Not Found!");}
        }
        // sender.send(extract_command(value.clone().unwrap()).unwrap().0).unwrap();
        // handler.write_value(Value::BulkString(extract_command(value.clone().unwrap()).unwrap().0)).await;
    }
    
}
//makes sense to store in a global shared hashmap
async fn store_key_value(args: Vec<Value>, kv_store: &mut Arc<tokio::sync::Mutex<HashMap<String, String>>>) -> Result<Value>{
    if args.len()!=2 {
        return Err(Error::msg("Can't have more/less than 2 arguments"));
    }
    
    let key = unpack_bulk_str(args[0].clone()).await.unwrap();
    let value = unpack_bulk_str(args[1].clone()).await.unwrap();
    kv_store.lock().await.insert(key, value);
    println!("{:?}", kv_store.lock().await);
    return Ok(Value::SimpleString("OK".to_string()));
}

async fn get_value_from_key(args: Vec<Value>, kv_store: &mut Arc<tokio::sync::Mutex<HashMap<String, String>>>) -> Result<Value>{
    if args.len()!=1 {
        return Err(Error::msg("Can have only 1 key as argument!"));
    }
    let key = unpack_bulk_str(args[0].clone()).await.unwrap();
    println!("{:?}", kv_store);
    match kv_store.lock().await.get(&key) {
        Some(v) => Ok(Value::BulkString(v.to_string())),
        None => Ok(Value::SimpleString("(null)".to_string()))
    }
}

async fn incr_command(args: Vec<Value>, kv_store: &mut Arc<tokio::sync::Mutex<HashMap<String, String>>>) -> Result<Value>{
    //1. key exits && value exists as integer -> incr by 1
    //2. key DNE -> set value as 1
    //3. key exists && value is not integer -> ERR(val not integer)

    let mut store = kv_store.lock().await;
    match unpack_bulk_str(args[0].clone()).await{
        Ok(k) => {
            //if key exists, then value would by-default exist
            // but value can be integer/non-integer
            match store.get(&k) {
                Some(v) => {
                    match v.parse::<i32>() {
                        Ok(val) => {
                            store.insert(k, (val+1).to_string()).unwrap();
                            Ok(Value::SimpleString((val+1).to_string()))
                        },
                        Err(e) => {
                            Ok(Value::BulkString("(error) ERR value is not an integer or out of range".to_string()))
                        }
                    }
                },
                None => {
                    store.insert(k, "1".to_string());
                    Ok(Value::SimpleString("1".to_string()))
                }
            }
        },
        Err(e) => {
            Err(Error::msg("Unable to parse key!"))
        }
    }
}

// fn handle_replconf() -> Value{

// }

// fn handle_psync(server_store: &mut Arc<std::sync::Mutex<ServerInfo>>) -> Value{
//     Value::BulkString(String::from_utf8(hex::decode("UkVESVMwMDEx+glyZWRpcy12ZXIFNy4yLjD6CnJlZGlzLWJpdHPAQPoFY3RpbWXCbQi8ZfoIdXNlZC1tZW3CsMQQAPoIYW9mLWJhc2XAAP/wbjv+wP9aog==").unwrap()).unwrap().to_string())
// }

async fn get_info(arg: String, server_store: &mut Arc<tokio::sync::Mutex<ServerInfo>>) -> Value{
    match arg.as_str(){
        "replication" => {
            let role = server_store.lock().await.role.clone();
            let replid = server_store.lock().await.master_replid.clone();
            let repl_offset = server_store.lock().await.master_repl_offset.clone();
            let info_str = format!(
                "role:{}\nmaster_replid:{}\nmaster_repl_offset:{}",
                role,
                replid,
                repl_offset
            );
            Value::BulkString(info_str)
        },
        _ => Value::BulkString("Variant Not Found".to_string())
    }
}

//extracting the command used after redis-cli, along with the args after the command[redis-cli <command> [..args]]
// returning (command, [..args])
async fn extract_command(value: Value) -> Result<(String, Vec<Value>)> {
    match value {
        Value::Array(a) => { //[command, ..arguments]
            Ok((
                unpack_bulk_str(a.first().unwrap().clone()).await?, //command 
                a.into_iter().skip(1).collect(), //[..arguments]
            ))
        },
        Value::BulkString(x) => {
            Ok((
                x,
                Vec::new(),
            ))
        }
        _ => Err(anyhow::anyhow!("Unexpected command format")),
    }
}
async fn unpack_bulk_str(value: Value) -> Result<String> {
    match value {
        Value::BulkString(s) => Ok(s),
        _ => Err(anyhow::anyhow!("Expected command to be a bulk string"))
    }
}