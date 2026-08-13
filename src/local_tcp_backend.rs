use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub(crate) fn ensure_reachable(port: u16) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));

    TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map(|_| ())
        .map_err(|_| format!("error: local TCP backend 127.0.0.1:{port} is not reachable"))
}
