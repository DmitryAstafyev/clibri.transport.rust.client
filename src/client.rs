use super::{
    errors::Error,
    options::{ConnectionType, Options},
};
use async_trait::async_trait;
use clibri::{
    client,
    client::{Control as ClientControl, Event, Impl, Message},
    env,
    env::logs,
};
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use hyper::{Client as HttpClient, StatusCode, Uri};
use log::{debug, error, warn};
use std::net::SocketAddr;
use tokio::{
    join,
    net::TcpStream,
    select,
    sync::{
        mpsc::{channel, Receiver, Sender},
        oneshot,
    },
};
use tokio_tungstenite::{
    connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod channels {
    pub const EVENTS: usize = 100;
    pub const MESSAGES: usize = 100;
    pub const SHUTDOWN: usize = 1;
}

#[derive(Debug, Clone)]
pub struct Control {
    shutdown: CancellationToken,
    tx_sender: Sender<Message>,
    tx_shutdown: Sender<oneshot::Sender<()>>,
}

impl Control {
    pub fn get_shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }
}

#[async_trait]
impl client::Control<Error> for Control {
    async fn shutdown(&self) -> Result<(), Error> {
        let (tx_response, rx_response): (oneshot::Sender<()>, oneshot::Receiver<()>) =
            oneshot::channel();
        self.tx_shutdown
            .send(tx_response)
            .await
            .map_err(|e| Error::Channel(e.to_string()))?;
        let result = rx_response.await;
        self.shutdown.cancel();
        result.map_err(|e| Error::Channel(e.to_string()))
    }
    async fn send(&self, msg: Message) -> Result<(), Error> {
        self.tx_sender
            .send(msg)
            .await
            .map_err(|e| Error::Channel(e.to_string()))
    }
}
#[derive(Debug)]
pub struct Client {
    options: Options,
    uuid: Uuid,
    control: Control,
    rx_sender: Option<Receiver<Message>>,
    rx_shutdown: Option<Receiver<oneshot::Sender<()>>>,
    rx_events: Option<Receiver<Event<Error>>>,
    tx_events: Sender<Event<Error>>,
}

