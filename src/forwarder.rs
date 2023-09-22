use std::io::Error;

use strum::Display;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::Encoder;

use crate::{
    listener::PortMapper,
    pg_codec::{
        ForwardingBackendCodec, ForwardingClientCodec, FrameInfo, SslOrStartup, StartupRequest,
    },
};
use futures::{SinkExt, StreamExt};
use tokio_util::codec::{Decoder, Framed};

pub struct ForwarderState {
    client: Framed<TcpStream, ForwardingClientCodec>,
    target: Framed<TcpStream, ForwardingBackendCodec>,
    debug_listener: TcpListener,
}

#[derive(Display)]
pub enum Forwarder {
    Start {
        client: TcpStream,
        target: TcpStream,
        port_mapper: Option<PortMapper>,
    },
    Authenticated {
        client: Framed<TcpStream, ForwardingClientCodec>,
        target: Framed<TcpStream, ForwardingBackendCodec>,
        port_mapper: Option<PortMapper>,
    },
    Listening {
        state: ForwarderState,
    },
    ForwardingClient {
        state: ForwarderState,
    },
    ForwardingServer {
        state: ForwarderState,
    },
    DebugMode {
        state: ForwarderState,
        debug_client: Framed<TcpStream, ForwardingClientCodec>,
    },
    DebugForwardingClient {
        state: ForwarderState,
        debug_client: Framed<TcpStream, ForwardingClientCodec>,
    },
    DebugForwardingServer {
        state: ForwarderState,
        debug_client: Framed<TcpStream, ForwardingClientCodec>,
    },
}

impl Forwarder {
    pub async fn start(
        client: TcpStream,
        target: TcpStream,
        port_mapper: Option<PortMapper>,
    ) -> Result<Self, Error> {
        let mut state = Self::Start {
            client,
            target,
            port_mapper,
        };
        loop {
            state = state.run().await?;
        }
    }

