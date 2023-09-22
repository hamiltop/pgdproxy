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

pub struct Listener;

impl Listener {
    /// Starts our listener. This will fire on Config.ch once we're ready to accept connections
    pub async fn start(config: Config) -> Result<(), Box<dyn std::error::Error>> {
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
                    task::spawn(async move {
                        let target = TcpStream::connect(target_address).await.unwrap();

                        match forwarder::Forwarder::start(socket, target, debug_binding).await {
                            Ok(_) => {}
                            Err(e) => println!("Error: {}", e),
                        };
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
