//! HD video-call media demo over E2E relay (1080p metadata + large frames).
//!
//! Streams synthetic HD RGB frames (and tiny JPEG keyframes) through Noise with
//! automatic fragmentation up to 25 MiB logical messages.
//!
//! ```text
//! set PEERSEAL_RELAY=relay-production-eb30.up.railway.app
//! cargo run --example vc_demo --release
//!
//! # lighter 720p / fewer frames:
//! cargo run --example vc_demo --release -- --720p --frames 30
//! ```
//!
//! Plug real capture by replacing `generate_hd_test_pattern_rgb` / JPEG encode
//! with ffmpeg, hardware encoder, or camera SDK — wire format is already HD VC.

use peerseal::{
    HdProfile, Identity, Invite, Node, NodeConfig, TransportKind, VcCall, VcConfig, VcControlKind,
    VcEvent, VideoCodec, VideoJitterBuffer, generate_hd_test_pattern_rgb, minimal_jpeg_bytes,
    normalize_relay_url, video_frame_from_payload,
};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> peerseal::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let use_720 = args.iter().any(|a| a == "--720p");
    let frames: u32 = args
        .windows(2)
        .find(|w| w[0] == "--frames")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(if use_720 { 60 } else { 20 });
    // Full 1080p RGB is ~6MB/frame — heavy for debug builds / free relay.
    // Default: JPEG keyframes + smaller RGB subsample for throughput proof.
    let full_rgb = args.iter().any(|a| a == "--full-rgb");

    let relay = normalize_relay_url(
        &std::env::var("PEERSEAL_RELAY")
            .unwrap_or_else(|_| "relay-production-eb30.up.railway.app".into()),
    )?;

    let cfg_media = if use_720 {
        VcConfig::hd_720p30()
    } else {
        VcConfig::hd_1080p30()
    };
    println!(
        "VC profile {}x{} @ {}fps codec={} full_rgb={full_rgb} frames={frames}",
        cfg_media.width,
        cfg_media.height,
        cfg_media.fps,
        cfg_media.video_codec.name()
    );
    println!(
        "logical max frame = {} MiB",
        peerseal::HARD_MAX_FRAME / (1024 * 1024)
    );

    let node_cfg = NodeConfig {
        force_relay: true,
        direct_first: false,
        relay_wait_timeout: Duration::from_secs(60),
        session: peerseal::SessionConfig {
            io_timeout: Some(Duration::from_secs(300)),
            ..Default::default()
        },
        ..Default::default()
    };

    let host = Node::bind("127.0.0.1:0")
        .await?
        .with_identity(Identity::generate()?)
        .with_relay(relay.clone())?
        .with_config(node_cfg.clone());
    let invite = host.create_invite(Duration::from_secs(180))?;
    let qr = invite.to_qr_string()?;

    let host_media = cfg_media.clone();
    let host_task = tokio::spawn(async move {
        let mut s = host.accept_peer(&invite).await?;
        assert_eq!(s.transport, TransportKind::Relay);
        println!("host SAS {}", s.info.sas_emojis());

        let mut call = VcCall::new(host_media);
        let mut jitter = VideoJitterBuffer::default();
        let mut got_video = 0u32;
        let mut bytes_in = 0u64;
        let t0 = Instant::now();

        // Wait for offer
        loop {
            match s.vc_recv_event(&mut call).await? {
                VcEvent::Offer(offer) => {
                    println!(
                        "host got offer {}x{} {}",
                        offer.width, offer.height, offer.fps
                    );
                    s.vc_send_answer(&mut call, &offer).await?;
                    break;
                }
                VcEvent::Other(_) => {}
                other => println!("host early: {other:?}"),
            }
        }

        while got_video < frames {
            match s.vc_recv_event(&mut call).await? {
                VcEvent::Video(frame) => {
                    bytes_in += frame.data.len() as u64;
                    if let Some(f) = jitter.push(frame) {
                        got_video += 1;
                        if got_video % 5 == 0 || got_video == 1 {
                            println!(
                                "host video seq={} {}x{} key={} bytes={} codec={:?}",
                                f.seq,
                                f.width,
                                f.height,
                                f.keyframe,
                                f.data.len(),
                                f.codec
                            );
                        }
                    }
                }
                VcEvent::Audio(a) => {
                    println!("host audio seq={} bytes={}", a.seq, a.data.len());
                }
                VcEvent::Control {
                    kind: VcControlKind::Bye,
                    ..
                } => break,
                VcEvent::Control { kind, value } => {
                    println!("host control {kind:?} value={value}");
                }
                other => println!("host: {other:?}"),
            }
        }

        let elapsed = t0.elapsed().as_secs_f64().max(0.001);
        println!(
            "host done: frames={got_video} bytes={bytes_in} ({:.2} MiB) avg {:.2} Mbps dropped={}",
            bytes_in as f64 / (1024.0 * 1024.0),
            (bytes_in as f64 * 8.0) / elapsed / 1_000_000.0,
            jitter.dropped
        );
        s.send_text("vc-ok").await?;
        Ok::<_, peerseal::Error>(got_video)
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let guest = Node::guest()
        .with_identity(Identity::generate()?)
        .with_relay(relay)?
        .with_config(node_cfg);
    let mut g = guest.join_invite(Invite::from_qr_string(&qr)?).await?;
    println!("guest SAS {}", g.info.sas_emojis());

    let mut call = VcCall::new(cfg_media.clone());
    g.vc_send_offer(&call).await?;

    // Wait answer
    loop {
        match g.vc_recv_event(&mut call).await? {
            VcEvent::Answer(a) => {
                println!("guest negotiated {}x{}@{}", a.width, a.height, a.fps);
                break;
            }
            other => println!("guest wait answer: {other:?}"),
        }
    }

    let w = cfg_media.width;
    let h = cfg_media.height;
    let jpeg = minimal_jpeg_bytes();
    let t_send = Instant::now();
    let mut sent_bytes = 0u64;

    for i in 0..frames {
        let keyframe = i % 15 == 0;
        let data = if full_rgb {
            // True HD raw frames (1080p ≈ 6.2 MiB) — proves 25 MiB path + throughput.
            generate_hd_test_pattern_rgb(w, h, i)
        } else if keyframe {
            // Realistic-ish: JPEG access unit with HD metadata
            jpeg.clone()
        } else {
            // Delta-ish small payload with HD dimensions in header
            let mut v = vec![0u8; 64 * 1024];
            v[0] = (i & 0xff) as u8;
            v
        };

        let codec = if full_rgb {
            VideoCodec::RawRgb24
        } else {
            VideoCodec::Jpeg
        };
        let frame = video_frame_from_payload(w, h, codec, keyframe, 0, 0, data);
        sent_bytes += frame.data.len() as u64;
        g.vc_send_video(&mut call, frame).await?;

        // Fake Opus-ish audio every frame
        g.vc_send_audio(
            &mut call,
            peerseal::AudioFrame {
                pts_ms: 0,
                seq: 0,
                codec: peerseal::AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 1,
                data: vec![0u8; 80], // silence packet placeholder
            },
        )
        .await?;

        if !full_rgb {
            tokio::time::sleep(call.frame_interval() / 3).await;
        }
    }

    g.vc_bye().await?;
    let send_elapsed = t_send.elapsed().as_secs_f64().max(0.001);
    println!(
        "guest sent {frames} frames, {:.2} MiB in {:.2}s ({:.2} Mbps)",
        sent_bytes as f64 / (1024.0 * 1024.0),
        send_elapsed,
        (sent_bytes as f64 * 8.0) / send_elapsed / 1_000_000.0
    );

    match g.recv_app().await? {
        peerseal::AppMessage::Text(t) => println!("guest got: {t}"),
        other => println!("guest final: {other:?}"),
    }

    let n = host_task.await.expect("join")?;
    println!(
        "VC DEMO OK — host received {n} video frames (profile {:?})",
        HdProfile::P1080
    );
    Ok(())
}
