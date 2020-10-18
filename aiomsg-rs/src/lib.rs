mod aiomsg_types;
mod broker;
mod msgproto;
mod test_utils;
mod utils;

use aiomsg_types::{DeliveryGuarantee, Identity, Payload, SendMode};
use async_std::future;
use async_std::io::BufReader;
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::{io, task};
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::sink::SinkExt;
// This makes "incoming.next()" available, although curiously,
// the code runs without this import being required!
use futures::stream::StreamExt;
use log::{debug, error, info};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use utils::hexify;

type Sender<T> = mpsc::UnboundedSender<T>;
type Receiver<T> = mpsc::UnboundedReceiver<T>;
// pub type Result<T> =
//     std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Error, Debug)]
pub enum AiomsgError {
    #[error("Failed to connect")]
    Unknown,
    #[error("async_std error")]
    Io {
        #[from]
        source: io::Error,
    },
    #[error("async_std error")]
    Async {
        #[from]
        source: mpsc::SendError,
    },
}

pub type Result<T> = std::result::Result<T, AiomsgError>;

pub struct Socket {
    pub send_mode: SendMode,
    pub delivery_guarantee: DeliveryGuarantee,
    pub identity: Identity,
    pub reconnection_delay: f64,
    // Internal stuff from here
    send_into_broker: Mutex<Sender<broker::Event>>,
    // Extras
    waiting_for_acks: BTreeMap<Identity, TcpStream>,
    // Tasks (joinhandles)
    sender_task: Mutex<Option<task::JoinHandle<()>>>,
    send_from_conn_loop: Mutex<Sender<Payload>>,
    recv_receiver: Mutex<Receiver<Payload>>,
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socket")
            .field("send_mode", &self.send_mode)
            .field("delivery_guarantee", &self.delivery_guarantee)
            .field("id", &utils::hexify(&self.identity, 4))
            .finish()
    }
}

// impl Default for Socket {
//     fn default() -> Arc<Self> {
//         Socket::new()
//     }
// }

impl Socket {
    pub fn new() -> Arc<Socket> {
        let (send_into_broker, broker_receiver) = mpsc::unbounded();
        let send_mode = SendMode::RoundRobin;
        let _broker_handle =
            task::spawn(broker::broker_loop(broker_receiver, send_mode));

        let (send_from_conn_loop, recv_receiver) = mpsc::unbounded();

        let sock = Socket {
            send_mode,
            delivery_guarantee: DeliveryGuarantee::AtMostOnce,
            identity: *uuid::Uuid::new_v4().as_bytes(),
            reconnection_delay: 0.1,
            // These fields private
            // send_queue_receiver: sq_receiver,
            // send_queue_sender: sq_sender,
            waiting_for_acks: BTreeMap::new(),
            sender_task: Mutex::new(None),
            send_into_broker: Mutex::new(send_into_broker),
            send_from_conn_loop: Mutex::new(send_from_conn_loop),
            recv_receiver: Mutex::new(recv_receiver),
        };

        Arc::new(sock)
    }
    pub fn set_send_mode(&mut self, value: SendMode) -> &mut Socket {
        self.send_mode = value;
        self
    }
    pub fn set_delivery_guarantee(
        &mut self,
        value: DeliveryGuarantee,
    ) -> &mut Socket {
        self.delivery_guarantee = value;
        self
    }
    pub fn set_identity(&mut self, value: &[u8]) -> &mut Socket {
        self.identity = <[u8; 16]>::try_from(value).expect("Wrong size");
        self
    }
    pub fn set_reconnection_delay(&mut self, value: f64) -> &mut Socket {
        self.reconnection_delay = value;
        self
    }

    pub async fn test_call(self: Arc<Socket>) {
        for _i in 0..3 {
            task::sleep(Duration::from_millis(50)).await;
            info!("{:?}", &self);
        }
        info!("leaving test-call");
    }

    pub async fn bind(
        self: &Arc<Socket>,
        hostname: &str,
        port: u32,
        ssl_context: Option<u32>,
    ) -> io::Result<()> {
        info!(
            "Binding socket {:?} to {}:{}",
            &utils::hexify(&self.identity, 4),
            &hostname,
            &port,
        );

        let addr = format!("{}:{}", hostname, port);
        let clone = self.clone();
        task::spawn(clone.accept_loop(addr));
        Ok(())
    }

