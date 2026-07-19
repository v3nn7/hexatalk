//! Live E2E over a real relay. Runs only when `PEERSEAL_LIVE_RELAY=1`.
//!
//! ```text
//! set PEERSEAL_RELAY=relay-production-eb30.up.railway.app
//! set PEERSEAL_LIVE_RELAY=1
//! cargo test --test integration_relay_live -- --nocapture
//! ```

#![cfg(feature = "relay")]

use peerseal::{Invite, Node, NodeConfig, TransportKind};
use std::time::Duration;

fn live_enabled() -> bool {
    matches!(
        std::env::var("PEERSEAL_LIVE_RELAY").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[tokio::test]
async fn e2e_over_configured_relay() {
    if !live_enabled() {
        eprintln!("skip: set PEERSEAL_LIVE_RELAY=1 and PEERSEAL_RELAY=… to run");
        return;
    }

    let relay = std::env::var("PEERSEAL_RELAY")
        .unwrap_or_else(|_| "relay-production-eb30.up.railway.app".into());

    let cfg = NodeConfig {
        force_relay: true,
        direct_first: false,
        relay_wait_timeout: Duration::from_secs(45),
        accept_timeout: Some(Duration::from_secs(45)),
        ..Default::default()
    };

    let host = Node::bind("127.0.0.1:0")
        .await
        .expect("bind")
        .with_relay(&relay)
        .expect("relay")
        .with_config(cfg.clone());

    let invite = host
        .create_invite(Duration::from_secs(120))
        .expect("invite");
    let qr = invite.to_qr_string().unwrap();

    let inv_h = invite.clone();
    let host_task = tokio::spawn(async move {
        let mut s = host.accept_peer(&inv_h).await.expect("host accept");
        assert_eq!(s.transport, TransportKind::Relay);
        assert_eq!(s.recv().await.unwrap(), b"live-ping");
        s.send(b"live-pong").await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let guest = Node::guest()
        .with_relay(&relay)
        .expect("relay")
        .with_config(cfg);
    let mut g = guest
        .join_invite(Invite::from_qr_string(&qr).unwrap())
        .await
        .expect("guest join");
    assert_eq!(g.transport, TransportKind::Relay);
    g.send(b"live-ping").await.unwrap();
    assert_eq!(g.recv().await.unwrap(), b"live-pong");

    host_task.await.unwrap();
}
