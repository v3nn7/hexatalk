# peerseal

Secure **direct-first** P2P for two devices: QR invite, **Noise E2EE**, optional WebSocket relay, identity/SAS, **HD VC** (up to **25 MiB** logical frames).

## Features (v0.3)

| Area | What you get |
|------|----------------|
| Pairing | `ps1:…` QR payload, short `room/token`, TTL |
| Transport | Direct TCP first → relay fallback |
| E2EE | `Noise_NNpsk0` or **`Noise_XXpsk3`** (identity) + ChaCha20-Poly1305 |
| Large messages | **25 MiB** logical max; auto-split to ~60 KiB Noise fragments |
| Verify | SAS emoji / numbers / hex, TOFU store |
| Rekey | Coordinated transport rekey |
| HD VC | Offer/answer, 720p/1080p profiles, video+audio frames, PLI, jitter helper |
| Files / photos | Chunked transfer + sha256, media streams |
| Discovery | LAN UDP beacon |
| Relay | Ciphertext-only WebSocket client |

## Production relay

```text
relay-production-eb30.up.railway.app
```

See **[RELAY.md](./RELAY.md)** for wishlist (frame size, limits API, keepalive, STUN/TURN for real VC).

## Quickstart

```powershell
$env:PEERSEAL_RELAY = "relay-production-eb30.up.railway.app"

# Chat + /file /photo /rekey /sas
cargo run --example pair_demo -- host --relay-only
# second terminal:
cargo run --example pair_demo -- guest --relay-only

# Automated file+photo over relay
cargo run --example transfer_demo

# HD VC media (1080p headers + frames over E2E relay)
cargo run --example vc_demo --release
# full raw 1080p RGB frames (~6 MiB each) — stress test:
cargo run --example vc_demo --release -- --full-rgb --frames 5

# Minimal ping/pong smoke
cargo run --example relay_smoke
```

### HD VC in code

```rust
use peerseal::{VcCall, VcConfig, VideoCodec, video_frame_from_payload};

let mut call = VcCall::new(VcConfig::hd_1080p30());
session.vc_send_offer(&call).await?;
// ... on offer: session.vc_send_answer(&mut call, &offer).await?;

let frame = video_frame_from_payload(
    1920, 1080, VideoCodec::H264, true, 0, 0, encoded_annex_b,
);
session.vc_send_video(&mut call, frame).await?;
```

Encode with ffmpeg / hardware / browser; peerseal carries the bitstream **E2E encrypted**.

## Library sketch

```rust
use peerseal::{Identity, Node, AppMessage};
use std::time::Duration;

let id = Identity::load_or_create("peer.key")?;
let node = Node::bind("0.0.0.0:0").await?
    .with_identity(id)
    .with_relay("relay-production-eb30.up.railway.app")?;

let invite = node.create_invite(Duration::from_secs(120))?;
let mut session = node.accept_peer(&invite).await?;

println!("Compare SAS: {}", session.info.sas_emojis());
session.send_text("hello").await?;
session.send_file("photo.jpg", "image/jpeg").await?;
session.send_photo(&jpeg_bytes, "image/jpeg", 1280, 720).await?;

// guest
// let mut session = Node::guest().with_identity(id2).join_invite(invite).await?;
match session.recv_app().await? {
    AppMessage::Text(t) => println!("{t}"),
    AppMessage::FileMeta { .. } => { /* session.recv_file(...) */ }
    AppMessage::MediaStart { .. } => { /* drain frames */ }
    _ => {}
}
```

## Security model

- **Invite token** binds the session (PSK). Treat invite as secret; short TTL.
- **Identity (X25519 static)** via `Noise_XXpsk3` authenticates the peer device across sessions.
- **SAS**: compare emojis/numbers out-of-band to catch MITM who stole a live invite.
- **TOFU**: remember fingerprints after first good verify.
- **Rekey**: rotates AEAD keys during long sessions (screen share / VC).
- **Relay**: untrusted; only ciphertext. No custom crypto primitives.

Patterns:

- `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` — no identity
- `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` — with `Identity`

## Modules

| Module | Role |
|--------|------|
| `invite` | QR encode/decode, TTL, PSK |
| `identity` | X25519 keys, fingerprint, TOFU |
| `sas` | Emoji / hex / numeric SAS |
| `session` | Noise handshake, AEAD frames, rekey |
| `protocol` | Typed app messages |
| `transfer` | Chunked files + sha256 |
| `media` | Photo / screen / audio stream helpers |
| `discovery` | LAN UDP beacon |
| `transport` | TCP + WS relay |
| `node` | High-level host/guest API |

## Tests

```powershell
cargo test

$env:PEERSEAL_LIVE_RELAY = "1"
$env:PEERSEAL_RELAY = "relay-production-eb30.up.railway.app"
cargo test --test integration_relay_live -- --nocapture
```

## What still needs the app (not this crate)

- Camera / mic capture, Opus/H264 encode
- Screen capture API (platform-specific)
- UI for SAS confirmation
- WebRTC stack if you want full-mesh HD video (relay can add STUN/TURN — see RELAY.md)

## License

MIT OR Apache-2.0
