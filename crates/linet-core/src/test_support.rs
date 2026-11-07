//! Изолированные mTLS/QUIC-сеансы для регрессионных тестов, без D-Bus и рабочего демона.

use std::sync::Arc;

use crate::files::Transfers;
use crate::{DeviceKind, Envelope, Hello, Identity, Inbound, Message, Session, frame, transport};
use tokio::sync::mpsc;

#[derive(Debug)]
struct AllowClient;

impl crate::tls::ClientPolicy for AllowClient {
    fn allow(&self, _: &crate::Fingerprint) -> bool {
        true
    }
}

pub(crate) struct TestLink {
    pub sender: Arc<Session>,
    pub receiver: Arc<Session>,
    pub sender_events: Option<mpsc::Receiver<Inbound>>,
    pub receiver_events: Option<mpsc::Receiver<Inbound>>,
    endpoints: [quinn::Endpoint; 2],
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl TestLink {
    pub async fn new() -> Self {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let server = transport::server_endpoint(&server_id, Arc::new(AllowClient), 0).unwrap();
        let client = transport::client_endpoint().unwrap();
        let addr = ([127, 0, 0, 1], server.local_addr().unwrap().port()).into();
        let config = transport::client_config(&client_id, server_id.fingerprint()).unwrap();
        let (client_conn, server_conn) = tokio::join!(
            async {
                client
                    .connect_with(config, addr, crate::SERVER_NAME)
                    .unwrap()
                    .await
                    .unwrap()
            },
            async { server.accept().await.unwrap().await.unwrap() },
        );
        let (mut client_tx, client_rx) = client_conn.open_bi().await.unwrap();
        frame::write(
            &mut client_tx,
            &Envelope {
                id: 0,
                reply_to: None,
                body: Message::Ping,
            },
        )
        .await
        .unwrap();
        let (server_tx, mut server_rx) = server_conn.accept_bi().await.unwrap();
        frame::read(&mut server_rx).await.unwrap().unwrap();
        let hello = |name: &str| Hello {
            name: name.into(),
            kind: DeviceKind::Phone,
            proto: crate::PROTO_VERSION,
            caps: vec![],
        };
        let (sender, sender_events) = Session::spawn(
            client_conn,
            client_tx,
            client_rx,
            server_id.fingerprint(),
            hello("receiver"),
            1,
        );
        let (receiver, receiver_events) = Session::spawn(
            server_conn,
            server_tx,
            server_rx,
            client_id.fingerprint(),
            hello("sender"),
            2,
        );
        Self {
            sender,
            receiver,
            sender_events: Some(sender_events),
            receiver_events: Some(receiver_events),
            endpoints: [server, client],
            tasks: vec![],
        }
    }

    pub fn pump(&mut self, outgoing: Arc<Transfers>, incoming: Arc<Transfers>) {
        for (session, mut events, transfers) in [
            (
                self.sender.clone(),
                self.sender_events.take().unwrap(),
                outgoing,
            ),
            (
                self.receiver.clone(),
                self.receiver_events.take().unwrap(),
                incoming,
            ),
        ] {
            self.tasks.push(tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    transfers.handle(&session, event);
                }
            }));
        }
    }
}

impl Drop for TestLink {
    fn drop(&mut self) {
        for endpoint in &self.endpoints {
            endpoint.close(0u32.into(), b"test finished");
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}
