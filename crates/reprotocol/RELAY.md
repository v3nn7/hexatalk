# Relay requirements for HD VC + 25 MiB logical frames

Peerseal client **already chunks** large logical messages into ~60 KiB Noise/WebSocket binary frames.
You do **not** need 25 MiB as a single WS frame for the current Rust client.

## What the client does

| Layer | Size |
|-------|------|
| Logical app message (one HD video AU / file piece) | **up to 25 MiB** |
| Noise plaintext fragment | ~60 KiB |
| WebSocket binary frame | ~60 KiB (+ a bit of AEAD) |

So relay max binary frame **≥ 64–128 KiB** is enough for peerseal v0.3+.

If other clients send unchunked blobs, set **`max_binary_frame = 26214400` (25 MiB)**.

## Required for HD VC quality

### 1. Throughput & buffering
- Prefer **binary** frames.
- Avoid tiny per-frame server processing overhead.
- Backpressure: if slow consumer, drop **oldest media** (optional); never drop file-transfer if you can distinguish (you usually cannot — opaque ciphertext). Best: large buffers (several MB) + high idle timeout.

### 2. Keepalive
- WS Ping/Pong every 15–30s  
- Idle timeout **≥ 300s** for calls  

### 3. Room lifetime
- Room lives while peers connected (not invite TTL).

### 4. `GET /v1/limits`
```json
{
  "max_binary_frame": 131072,
  "max_logical_hint": 26214400,
  "idle_timeout_sec": 300,
  "max_peers_per_room": 2,
  "supports_binary": true,
  "supports_ping": true
}
```

### 5. Logging
- Never log payload bodies.
- Metrics: bytes_forwarded, active_rooms, disconnect reasons.

## True WebRTC HD (optional next step on relay)

For browser-grade 1080p60 with less relay CPU:

1. **STUN** `stun:your-host:3478`
2. **TURN** with short-lived credentials  
3. Peerseal stays E2E **signaling** (SDP/ICE in encrypted chat)  
4. Media goes WebRTC (SRTP), not WS relay

Until then, peerseal HD VC uses **E2E encrypted media on the same Noise channel** (works today on Railway).

## Checklist for Railway deploy

- [ ] `max_binary_frame` ≥ 128 KiB (or 25 MiB if you want unchunked clients)
- [ ] Idle ≥ 300s + WS ping
- [ ] Room TTL = connection lifetime  
- [ ] `/v1/limits` JSON  
- [ ] No payload logging  
- [ ] (Later) STUN/TURN for WebRTC
