//! Non-interactive live smoke: two peers over `PEERSEAL_RELAY` (relay-only).
//!
//! ```text
//! set PEERSEAL_RELAY=relay-production-eb30.up.railway.app
//! cargo run --example relay_smoke
//! ```

use peerseal::{Invite, Node, NodeConfig, TransportKind, normalize_relay_url};
use std::time::Duration;

#[tokio::main]
async fn main() -> peerseal::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let relay_raw = std::env::var("PEERSEAL_RELAY")
        .unwrap_or_else(|_| "relay-production-eb30.up.railway.app".into());
    let relay = normalize_relay_url(&relay_raw)?;
    println!("relay = {relay}");

    let cfg = NodeConfig {
        dial_timeout: Duration::from_millis(200),
        accept_timeout: Some(Duration::from_secs(45)),
        relay_wait_timeout: Duration::from_secs(45),
        direct_first: false,
        force_relay: true,
        ..Default::default()
    };

    let host = Node::bind("127.0.0.1:0")
        .await?
        .with_relay(relay.clone())?
        .with_config(cfg.clone());

    let invite = host.create_invite(Duration::from_secs(120))?;
    assert!(
        invite.addrs.is_empty(),
        "force_relay invites must not advertise direct addrs"
    );
    let qr = invite.to_qr_string()?;
    println!("room={} invite_len={}", invite.room_id, qr.len());

    let inv_host = invite.clone();
    let host_task = tokio::spawn(async move {
        let mut s = host.accept_peer(&inv_host).await?;
        println!("host transport={:?}", s.transport);
        let msg = s.recv().await?;
        println!("host got: {}", String::from_utf8_lossy(&msg));
        s.send(b"pong-from-host").await?;
        Ok::<_, peerseal::Error>(s.transport)
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let guest_invite = Invite::from_qr_string(&qr)?;
    let guest = Node::guest().with_relay(relay)?.with_config(cfg);
    let mut g = guest.join_invite(guest_invite).await?;
    println!("guest transport={:?}", g.transport);
    assert_eq!(g.transport, TransportKind::Relay);
    g.send(b"ping-from-guest").await?;
    let reply = g.recv().await?;
    println!("guest got: {}", String::from_utf8_lossy(&reply));
    assert_eq!(&reply, b"pong-from-host");

    let ht = host_task.await.expect("host join")?;
    assert_eq!(ht, TransportKind::Relay);
    println!("SMOKE OK — E2E chat over Railway relay");
    Ok(())
}
