use std::{collections::HashMap, sync::Arc};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{oneshot, Mutex},
    task,
};

use crate::forwarder;

pub struct Config {
    pub binding: String,
    pub target_address: String,
    pub ch: Option<oneshot::Sender<()>>,
    pub debug_binding: Option<String>,
}

pub struct Listener {
    // Track active connections so we can shut them down
    active_connections: Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>>,
}

impl Listener {
    pub fn new() -> Self {
        Listener {
            active_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    
    // Shut down all active connections
    pub async fn shutdown(&self) {
        let mut connections = self.active_connections.lock().await;
        for (_id, sender) in connections.drain() {
            let _ = sender.send(());
        }
    }
    /// Starts our listener. This will fire on Config.ch once we're ready to accept connections
    pub async fn start(self, config: Config) -> Result<(), Box<dyn std::error::Error>> {
        let target_address = config.target_address.clone();
        let listener = TcpListener::bind(&config.binding).await?;
        if let Some(ch) = config.ch {
            ch.send(()).or(Err("Oneshot Failed"))?;
        }
        loop {
            match listener.accept().await {
                Ok((socket, _)) => {
                    let target_address = target_address.clone();
                    let debug_binding = config.debug_binding.clone();
                    // Create a shutdown channel for this connection
                    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
                    
                    // Spawn the task with the shutdown channel
                    let handle = task::spawn(async move {
                        let target = TcpStream::connect(target_address).await.unwrap();

                        match forwarder::Forwarder::start(
                            socket, 
                            target, 
                            debug_binding, 
                            Some(shutdown_receiver)
                        ).await {
                            Ok(_) => {}
                            Err(e) => println!("Error: {}", e),
                        };
                    });
                    
                    // Store the shutdown sender in our active connections map
                    let handle_id = handle.id();
                    let active_connections = self.active_connections.clone();
                    {
                        let mut connections = active_connections.lock().await;
                        connections.insert(handle_id.as_u64(), shutdown_sender.clone());
                    }
                    
                    // First make sure the shutdown is sent when the task completes
                    tokio::spawn(async move {
                        // Send shutdown when either the task completes or the maximum time is reached
                        tokio::select! {
                            _ = handle => {
                                // Task completed normally
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {
                                // Safety timeout to avoid resource leaks
                            }
                        }
                        
                        // Always send shutdown signal when we exit
                        let _ = shutdown_sender.send(());
                        
                        // Remove from active connections
                        let mut connections = active_connections.lock().await;
                        connections.remove(&handle_id.as_u64());
                    });
                }
                Err(e) => {
                    println!("Error accepting connection: {}", e);
                }
            }
        }
    }
}

/// This can be used to get the debug_port associated with a given connection
/// NOTE: this may not work if you have any other proxies in between
#[derive(Clone)]
pub struct PortMapper {
    inner: Arc<Mutex<HashMap<u16, u16>>>,
}

impl PortMapper {
    pub fn new() -> Self {
        PortMapper {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn add(&self, client: u16, debug: u16) {
        self.inner.lock().await.insert(client, debug);
    }

    pub async fn lookup_debug_port(&self, client: u16) -> Option<u16> {
        self.inner.lock().await.get(&client).map(|x| *x)
    }

    /// When all else fails, you can use this to get all the debug ports
    /// and enumerate them to find the one you want
    pub async fn get_all_debug_ports(&self) -> Vec<u16> {
        self.inner.lock().await.values().map(|x| *x).collect()
    }
}
