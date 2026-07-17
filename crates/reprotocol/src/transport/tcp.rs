//! Direct TCP dial / accept helpers.

use crate::error::{Error, Result};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Bound TCP listener plus advertised addresses for invites.
pub struct TcpEndpoint {
    /// Tokio TCP listener.
    pub listener: TcpListener,
    /// Local bind address.
    pub local_addr: SocketAddr,
    /// Addresses to put in the invite (LAN-oriented best effort).
    pub advertise_addrs: Vec<String>,
}

impl TcpEndpoint {
    /// Bind to `bind_addr` (e.g. `0.0.0.0:0`) and collect advertise addresses.
    pub async fn bind(bind_addr: impl tokio::net::ToSocketAddrs) -> Result<Self> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let advertise_addrs = local_addrs_for_port(local_addr.port());
        Ok(Self {
            listener,
            local_addr,
            advertise_addrs,
        })
    }

    /// Accept one inbound TCP connection with optional timeout.
    pub async fn accept(&self, accept_timeout: Option<Duration>) -> Result<TcpStream> {
        let fut = async {
            let (stream, peer) = self.listener.accept().await?;
            tracing::debug!(%peer, "accepted direct TCP peer");
            Ok::<_, Error>(stream)
        };
        match accept_timeout {
            Some(t) => timeout(t, fut)
                .await
                .map_err(|_| Error::Timeout("accept timed out".into()))?,
            None => fut.await,
        }
    }
}

/// Dial addresses in parallel; first successful connection is returned.
pub async fn dial_direct(addrs: &[String], dial_timeout: Duration) -> Result<TcpStream> {
    if addrs.is_empty() {
        return Err(Error::ConnectFailed("no direct addresses in invite".into()));
    }

    let (tx, mut rx) = mpsc::channel::<Result<TcpStream>>(addrs.len());

    for a in addrs {
        let addr = a.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = match timeout(dial_timeout, TcpStream::connect(&addr)).await {
                Ok(Ok(s)) => {
                    tracing::debug!(%addr, "direct TCP connected");
                    Ok(s)
                }
                Ok(Err(e)) => Err(Error::Io(e)),
                Err(_) => Err(Error::Timeout(format!("dial {addr} timed out"))),
            };
            let _ = tx.send(result).await;
        });
    }
    drop(tx);

    let mut last_err = Error::ConnectFailed("all direct dials failed".into());
    let mut remaining = addrs.len();
    while remaining > 0 {
        match rx.recv().await {
            Some(Ok(stream)) => return Ok(stream),
            Some(Err(e)) => {
                last_err = e;
                remaining -= 1;
            }
            None => break,
        }
    }
    Err(last_err)
}

/// Collect host-visible addresses for a listening port (best-effort).
pub fn local_addrs_for_port(port: u16) -> Vec<String> {
    let mut out = Vec::new();

    // Always include loopback for same-machine demos.
    out.push(format!("127.0.0.1:{port}"));

    // Enumerate primary outbound IPv4 via UDP "connect" trick (no packets needed).
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = socket.local_addr() {
                let ip = local.ip();
                if !ip.is_loopback() {
                    let s = match ip {
                        std::net::IpAddr::V4(v4) => format!("{v4}:{port}"),
                        std::net::IpAddr::V6(v6) => format!("[{v6}]:{port}"),
                    };
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }

    if let Ok(hosts) = local_ip_guess(port) {
        for h in hosts {
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }

    out
}

fn local_ip_guess(port: u16) -> std::io::Result<Vec<String>> {
    use std::net::ToSocketAddrs;
    let mut v = Vec::new();
    if let Ok(hostname) = hostname_string() {
        if let Ok(iter) = (hostname.as_str(), 0u16).to_socket_addrs() {
            for sa in iter {
                let ip = sa.ip();
                if ip.is_loopback() || ip.is_unspecified() {
                    continue;
                }
                let s = match ip {
                    std::net::IpAddr::V4(v4) => format!("{v4}:{port}"),
                    std::net::IpAddr::V6(v6) => {
                        if is_unicast_link_local_v6(&v6) {
                            continue;
                        }
                        format!("[{v6}]:{port}")
                    }
                };
                if !v.contains(&s) {
                    v.push(s);
                }
            }
        }
    }
    Ok(v)
}

fn hostname_string() -> std::io::Result<String> {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        return Ok(h);
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        return Ok(h);
    }
    Ok("localhost".into())
}

fn is_unicast_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    (segments[0] & 0xffc0) == 0xfe80
}
