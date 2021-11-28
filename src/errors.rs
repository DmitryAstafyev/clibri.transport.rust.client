use clibri::client;
use thiserror::Error as ThisError;

#[derive(ThisError, Debug, Clone)]
pub enum Error {
    #[error("connecting error: `{0}`")]
    Connecting(String),
    #[error("closing error: `{0}`")]
    Closing(String),
    #[error("fail to make http request: `{0}`")]
    HttpRequest(String),
    #[error("fail to get connection port from distributor")]
    DistributorFail,
    #[error("invalid response from distributor")]
    DistributorInvalidResponse,
    #[error("fail to get or parse distrubutor response: `{0}`")]
    DistributorResponse(String),
    #[error("fail on distrubutor url: `{0}`")]
    DistributorUrl(String),
    #[error("error on channel: `{0}`")]
    Channel(String),
    #[error("read socket error: `{0}`")]
    Read(String),
    #[error("write socket error: `{0}`")]
    Write(String),
    #[error("fail parse socket address: `{0}`")]
    SocketAddr(String),
    #[error("events ovserver already taken")]
    ObserverAlreadyTaken,
    #[error("client is already inited")]
    AlreadyInited,
    #[error("nobody listens client events")]
    NoMaster,
}

impl client::Error for Error {}
