//! Interactive host/guest pairing demo with identity + SAS.
//!
//! ```text
//! set PEERSEAL_RELAY=relay-production-eb30.up.railway.app
//! cargo run --example pair_demo -- host --relay-only
//! cargo run --example pair_demo -- guest --relay-only
//! ```

use peerseal::{
    AppMessage, Identity, Invite, Node, NodeConfig, TofuStore, TransportKind, env_relay_url,
    normalize_relay_url,
};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

struct Args {
    role: String,
    relay_only: bool,
    relay: Option<String>,
}

fn parse_args() -> Args {
    let mut role = "help".to_string();
    let mut relay_only = false;
    let mut relay = None;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "host" | "guest" | "help" => role = a,
            "--relay-only" | "-r" => relay_only = true,
            "--relay" => relay = args.next(),
            other if other.starts_with("--relay=") => {
                relay = Some(other.trim_start_matches("--relay=").to_string());
            }
            _ => {}
        }
    }
    Args {
        role,
        relay_only,
        relay,
    }
}

fn identity_path(role: &str) -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("peerseal-{role}-identity.key"));
    p
}

fn tofu_path(role: &str) -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("peerseal-{role}-tofu.txt"));
    p
}

#[tokio::main]
async fn main() -> peerseal::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = parse_args();
    match args.role.as_str() {
        "host" => run_host(args).await,
        "guest" => run_guest(args).await,
        _ => {
            eprintln!(
                "peerseal pair_demo\n\n\
                 Usage:\n\
                   pair_demo host [--relay-only] [--relay HOST]\n\
                   pair_demo guest [--relay-only] [--relay HOST]\n\n\
                 Commands in chat:\n\
                   /file <path>     send file\n\
                   /photo <path>    send file bytes as photo\n\
                   /rekey           rotate session keys\n\
                   /sas             print SAS again\n\
                   /quit\n\n\
                 Env: PEERSEAL_RELAY=relay-production-eb30.up.railway.app"
            );
            Ok(())
        }
    }
}

fn resolve_relay(cli: &Option<String>) -> peerseal::Result<Option<String>> {
    if let Some(r) = cli {
        return Ok(Some(normalize_relay_url(r)?));
    }
    Ok(env_relay_url())
}

fn node_config(relay_only: bool) -> NodeConfig {
    NodeConfig {
        force_relay: relay_only,
        direct_first: !relay_only,
        ..Default::default()
    }
}

async fn run_host(args: Args) -> peerseal::Result<()> {
    let id = Identity::load_or_create(identity_path("host"))?;
    println!("local fingerprint: {}", id.short_fingerprint());

    let mut node = Node::bind("0.0.0.0:0")
        .await?
        .with_identity(id)
        .with_config(node_config(args.relay_only));
    if let Some(relay) = resolve_relay(&args.relay)? {
        node = node.with_relay(relay)?;
    }
    if args.relay_only {
        node = node.force_relay();
    }

    let invite = node.create_invite(Duration::from_secs(180))?;
    let qr = invite.to_qr_string()?;
    println!("══════════════════════════════════════════════");
    println!("  room:  {}", invite.room_id);
    println!("  addrs: {:?}", invite.addrs);
    println!(
        "  relay: {}",
        invite.relay_url.as_deref().unwrap_or("(none)")
    );
    println!("  mode:  {}", if args.relay_only { "RELAY-ONLY" } else { "direct-first" });
    println!("──────────────────────────────────────────────");
    println!("  invite:\n\n  {qr}\n");
    println!("  short: {}", invite.to_short_code());
    println!("══════════════════════════════════════════════");

    let mut session = node.accept_peer(&invite).await?;
    post_connect(&mut session, "host").await?;
    chat_loop(&mut session).await
}

