//! Small shared helpers.

use crate::error::{Error, Result};

/// Normalize a user-supplied relay base URL to `ws://` or `wss://`.
///
/// Accepts:
/// - `wss://host` / `ws://host` (unchanged, trailing `/` stripped)
/// - `https://host` → `wss://host`
/// - `http://host` → `ws://host`
/// - bare `host` or `host:port` → `wss://host` (production default)
pub fn normalize_relay_url(input: &str) -> Result<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err(Error::InvalidInvite("relay URL is empty".into()));
    }

    let out = if let Some(rest) = s.strip_prefix("wss://") {
        format!("wss://{}", rest.trim_end_matches('/'))
    } else if let Some(rest) = s.strip_prefix("ws://") {
        format!("ws://{}", rest.trim_end_matches('/'))
    } else if let Some(rest) = s.strip_prefix("https://") {
        format!("wss://{}", rest.trim_end_matches('/'))
    } else if let Some(rest) = s.strip_prefix("http://") {
        format!("ws://{}", rest.trim_end_matches('/'))
    } else if s.contains("://") {
        return Err(Error::InvalidInvite(format!(
            "unsupported relay URL scheme in `{s}` (use ws/wss/http/https or bare host)"
        )));
    } else {
        // bare hostname
        format!("wss://{}", s.trim_end_matches('/'))
    };

    if out == "wss://" || out == "ws://" {
        return Err(Error::InvalidInvite("relay URL missing host".into()));
    }
    Ok(out)
}

/// Read `PEERSEAL_RELAY` and normalize, if set.
pub fn env_relay_url() -> Option<String> {
    std::env::var("PEERSEAL_RELAY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|s| normalize_relay_url(&s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants() {
        assert_eq!(
            normalize_relay_url("relay-production-eb30.up.railway.app").unwrap(),
            "wss://relay-production-eb30.up.railway.app"
        );
        assert_eq!(
            normalize_relay_url("https://relay.example/").unwrap(),
            "wss://relay.example"
        );
        assert_eq!(
            normalize_relay_url("http://localhost:8080").unwrap(),
            "ws://localhost:8080"
        );
        assert_eq!(
            normalize_relay_url("wss://r.example/").unwrap(),
            "wss://r.example"
        );
    }
}
