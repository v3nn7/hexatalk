//! End-to-end direct TCP pairing test (two tasks, one process).

use peerseal::{Invite, Node, TransportKind};
use std::time::Duration;

#[tokio::test]
async fn host_guest_direct_chat() {
    let host = Node::bind("127.0.0.1:0")
        .await
        .expect("bind")
        .with_config(peerseal::NodeConfig {
            dial_timeout: Duration::from_secs(2),
            accept_timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        });

    let invite = host
        .create_invite(Duration::from_secs(120))
        .expect("invite");
    // Force loopback-only for CI determinism
    let mut invite = invite;
    let port = host.local_addr().unwrap().port();
    invite.addrs = vec![format!("127.0.0.1:{port}")];
    invite.relay_url = None;

    let qr = invite.to_qr_string().unwrap();
    let invite_guest = Invite::from_qr_string(&qr).unwrap();

    let host_task = tokio::spawn(async move {
        let mut session = host.accept_peer(&invite).await.expect("accept");
        assert_eq!(session.transport, TransportKind::DirectTcp);
        let msg = session.recv().await.expect("recv");
        assert_eq!(msg, b"hello-from-guest");
        session.send(b"hello-from-host").await.expect("send");
    });

    // Tiny delay so accept is armed
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut guest = Node::join(invite_guest).await.expect("join");
    assert_eq!(guest.transport, TransportKind::DirectTcp);
    guest.send(b"hello-from-guest").await.expect("g-send");
    let reply = guest.recv().await.expect("g-recv");
    assert_eq!(reply, b"hello-from-host");

    host_task.await.expect("host task");
}

#[tokio::test]
async fn expired_invite_rejected_on_join() {
    let mut invite = Invite::create(peerseal::InviteOptions {
        ttl: Some(Duration::from_secs(60)),
        room_id: Some("expiredroom1".into()),
        token: Some("expiredtoken1".into()),
        addrs: vec!["127.0.0.1:1".into()],
        ..Default::default()
    })
    .unwrap();
    invite.expires_at = 1;

    match Node::join(invite).await {
        Err(peerseal::Error::InviteExpired { .. }) => {}
        Err(other) => panic!("expected InviteExpired, got {other}"),
        Ok(_) => panic!("expected expired invite to fail"),
    }
}