impl Client {
    pub fn new(options: Options) -> Self {
        let (tx_sender, rx_sender): (Sender<Message>, Receiver<Message>) =
            channel(channels::MESSAGES);
        let (tx_shutdown, rx_shutdown): (
            Sender<oneshot::Sender<()>>,
            Receiver<oneshot::Sender<()>>,
        ) = channel(channels::SHUTDOWN);
        let (tx_events, rx_events): (Sender<Event<Error>>, Receiver<Event<Error>>) =
            channel(channels::EVENTS);
        Self {
            options,
            tx_events,
            rx_sender: Some(rx_sender),
            rx_events: Some(rx_events),
            rx_shutdown: Some(rx_shutdown),
            uuid: Uuid::new_v4(),
            control: Control {
                tx_sender,
                tx_shutdown,
                shutdown: CancellationToken::new(),
            },
        }
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    async fn direct_connection(
        addr: SocketAddr,
        tx_events: Sender<Event<Error>>,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Error> {
        let addr_str = format!("ws://{}:{}", addr.ip(), addr.port());
        match connect_async(&addr_str).await {
            Ok((ws, _)) => {
                if let Err(err) = tx_events.send(Event::Connected(addr)).await {
                    error!(
                        target: logs::targets::CLIENT,
                        "fail to emit Event::Connected: {:?}", err
                    );
                }
                Ok(ws)
            }
            Err(err) => Err(Error::Connecting(format!(
                "Fail to connect to {}; error: {}",
                addr_str, err
            ))),
        }
    }

    async fn distributor_connection(
        addr: SocketAddr,
        tx_events: Sender<Event<Error>>,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Error> {
        let distributor_url = format!("http://{}:{}", addr.ip(), addr.port())
            .parse::<Uri>()
            .map_err(|e| Error::DistributorUrl(e.to_string()))?;
        debug!(
            target: logs::targets::CLIENT,
            "requesting port for websocket connection from {}", distributor_url
        );
        let http_client = HttpClient::new();
        let response = http_client
            .get(distributor_url)
            .await
            .map_err(|e| Error::HttpRequest(e.to_string()))?;
        if response.status() != StatusCode::OK {
            error!(
                target: logs::targets::CLIENT,
                "has been gotten status: {}",
                response.status()
            );
            return Err(Error::DistributorFail);
        }
        let buffer = hyper::body::to_bytes(response)
            .await
            .map_err(|e| Error::DistributorResponse(e.to_string()))?;
        let port: u16 = String::from(
            std::str::from_utf8(&buffer.to_vec())
                .map_err(|e| Error::DistributorResponse(e.to_string()))?,
        )
        .parse()
        .map_err(|_| Error::DistributorInvalidResponse)?;
        let addr_str = format!("ws://{}:{}", addr.ip(), port);
        debug!(
            target: logs::targets::CLIENT,
            "will try to connect to: {}", addr_str
        );
        match connect_async(&addr_str).await {
            Ok((ws, _)) => {
                let addr = format!("{}:{}", addr.ip(), port)
                    .parse::<SocketAddr>()
                    .map_err(|e| Error::SocketAddr(e.to_string()))?;
                if let Err(err) = tx_events.send(Event::Connected(addr)).await {
                    error!(
                        target: logs::targets::CLIENT,
                        "fail to emit Event::Connected: {:?}", err
                    );
                }
                Ok(ws)
            }
            Err(err) => Err(Error::Connecting(format!(
                "Fail to connect to {}; error: {}",
                addr_str, err
            ))),
        }
    }

    async fn reader_task(
        mut reader: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
        tx_events: Sender<Event<Error>>,
        cancel: CancellationToken,
    ) -> (
        SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
        Option<Error>,
    ) {
        debug!(target: logs::targets::CLIENT, "reader_task is started");
        let error = select! {
            res = async {
                while let Some(msg) = reader.next().await {
                    match msg {
                        Ok(msg) => match msg {
                            WsMessage::Binary(buffer) => tx_events
                                .send(Event::Message(Message::Binary(buffer)))
                                .await
                                .map_err(|e| Error::Channel(e.to_string()))?,
                            WsMessage::Text(txt) => tx_events
                                .send(Event::Message(Message::Text(txt)))
                                .await
                                .map_err(|e| Error::Channel(e.to_string()))?,
                            WsMessage::Ping(buffer) => tx_events
                                .send(Event::Message(Message::Ping(buffer)))
                                .await
                                .map_err(|e| Error::Channel(e.to_string()))?,
                            WsMessage::Pong(buffer) => tx_events
                                .send(Event::Message(Message::Pong(buffer)))
                                .await
                                .map_err(|e| Error::Channel(e.to_string()))?,
                            WsMessage::Close(frame) => {
                                if let Some(frame) = frame {
                                    debug!(
                                        target: logs::targets::CLIENT,
                                        "connection would be closed with {:?}", frame
                                    );
                                } else {
                                    debug!(
                                        target: logs::targets::CLIENT,
                                        "connection would be closed without CloseFrame"
                                    );
                                }
                            }
                        },
                        Err(err) => {
                            return Err(Error::Read(err.to_string()));
                        }
                    }
                }
                Ok(())
            } => {
                if let Err(error) = res {
                    Some(error)
                } else {
                    None
                }
            },
            _ = cancel.cancelled() => None
        };
        if let Some(err) = error.clone() {
            if let Err(err) = tx_events.send(Event::Error(err)).await {
                error!(
                    target: logs::targets::CLIENT,
                    "fail to send event Error; error: {:?}", err
                );
            }
        }
        debug!(target: logs::targets::CLIENT, "reader_task is finished");
        cancel.cancel();
        (reader, error)
    }

    async fn writer_task(
        mut writer: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>,
        tx_events: Sender<Event<Error>>,
        mut rx_sender: Receiver<Message>,
        cancel: CancellationToken,
    ) -> (
        SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>,
        Option<Error>,
    ) {
        let error = select! {
            res = async {
                while let Some(msg) = rx_sender.recv().await {
                    match msg {
                        Message::Binary(buffer) => writer
                            .send(WsMessage::Binary(buffer))
                            .await
                            .map_err(|e| Error::Write(e.to_string()))?,
                        Message::Text(txt) => writer
                            .send(WsMessage::Text(txt))
                            .await
                            .map_err(|e| Error::Write(e.to_string()))?,
                        Message::Ping(buffer) => writer
                            .send(WsMessage::Ping(buffer))
                            .await
                            .map_err(|e| Error::Write(e.to_string()))?,
                        Message::Pong(buffer) => writer
                            .send(WsMessage::Pong(buffer))
                            .await
                            .map_err(|e| Error::Write(e.to_string()))?,
                    }
                }
                Ok(())
            } => {
                if let Err(error) = res {
                    Some(error)
                } else {
                    None
                }
            },
            _ = cancel.cancelled() => None
        };
        if let Some(err) = error.clone() {
            if let Err(err) = tx_events.send(Event::Error(err)).await {
                error!(
                    target: logs::targets::CLIENT,
                    "fail to send event Error; error: {:?}", err
                );
            }
        }
        cancel.cancel();
        (writer, error)
    }

    fn reinit(&mut self) {
        let (tx_sender, rx_sender): (Sender<Message>, Receiver<Message>) =
            channel(channels::MESSAGES);
        let (tx_shutdown, rx_shutdown): (
            Sender<oneshot::Sender<()>>,
            Receiver<oneshot::Sender<()>>,
        ) = channel(channels::SHUTDOWN);
        let (tx_events, rx_events): (Sender<Event<Error>>, Receiver<Event<Error>>) =
            channel(channels::EVENTS);
        self.tx_events = tx_events;
        self.rx_events = Some(rx_events);
        self.rx_sender = Some(rx_sender);
        self.rx_shutdown = Some(rx_shutdown);
        self.control = Control {
            tx_sender,
            tx_shutdown,
            shutdown: CancellationToken::new(),
        };
        debug!(target: logs::targets::CLIENT, "client has been reinited");
    }
}

#[async_trait]
impl Impl<Error, Control> for Client {
    async fn connect(&mut self) -> Result<(), Error> {
        env::logs::init();
        debug!(target: logs::targets::CLIENT, "client is started");
        let socket = match self.options.connection {
            ConnectionType::Direct(addr) => {
                Self::direct_connection(addr, self.tx_events.clone()).await
            }
            ConnectionType::Distributor(addr) => {
                Self::distributor_connection(addr, self.tx_events.clone()).await
            }
        };
        let socket = match socket {
            Ok(socket) => socket,
            Err(err) => {
                self.reinit();
                warn!(
                    target: logs::targets::CLIENT,
                    "client is finished with error: {}", err
                );
                if let Err(err) = self.tx_events.send(Event::Error(err.clone())).await {
                    error!(
                        target: logs::targets::CLIENT,
                        "fail to send event Error; error: {:?}", err
                    );
                }
                return Err(err);
            }
        };
        let (writer, reader) = socket.split();
        let rx_sender = if let Some(rx_sender) = self.rx_sender.take() {
            rx_sender
        } else {
            return Err(Error::AlreadyInited);
        };
        let mut rx_shutdown = if let Some(rx_shutdown) = self.rx_shutdown.take() {
            rx_shutdown
        } else {
            return Err(Error::AlreadyInited);
        };
        let cancel = CancellationToken::new();
        let (((reader, reader_error), (writer, writer_error)), tx_shutdown_response) = join!(
            async {
                join!(
                    Self::reader_task(reader, self.tx_events.clone(), cancel.clone()),
                    Self::writer_task(writer, self.tx_events.clone(), rx_sender, cancel.clone()),
                )
            },
            async {
                select! {
                    res = async {
                        if let Some(tx_shutdown_response) = rx_shutdown.recv().await {
                            cancel.cancel();
                            Some(tx_shutdown_response)
                        } else {
                            None
                        }
                    } => res,
                    _ = cancel.cancelled() => None
                }
            }
        );
        if let Err(err) = self.tx_events.send(Event::Disconnected).await {
            warn!(
                target: logs::targets::CLIENT,
                "fail to send event Disconnected; error: {:?}", err
            );
        }
        let close_err = match writer.reunite(reader) {
            Ok(mut ws) => ws
                .close(None)
                .await
                .map_err(|e| Error::Closing(e.to_string())),
            Err(err) => Err(Error::Closing(err.to_string())),
        };
        self.reinit();
        if let Some(tx_shutdown_response) = tx_shutdown_response {
            if tx_shutdown_response.send(()).is_err() {
                error!(
                    target: logs::targets::CLIENT,
                    "fail to send shutdown response"
                );
            }
        }
        debug!(target: logs::targets::CLIENT, "client is finished");
        if let Some(error) = reader_error {
            Err(error)
        } else if let Some(error) = writer_error {
            Err(error)
        } else {
            close_err
        }
    }

    fn observer(&mut self) -> Result<Receiver<Event<Error>>, Error> {
        if let Some(rx_events) = self.rx_events.take() {
            Ok(rx_events)
        } else {
            Err(Error::ObserverAlreadyTaken)
        }
    }

    fn control(&self) -> Control {
        self.control.clone()
    }
}
