# HexaTalk

Native desktop messenger in **Rust + Slint** with a private backend (`https://api.vyrapp.pro`). Includes an Android client, a P2P E2EE library (**peerseal**), WebRTC voice / screen share, and Discord-style servers.

## Features

### Chat & Social
- Registration / login (sessions + Bearer token)
- Friends, requests, blocks, nicknames / favorites
- 1:1 DMs (optional live peerseal E2EE), group DMs, server channels
- Reactions, replies, mentions, pin, attachments, typing indicators, presence

### Servers (Discord-like)
- Roles + bitfield permissions (`VIEW`, `SEND`, `MANAGE_*`, `VOICE`, `ANNOUNCE`)
- Channel categories with ordering
- Permission overwrites per channel (role / member)
- `#announcements` — always on top, read-only for everyone except staff
- Mute server / channel (no toast spam)

### Voice & Media
- 1:1 WebRTC calls with ADPCM audio codec
- Voice rooms (full-mesh topology)
- Screen sharing (JPEG over data channel)
- System audio capture (Stereo Mix / VB-Cable / loopback)
- Per-viewer mute with signal back to the sharer
- Go-live quality HUD: fps, kbps, KB/frame
- On-device audio DSP: noise gate, HPF, AGC, deharsh

### Security
- Platform admin panel (users, roles, bans, stats, reports)
- SAS / fingerprint verification in peerseal DMs

### Mobile
- Android client in `crates/hexatalk-mobile` — chat, friends, servers

### Bots
- Headless `hexatalk-bot` SDK (login token, send to channels)

## Project Structure

| Path | Role |
|------|------|
| `src/` | Desktop app (Slint UI, WebRTC, peerseal, tray) |
| `src/net/api/` | REST + WebSocket client for api.vyrapp.pro |
| `ui/*.slint` | GUI markup |
| `crates/reprotocol` | peerseal P2P library |
| `crates/hexatalk-bot` | Bot SDK |
| `crates/hexatalk-mobile` | Android client |
| `server/hexatalk-relay` | peerseal WebSocket relay |

## Running

```bash
cargo run        # desktop (API: https://api.vyrapp.pro)
```

Override the API URL via `.env.local`: `API_URL=…`.

Mobile APK: see `crates/hexatalk-mobile/README.md`.

## Licensing

| Component | License |
|-----------|---------|
| Desktop app + mobile | `GPL-3.0-only` |
| `reprotocol` (peerseal) | `GPL-3.0-only OR Apache-2.0` |
| `hexatalk-relay` | `GPL-3.0-only OR Apache-2.0` |
| `hexatalk-bot` | `Apache-2.0` |