    pub async fn connect(
        self: &Arc<Socket>,
        hostname: &str,
        port: u32,
        ssl_context: Option<u32>,
    ) -> Result<()> {
        // TODO: this should actually spawn a long-running task that will
        //  keep connecting every time the connection drops.
        info!(
            "Connecting socket {:?} to {}:{}",
            &utils::hexify(&self.identity, 4),
            &hostname,
            &port,
        );

        let addr = format!("{}:{}", hostname, port);
        let clone = self.clone();

        let stream = match future::timeout(
            Duration::from_secs(5),
            TcpStream::connect(addr),
        )
        .await
        {
            Ok(s) => s?,
            Err(_) => panic!("Timed out"),
        };

        // let mut identity: Identity = [0; 16];
        // // TODO: assert length of first_msg is 16 exactly.
        // identity.clone_from_slice(&first_msg[..]);

        let _handle = task::spawn(clone.connection_loop(stream));
        // TODO: get rid of this, just a pause to make sure the connection
        // is registered before "sends" can happen.
        async_std::task::sleep(Duration::from_secs(1)).await;
        Ok(())
    }

    pub async fn accept_loop(
        self: Arc<Socket>,
        addr: String,
    ) -> io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            debug!("Accepting from: {}", stream.peer_addr()?);
            let clone = self.clone();
            let _handle = task::spawn(clone.connection_loop(stream));
        }
        Ok(())
    }

    async fn connection_loop(
        self: Arc<Socket>,
        stream: TcpStream,
    ) -> Result<()> {
        // 1. Send my own identity
        info!("Send my id: {}", hexify(&self.identity, 4));
        msgproto::send_msg(&stream, &self.identity).await?;
        // 2. Get the other side's identity
        let prospective_identity = msgproto::read_msg(&stream).await;
        if prospective_identity.is_none() {
            error!("Didn't get identity, bailing.");
            return Ok(());
        };
        let other_identity_vec = prospective_identity.unwrap();
        let mut other_identity: Identity = [0; 16];
        other_identity.clone_from_slice(&other_identity_vec[..]);
        info!("Got other identity: {}", hexify(&other_identity, 4));

        // 3. Add this peer to my mapping of active connections
        let stream = Arc::new(stream);
        {
            let mut broker = self.send_into_broker.lock().await;
            broker
                .send(broker::Event::NewPeer {
                    name: other_identity,
                    stream: Arc::clone(&stream),
                })
                .await?;
        }

        // 4. Receiving messages from this connection
        while let Some(bytes) = msgproto::read_msg(&stream).await {
            let s = String::from_utf8_lossy(&bytes).to_string();
            // received_messages.push(s);
            info!("received: {}", &s);
            {
                let mut sender = self.send_from_conn_loop.lock().await;
                info!("sending data into channel: {}", hexify(&bytes, 4));
                sender.send(bytes).await?;
            }
        }
        Ok(())
    }

    async fn recv(self: &Arc<Socket>) -> io::Result<Option<Vec<u8>>> {
        // 1. Wait for messages coming from a Receiver side of a channel
        let mut receiver = self.recv_receiver.lock().await;
        match receiver.next().await {
            Some(msg) => {
                info!("recv: received {}", hexify(&msg, 4));
                Ok(Some(msg))
            }
            None => {
                info!("recv: no data!");
                Ok(None)
            }
        }
    }

    async fn send(self: &Arc<Socket>, msg: &[u8]) -> Result<()> {
        // 1. Put message onto a channel for sending
        let mut broker = self.send_into_broker.lock().await;
        broker
            .send(broker::Event::Message {
                from: self.identity,
                to: vec![],
                msg: msg.to_vec(),
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;

    #[test]
    fn crate_it_works() {
        std::env::set_var("RUST_LOG", "info");
        pretty_env_logger::init();
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn crate_socket_lyfe() {
        let sock = Socket::new();
        info!("{:?}", &sock);
    }

    #[async_std::test]
    async fn crate_socket_run_async_method() {
        let sock = Socket::new();
        sock.clone().test_call().await;
    }

    #[async_std::test]
    async fn crate_sock_broker() -> io::Result<()> {
        info!("Running sock_broker");
        let _addr = test_utils::get_addr();

        async fn client() -> Result<()> {
            async_std::task::sleep(Duration::from_secs(3)).await;
            let sock = Socket::new();
            sock.connect("127.0.0.1", 27005, None).await?;
            sock.send(b"blah1").await?;
            sock.send(b"blah2").await?;
            sock.send(b"blah3").await?;
            Ok(())
        }

        async fn server() -> io::Result<Vec<String>> {
            let mut result = vec![];
            let sock = Socket::new();
            sock.bind("127.0.0.1", 27005, None).await?;
            let rng: std::ops::Range<u32> = 0..3;
            for _i in rng {
                match sock.recv().await? {
                    Some(msg) => {
                        info!("Received: {:?}", &msg);
                        result.push(
                            String::from_utf8_lossy(&msg[..]).to_string(),
                        );
                    }
                    None => break,
                }
            }
            Ok(result)
        }

        task::spawn(client());
        let received = server().await?;
        info!("This was received: {:?}", &received);
        assert_eq!(received, vec!["blah1", "blah2", "blah3"]);
        Ok(())
    }
}