    async fn run(self) -> Result<Self, Error> {
        let new_state = match self {
            Forwarder::Start {
                mut client,
                mut target,
                port_mapper,
            } => {
                Self::startup(&mut client, &mut target).await?;
                let mut client = ForwardingClientCodec.framed(client);
                let mut target = ForwardingBackendCodec.framed(target);
                // Do authentication
                Self::authenticate(&mut client, &mut target).await?;
                Self::Authenticated {
                    client,
                    target,
                    port_mapper,
                }
            }
            Forwarder::Authenticated {
                client,
                target,
                port_mapper,
            } => {
                // TODO: If a Debug Port is specified, we will only be able to have one connection.
                // Find a way to make this discoverable. Tricky since the forwarder spawns a new task
                // for each connection.
                let debug_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

                let debug_port = debug_listener.local_addr().unwrap().port();
                // We can use the target port as the client port because that's what postgres will report with
                // `select inet_client_port()`
                let client_port = target.get_ref().local_addr().unwrap().port();
                if let Some(pm) = port_mapper.as_ref() {
                    pm.add(client_port, debug_port).await;
                }
                println!(
                    "Listening for debug on port {}",
                    debug_listener.local_addr().unwrap().port()
                );
                Self::Listening {
                    state: ForwarderState {
                        client,
                        target,
                        debug_listener,
                    },
                }
            }
            Forwarder::Listening { mut state } => {
                tokio::select! {
                    message = state.client.next() => {
                        match message {
                            Some(Ok(data)) => {
                                let (done, _) = Self::forward(&mut state.client, &mut state.target, Some(data)).await?;
                                if done {
                                    Self::Listening { state }
                                } else {
                                    Self::ForwardingClient { state }
                                }
                            }
                            Some(Err(e)) => {
                                Err(e)?
                            }
                            None => {
                                println!("Client disconnected");
                                Err(Error::new(std::io::ErrorKind::Other, "Client disconnected"))?
                            }
                        }
                    },
                    message = state.target.next() => {
                        match message {
                            Some(Ok(data)) => {
                                let (done, _) = Self::forward(&mut state.target, &mut state.client, Some(data)).await?;
                                if done {
                                    Self::Listening { state }
                                } else {
                                    Self::ForwardingServer { state }
                                }
                            }
                            Some(Err(e)) => {
                                Err(e)?
                            }
                            None => {
                                println!("Target disconnected");
                                Err(Error::new(std::io::ErrorKind::Other, "Target disconnected"))?
                            }
                        }
                    }
                    data = state.debug_listener.accept() => {
                        match data {
                            Ok((mut debug_client, _)) => {
                                Self::fake_startup(&mut debug_client).await?;
                                let mut debug_client = ForwardingClientCodec.framed(debug_client);
                                Self::fake_authenticate(&mut debug_client).await?;
                                Self::DebugMode {
                                    state,
                                    debug_client,
                                }
                            }
                            Err(e) => {
                                println!("Error accepting debug client: {}", e);
                                Err(e)?
                            }
                        }
                    }
                }
            }
            Forwarder::ForwardingClient { mut state } => {
                let (done, _) = Self::forward(&mut state.client, &mut state.target, None).await?;
                if done {
                    Self::ForwardingServer { state }
                } else {
                    Self::ForwardingClient { state }
                }
            }
            Forwarder::ForwardingServer { mut state } => {
                let (done, _) = Self::forward(&mut state.target, &mut state.client, None).await?;
                if done {
                    Self::Listening { state }
                } else {
                    Self::ForwardingServer { state }
                }
            }
            Forwarder::DebugMode {
                mut state,
                mut debug_client,
            } => {
                tokio::select! {
                    message = debug_client.next() => {
                        match message {
                            Some(Ok(data)) => {
                                if data[0] == 88 {
                                    Self::Listening { state }
                                } else {
                                    let (done, _) = Self::forward(&mut debug_client, &mut state.target, Some(data)).await?;
                                    if done {
                                        Self::DebugForwardingServer { state, debug_client }
                                    } else {
                                        Self::DebugForwardingClient { state, debug_client }
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                println!("Error reading from debug client: {:?}", e);
                                debug_client.close().await.unwrap();
                                Self::Listening { state }
                            }
                            None => {
                                println!("Debug client disconnected");
                                debug_client.close().await.unwrap();
                                Self::Listening { state }
                            }
                        }
                    }
                    message = state.target.next() => {
                        match message {
                            Some(Ok(data)) => {
                                let (done, _) = Self::forward(&mut state.target, &mut debug_client, Some(data)).await?;
                                if done {
                                    Self::Listening { state }
                                } else {
                                    Self::ForwardingServer { state }
                                }
                            }
                            Some(Err(e)) => {
                                Err(e)?
                            }
                            None => {
                                println!("Target disconnected");
                                Err(Error::new(std::io::ErrorKind::Other, "Target disconnected"))?
                            }
                        }
                    }
                }
            }
            Forwarder::DebugForwardingClient {
                mut state,
                mut debug_client,
            } => match Self::forward(&mut debug_client, &mut state.target, None).await {
                Ok((done, _)) => {
                    if done {
                        Self::DebugForwardingServer {
                            state,
                            debug_client,
                        }
                    } else {
                        Self::DebugForwardingClient {
                            state,
                            debug_client,
                        }
                    }
                }
                Err(e) => {
                    println!("Error reading from debug client: {:?}", e);
                    Self::DebugMode {
                        state,
                        debug_client,
                    }
                }
            },
            Forwarder::DebugForwardingServer {
                mut state,
                mut debug_client,
            } => match Self::forward(&mut state.target, &mut debug_client, None).await {
                Ok((done, _)) => {
                    if done {
                        Self::DebugMode {
                            state,
                            debug_client,
                        }
                    } else {
                        Self::DebugForwardingServer {
                            state,
                            debug_client,
                        }
                    }
                }
                Err(e) => {
                    println!("Error reading from debug client: {:?}", e);
                    Self::DebugMode {
                        state,
                        debug_client,
                    }
                }
            },
        };
        Ok(new_state)
    }

    async fn forward<T, U, V>(
        client: &mut Framed<TcpStream, U>,
        target: &mut Framed<TcpStream, V>,
        initial_message: Option<T>,
    ) -> Result<(bool, Option<u8>), Error>
    where
        T: FrameInfo + std::fmt::Debug,
        U: Decoder<Item = T>,
        U::Error: std::fmt::Debug,
        V: Encoder<T>,
        V::Error: std::fmt::Debug,
    {
        Self::do_forward(client, target, initial_message, true).await
    }

    async fn do_forward<T, U, V>(
        client: &mut Framed<TcpStream, U>,
        target: &mut Framed<TcpStream, V>,
        initial_message: Option<T>,
        until_complete: bool,
    ) -> Result<(bool, Option<u8>), Error>
    where
        T: FrameInfo + std::fmt::Debug,
        U: Decoder<Item = T>,
        U::Error: std::fmt::Debug,
        V: Encoder<T>,
        V::Error: std::fmt::Debug,
    {
        if let Some(data) = initial_message {
            let done = data.done();
            let command = data.command();
            match target.send(data).await {
                Ok(_) => {
                    if done || !until_complete {
                        return Ok((done, command));
                    }
                }
                Err(e) => {
                    println!("Error sending to target: {:?}", e);
                    Err(Error::new(std::io::ErrorKind::Other, "Framing Error"))?;
                }
            };
        }
        loop {
            match client.next().await {
                Some(Ok(data)) => {
                    let done = data.done();
                    let command = data.command();
                    match target.send(data).await {
                        Ok(_) => {
                            if done || !until_complete {
                                return Ok((done, command));
                            }
                        }
                        Err(e) => {
                            println!("Error sending to target: {:?}", e);
                            Err(Error::new(std::io::ErrorKind::Other, "Framing Error"))?;
                        }
                    };
                }
                Some(Err(e)) => {
                    println!("Error reading from client: {:?}", e);
                    //return Err(e);
                    return Err(Error::new(std::io::ErrorKind::Other, "Framing Error"));
                }
                None => {
                    println!("Client disconnected");
                    return Err(Error::new(std::io::ErrorKind::Other, "Client disconnected"));
                }
            }
        }
    }

    #[async_recursion::async_recursion]
    async fn startup(client: &mut TcpStream, target: &mut TcpStream) -> Result<(), Error> {
        let mut client = Framed::new(client, StartupRequest);
        match client.next().await {
            Some(data) => {
                let data = data?;
                match data {
                    SslOrStartup::SslRequest(payload) => {
                        target.write_all(&payload).await?;
                        let Ok(78) = target.read_u8().await else {
                            return Err(Error::new(
                                std::io::ErrorKind::Other,
                                "Expected 'N' from target",
                            ));
                        };
                        client.get_mut().write_u8(78).await?;
                        // Should be a StartupRequest now
                        Self::startup(client.get_mut(), target).await
                    }
                    SslOrStartup::StartupRequest(payload) => {
                        //
                        target.write(&payload).await?;
                        Ok(())
                    }
                }
            }
            None => {
                println!("Client disconnected");
                return Err(Error::new(std::io::ErrorKind::Other, "Client disconnected"));
            }
        }
    }

    #[async_recursion::async_recursion]
    async fn fake_startup(client: &mut TcpStream) -> Result<(), Error> {
        // TODO: return all the same initial data that the main server does.
        let mut client = Framed::new(client, StartupRequest);
        match client.next().await {
            Some(data) => {
                let data = data?;
                match data {
                    SslOrStartup::SslRequest(_) => {
                        client.get_mut().write_u8(78).await?;
                        // Should be a StartupRequest now
                        Self::fake_startup(client.get_mut()).await
                    }
                    SslOrStartup::StartupRequest(_) => Ok(()),
                }
            }
            None => {
                println!("Client disconnected");
                return Err(Error::new(std::io::ErrorKind::Other, "Client disconnected"));
            }
        }
    }

    // This is like a state machine itself.
    async fn authenticate(
        client: &mut Framed<TcpStream, ForwardingClientCodec>,
        target: &mut Framed<TcpStream, ForwardingBackendCodec>,
    ) -> Result<(), Error> {
        // Server sends AuthRequest
        let (_, tag) = Self::do_forward(target, client, None, false).await?;

        if tag.is_none() || tag.is_some_and(|t| t != 82) {
            // Client sends password
            Self::do_forward(client, target, None, false).await?;
        }

        // Server sends ReadyForQuery
        Self::forward(target, client, None).await?;
        Ok(())
    }

    async fn fake_authenticate(
        client: &mut Framed<TcpStream, ForwardingClientCodec>,
    ) -> Result<(), Error> {
        // Server sends AuthRequest
        client
            .get_mut()
            .write(&mut [82, 0, 0, 0, 8, 0, 0, 0, 0])
            .await?;

        // Server sends ReadyForQuery
        client.get_mut().write(&[90, 0, 0, 0, 5, 73]).await?;
        Ok(())
    }
}

// enum Authentication {
//     Start,
//     VersionAndParameters,
//     AuthRequest,
//     Password,
//     ReadyForQuery,
// }
/*
impl Forwarder {
    async fn run(&mut self, auth: bool) {
        if auth {
            self.authentication().await.unwrap();
            println!("Real Auth Complete");
        } else {
            self.fake_auth().await;
            println!("Fake Auth Complete");
        }
        loop {
            {
                // We have to parse this out manually because we don't want to acquire
                // the lock until we have a message we want to send.
                let client_msg = self.client.read_u8().await.unwrap();
                dbg!(client_msg);
                let len = self.client.read_i32().await.unwrap();

                let mut lock = self.target.lock().await;

                lock.write_u8(client_msg).await.unwrap();
                lock.write_i32(len).await.unwrap();
                for _ in 0..(len - 4) {
                    let d = self.client.read_u8().await.unwrap();
                    lock.write_u8(d).await.unwrap();
                }

                // some commands are multi message and we don't want to
                // call forward_until_ready until after we've sent all the
                // client messages
                let additional_msg_count = match client_msg {
                    80 => 2, // Parse
                    66 => 3, // Describe
                    _ => 0,
                };
                for _ in 0..additional_msg_count {
                    let client_msg = Self::forward_message(&mut self.client, &mut lock)
                        .await
                        .unwrap();
                    dbg!(client_msg);
                }
                Self::forward_until_ready(&mut lock, &mut self.client)
                    .await
                    .unwrap();
            }
        }
    }

    // Fake auth for secondary connection. We can just send it AuthOK and ReadyForQuery
    async fn fake_auth(&mut self) {
        let len = self.client.read_i32().await.unwrap();
        let _major_version = self.client.read_i16().await.unwrap();
        let _minor_version = self.client.read_i16().await.unwrap();

        let mut str = vec![];
        for _ in 0..(len - 8) {
            let d = self.client.read_u8().await.unwrap();
            if d == 0 {
                // dbg!(String::from_utf8_lossy(&str));
                str.truncate(0);
            } else {
                str.push(d);
            }
        }

        // AuthOK
        self.client.write_u8(82).await.unwrap();
        self.client.write_i32(8).await.unwrap();
        self.client.write_i32(0).await.unwrap();
        // ReadyForQuery
        self.client.write_u8(90).await.unwrap();
        self.client.write_i32(5).await.unwrap();
        self.client.write_u8(73).await.unwrap();
    }

    // For our primary connection we want to proxy auth
    async fn authentication(&mut self) -> Result<(), tokio::io::Error> {
        let mut lock = self.target.lock().await;

        // versions and parameters
        let len = self.client.read_i32().await?;
        lock.write_i32(len).await?;

        let major_version = self.client.read_i16().await?;
        lock.write_i16(major_version).await?;

        let minor_version = self.client.read_i16().await?;
        lock.write_i16(minor_version).await?;

        let mut str = vec![];
        for _ in 0..(len - 8) {
            let d = self.client.read_u8().await?;
            if d == 0 {
                // dbg!(String::from_utf8_lossy(&str));
                str.truncate(0);
            } else {
                str.push(d);
            }
            lock.write_u8(d).await?;
        }

        // auth
        let server_msg = Self::forward_message(&mut lock, &mut self.client).await?;
        dbg!(server_msg);
        // password
        let client_msg = Self::forward_message(&mut self.client, &mut lock).await?;
        dbg!(client_msg);
        // other stuff
        Self::forward_until_ready(&mut lock, &mut self.client).await?;
        Ok(())
    }

    // Often we'll just want to forward the server response until we get
    // ReadyForQuery
    async fn forward_until_ready(
        server: &mut TcpStream,
        client: &mut TcpStream,
    ) -> Result<(), tokio::io::Error> {
        loop {
            let server_msg = Self::forward_message(server, client).await?;
            dbg!(server_msg);

            if server_msg == 90 {
                break;
            }
        }
        Ok(())
    }

    async fn forward_message(
        src: &mut TcpStream,
        dest: &mut TcpStream,
    ) -> Result<u8, tokio::io::Error> {
        let command = src.read_u8().await?;
        dest.write_u8(command).await.unwrap();

        let len = src.read_i32().await.unwrap();
        dest.write_i32(len).await.unwrap();

        // len includes itself, so subtract it out
        for _ in 0..(len - 4) {
            let d = src.read_u8().await.unwrap();
            dest.write_u8(d).await.unwrap();
        }
        Ok(command)
    }
}
*/

/*
pub struct Connection {}

impl Connection {
    pub async fn run(client: TcpStream, target: TcpStream) {
        let target = Arc::new(Mutex::new(target));
        let target_for_primary = target.clone();
        task::spawn(async move {
            Forwarder {
                client,
                target: target_for_primary,
            }
            .run(true)
            .await
        });

        let listener = TcpListener::bind("127.0.0.1:10000").await.unwrap();

        let port = listener.local_addr().unwrap().port();

        println!("Listening for secondary on port {port}");
        loop {
            // Only support a single secondary connection, so we can just block
            match listener.accept().await {
                Ok((socket, _)) => {
                    println!("Secondary connected on port {port}");
                    Forwarder {
                        client: socket,
                        target: target.clone(),
                    }
                    .run(false)
                    .await;
                }
                Err(e) => println!("Encountered Error: {e}"),
            };
        }
    }
}
*/
