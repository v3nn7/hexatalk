//! Transport adapters: direct TCP and optional WebSocket relay.

mod tcp;

pub use tcp::{TcpEndpoint, dial_direct, local_addrs_for_port};

#[cfg(feature = "relay")]
mod relay;

#[cfg(feature = "relay")]
pub use relay::{RelayConnection, connect_via_relay};
