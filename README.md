# HexaTalk

Natywny komunikator desktop (Rust + **Slint**) z własnym API
(`https://api.vyrapp.pro`). Dodatkowo: mobile Android (egui), bot SDK,
P2P **peerseal** (E2EE), WebRTC głos / screen share, serwery w stylu Discord.

## Funkcje (aktualne)

### Czat i social
- Rejestracja / logowanie (sesje + Bearer token)
- Znajomi, zaproszenia, bloki, nicki / ulubione
- DM 1:1 (opcjonalnie live peerseal E2EE), grupy, kanały serwerowe
- Reakcje, reply, mentions, pin, załączniki, typing, presence

### Serwery (Discord-like)
- Role + bitfield permissions (`VIEW`, `SEND`, `MANAGE_*`, `VOICE`, **`ANNOUNCE`**)
- **Kategorie kanałów** + position
- **Permission overwrites** per kanał (rola / członek)
- **#announcements** — zawsze na górze, wszyscy czytają, piszą tylko staff (`ANNOUNCE` / owner)
- Mute kanału / serwera — desktop nie spamuje toastami

### Głos i media
- Call 1:1 WebRTC + ADPCM
- Voice rooms (full-mesh)
- Screen share (JPEG over data channel)
- **System audio** (best-effort: Stereo Mix / VB-Cable / loopback)
- **Mute stream** po stronie oglądającego (+ sygnał do sharera)
- **Go-live quality HUD**: fps / kbps / KB/frame
- Lokalne DSP: noise gate + HPF + **AGC** + deharsh (niski CPU, na urządzeniu)

### Bezpieczeństwo
- Platform admin panel (lista userów, role, ban, stats, reports)
- SAS / fingerprint peerseal w DM

### Mobile + infra
- Android crate (`crates/hexatalk-mobile`) — chat, friends, servers
- Auto-update check, tray, installer

### Boty
- Headless `hexatalk-bot` SDK (login token, send to channels)

## Struktura

| Ścieżka | Rola |
|---------|------|
| `src/` | Desktop app (Slint UI, WebRTC, peerseal, tray) |
| `src/net/api/` | REST + WebSocket adapter do api.vyrapp.pro |
| `ui/*.slint` | GUI |
| `crates/reprotocol` | peerseal P2P (nie edytować — tylko integrować) |
| `crates/hexatalk-bot` | Bot SDK |
| `crates/hexatalk-mobile` | Android client |
| `server/hexatalk-relay` | Relay peerseal |

## Uruchomienie

```bash
cargo run        # desktop (API: https://api.vyrapp.pro)
```

Opcjonalnie w `.env.local`: `API_URL=…` (override bake w `build.rs`).

Mobile APK: zobacz `crates/hexatalk-mobile/README.md`.

## Model bezpieczeństwa

Własny system logowania (Bearer session token). Do produkcji: 2FA,
twardsza rotacja tokenów, FCM z podpisem. E2EE DM przez peerseal; historia
na serwerze jest opcjonalna (`storeChatHistory` / per-chat store).
