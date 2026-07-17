//! Automated file + photo + rekey demo over relay (or direct if same process TCP).
//!
//! ```text
//! set PEERSEAL_RELAY=relay-production-eb30.up.railway.app
//! cargo run --example transfer_demo
//! ```

use peerseal::{AppMessage, Identity, Invite, Node, NodeConfig, TransportKind, normalize_relay_url};
use std::time::Duration;

#[tokio::main]
async fn main() -> peerseal::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let relay = normalize_relay_url(
        &std::env::var("PEERSEAL_RELAY")
            .unwrap_or_else(|_| "relay-production-eb30.up.railway.app".into()),
    )?;

    let cfg = NodeConfig {
        force_relay: true,
        direct_first: false,
        relay_wait_timeout: Duration::from_secs(45),
        ..Default::default()
    };

    let host_id = Identity::generate()?;
    let guest_id = Identity::generate()?;

    let host = Node::bind("127.0.0.1:0")
        .await?
        .with_identity(host_id)
        .with_relay(relay.clone())?
        .with_config(cfg.clone());

    let invite = host.create_invite(Duration::from_secs(120))?;
    let qr = invite.to_qr_string()?;
    println!("room={} pattern will be XXpsk3", invite.room_id);

    let inv_h = invite.clone();
    let host_task = tokio::spawn(async move {
        let mut s = host.accept_peer(&inv_h).await?;
        assert_eq!(s.transport, TransportKind::Relay);
        println!("host SAS {}", s.info.sas_emojis());
        // expect file
        let meta = s.recv_app().await?;
        let dest = std::env::temp_dir().join("peerseal-transfer-demo.bin");
        let hash = s.recv_file(meta, &dest).await?;
        println!("host got file hash[0]={:02x} path={:?}", hash[0], dest);
        // expect photo stream
        match s.recv_app().await? {
            AppMessage::MediaStart { stream_id, .. } => {
                let mut n = 0usize;
                loop {
                    match s.recv_app().await? {
                        AppMessage::MediaFrame { data, .. } => n += data.len(),
                        AppMessage::MediaEnd { stream_id: sid } if sid == stream_id => break,
                        _ => {}
                    }
                }
                println!("host got photo {n} bytes");
            }
            other => return Err(peerseal::Error::Protocol(format!("want MediaStart, got {other:?}"))),
        }
        s.send_text("transfer-ok").await?;
        Ok::<_, peerseal::Error>(s.info.sas_emojis())
    });

    tokio::time::sleep(Duration::from_millis(400)).await;

    let guest = Node::guest()
        .with_identity(guest_id)
        .with_relay(relay)?
        .with_config(cfg);
    let mut g = guest
        .join_invite(Invite::from_qr_string(&qr)?)
        .await?;
    println!("guest SAS {}", g.info.sas_emojis());
    assert_eq!(g.info.pattern, peerseal::NoisePattern::XxPsk3);

    let payload = b"hello-from-transfer-demo-over-e2ee".repeat(100);
    g.send_bytes_as_file("demo.bin", &payload, "application/octet-stream")
        .await?;
    g.send_photo(b"\xff\xd8\xfffake-jpeg-bytes", "image/jpeg", 64, 64)
        .await?;
    let ack = g.recv_app().await?;
    assert_eq!(ack, AppMessage::Text("transfer-ok".into()));

    let host_sas = host_task.await.expect("join")?;
    assert_eq!(host_sas, g.info.sas_emojis());
    println!("TRANSFER DEMO OK");
    Ok(())
}
