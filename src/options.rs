use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Distributor {
    pub addr: SocketAddr,
    pub attempts: usize,
    pub delay_between_attempts: u64,
}

impl Distributor {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            attempts: 3,
            delay_between_attempts: 3000,
        }
    }
}
#[derive(Clone, Debug)]
pub enum ConnectionType {
    Direct(SocketAddr),
    Distributor(Distributor),
}

#[derive(Clone, Debug)]
pub struct Options {
    pub connection: ConnectionType,
}