async fn run_guest(args: Args) -> peerseal::Result<()> {
    let id = Identity::load_or_create(identity_path("guest"))?;
    println!("local fingerprint: {}", id.short_fingerprint());

    print!("paste invite: ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).map_err(peerseal::Error::Io)?;
    let line = line.trim();
    if line.is_empty() {
        return Err(peerseal::Error::InvalidInvite("empty".into()));
    }

    let relay = resolve_relay(&args.relay)?;
    let invite = if line.starts_with("ps1:") || line.starts_with("peerseal:1:") {
        Invite::from_qr_string(line)?
    } else if line.contains('/') {
        Invite::from_short_code(line, Duration::from_secs(180), relay.clone(), vec![])?
    } else {
        Invite::parse(line)?
    };

    let mut node = Node::guest()
        .with_identity(id)
        .with_config(node_config(args.relay_only));
    if let Some(r) = relay {
        node = node.with_relay(r)?;
    }
    if args.relay_only {
        node = node.force_relay();
    }

    let mut session = node.join_invite(invite).await?;
    post_connect(&mut session, "guest").await?;
    chat_loop(&mut session).await
}

async fn post_connect(session: &mut peerseal::Session, role: &str) -> peerseal::Result<()> {
    println!(
        "session via {:?} pattern={:?}",
        session.transport, session.info.pattern
    );
    if session.transport == TransportKind::Relay {
        println!("(relay sees only ciphertext)");
    }
    println!("SAS emoji:  {}", session.info.sas_emojis());
    println!("SAS nums:   {}", session.info.sas_numbers());
    println!("SAS hex:    {}", session.info.sas_hex());
    if let Some(fp) = session.info.remote_short_fingerprint() {
        println!("remote fp:  {fp}");
        let mut tofu = TofuStore::load(tofu_path(role)).unwrap_or_default();
        if let Some(check) = session.tofu_check(&mut tofu)? {
            println!("TOFU:       {check:?}");
        }
    }
    println!("compare SAS with the other peer out-of-band, then chat.");
    println!("commands: /file /photo /rekey /sas /quit");
    Ok(())
}

async fn chat_loop(session: &mut peerseal::Session) -> peerseal::Result<()> {
    let (tx_line, mut rx_line) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines().flatten() {
            if tx_line.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            line = rx_line.recv() => {
                match line {
                    Some(l) => {
                        let l = l.trim_end().to_string();
                        if l == "/quit" || l == "/exit" {
                            session.close();
                            println!("bye");
                            break;
                        }
                        if l == "/sas" {
                            println!("SAS: {}", session.info.sas_emojis());
                            continue;
                        }
                        if l == "/rekey" {
                            session.request_rekey().await?;
                            println!("(rekeyed)");
                            continue;
                        }
                        if let Some(path) = l.strip_prefix("/file ") {
                            let path = path.trim();
                            match session.send_file(path, "application/octet-stream").await {
                                Ok(id) => println!("sent file id={id}"),
                                Err(e) => eprintln!("file send error: {e}"),
                            }
                            continue;
                        }
                        if let Some(path) = l.strip_prefix("/photo ") {
                            let path = path.trim();
                            match tokio::fs::read(path).await {
                                Ok(bytes) => {
                                    match session.send_photo(&bytes, "image/jpeg", 0, 0).await {
                                        Ok(id) => println!("sent photo stream={id} ({} bytes)", bytes.len()),
                                        Err(e) => eprintln!("photo error: {e}"),
                                    }
                                }
                                Err(e) => eprintln!("read error: {e}"),
                            }
                            continue;
                        }
                        if l.is_empty() {
                            continue;
                        }
                        session.send_text(l).await?;
                    }
                    None => break,
                }
            }
            msg = session.recv_app() => {
                match msg {
                    Ok(AppMessage::Text(t)) => println!("peer> {t}"),
                    Ok(AppMessage::Binary(b)) => println!("peer> [binary {} bytes]", b.len()),
                    Ok(AppMessage::FileMeta { id, name, size, content_type }) => {
                        println!("peer> file meta id={id} name={name} size={size} type={content_type}");
                        let dest = env::temp_dir().join(format!("peerseal-recv-{name}"));
                        match session.recv_file(
                            AppMessage::FileMeta { id, name: name.clone(), size, content_type },
                            &dest,
                        ).await {
                            Ok(hash) => println!("saved {:?} sha256={:02x}…", dest, hash[0]),
                            Err(e) => eprintln!("recv file error: {e}"),
                        }
                    }
                    Ok(AppMessage::MediaStart { stream_id, kind, content_type, width, height }) => {
                        println!("peer> media start stream={stream_id} kind={kind:?} {content_type} {width}x{height}");
                        // Drain frames until MediaEnd
                        let mut total = 0usize;
                        loop {
                            match session.recv_app().await {
                                Ok(AppMessage::MediaFrame { data, .. }) => total += data.len(),
                                Ok(AppMessage::MediaEnd { .. }) => {
                                    println!("peer> media end ({total} bytes assembled)");
                                    break;
                                }
                                Ok(other) => {
                                    eprintln!("unexpected during media: {other:?}");
                                    break;
                                }
                                Err(e) => {
                                    eprintln!("media recv error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    Ok(AppMessage::SasAck(ok)) => println!("peer> SAS ack: {ok}"),
                    Ok(AppMessage::Pong(_)) => {}
                    Ok(other) => println!("peer> {other:?}"),
                    Err(e) => {
                        eprintln!("recv error / peer gone: {e}");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}
