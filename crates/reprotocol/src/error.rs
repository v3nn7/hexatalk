//! Error types for peerseal.

use thiserror::Error;

/// Library-wide error type.
#[derive(Debug, Error)]
pub enum Error {
    /// Invite payload is malformed or uses an unsupported version.
    #[error("invalid invite: {0}")]
    InvalidInvite(String),

    /// Invite TTL has expired.
    #[error("invite expired at unix {expires_at}")]
    InviteExpired {
        /// Unix timestamp when the invite expired.
        expires_at: u64,
    },

    /// Room id or token does not meet relay/protocol constraints.
    #[error("invalid credentials: {0}")]
    InvalidCredentials(String),

    /// I/O failure on a transport.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Cryptographic or Noise handshake failure.
    #[error("crypto error: {0}")]
    Crypto(String),

    /// Frame too large or framing protocol violation.
    #[error("framing error: {0}")]
    Framing(String),

    /// Session is closed or not ready.
    #[error("session error: {0}")]
    Session(String),

    /// Connection / dial timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// No transport path succeeded (direct and optional relay).
    #[error("connection failed: {0}")]
    ConnectFailed(String),

    /// Relay protocol or WebSocket error.
    #[error("relay error: {0}")]
    Relay(String),

    /// Application protocol (typed messages / transfer) error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Identity / TOFU / verification failure.
    #[error("identity error: {0}")]
    Identity(String),

    /// Feature not enabled in this build.
    #[error("feature not enabled: {0}")]
    FeatureDisabled(&'static str),
}

/// Convenient result alias.
pub type Result<T> = std::result::Result<T, Error>;
