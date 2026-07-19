# Talkyss

Natywny komunikator desktop (Rust + **Slint**) z realtime bazą
[Convex](https://convex.dev). Dodatkowo: mobile Android (egui), bot SDK,
P2P **peerseal** (E2EE), WebRTC głos / screen share, serwery w stylu Discord.

## Funkcje (aktualne)

### Czat i social
- Rejestracja / logowanie (PBKDF2 + sesje), limity logowania
- Znajomi, zaproszenia, bloki, nicki / ulubione
- DM 1:1 (opcjonalnie live peerseal E2EE), grupy, kanały serwerowe
- Reakcje, reply, mentions, pin, załączniki, typing, presence

### Serwery (Discord-like)
- Role + bitfield permissions (`VIEW`, `SEND`, `MANAGE_*`, `VOICE`, **`ANNOUNCE`**)
- **Kategorie kanałów** + position
- **Permission overwrites** per kanał (rola / członek)
- **#announcements** — zawsze na górze, wszyscy czytają, piszą tylko staff (`ANNOUNCE` / owner)
- Mute kanału / serwera (`notificationPrefs`) — desktop nie spamuje toastami

### Głos i media
- Call 1:1 WebRTC + ADPCM
- Voice rooms (full-mesh)
- Screen share (JPEG over data channel)
- **System audio** (best-effort: Stereo Mix / VB-Cable / loopback)
- **Mute stream** po stronie oglądającego (+ sygnał do sharera)
- **Go-live quality HUD**: fps / kbps / KB/frame
- Lokalne DSP: noise gate + HPF + **AGC** + deharsh (niski CPU, na urządzeniu)

### Bezpieczeństwo
- Lista sesji / revoke (`prefs:listSessions`, `prefs:revokeSession`)
- `prefs:touchSession` (device name + platform) przy logowaniu
- Sign out other devices
- SAS / fingerprint peerseal w DM

### Mobile + infra
- Android crate (`crates/talkyss-mobile`) — chat, friends, servers
- `push:registerToken` + `push:tokensForConversationNotify` (FCM/APNs keys w Convex env)
- Auto-update check, tray, installer

### Boty
- Headless `talkyss-bot` SDK (login token, send to channels)

## Struktura

| Ścieżka | Rola |
|---------|------|
| `convex/` | Schema + mutations/queries (auth, servers, channels, messages, voice, push…) |
| `src/` | Desktop app (Slint UI, WebRTC, peerseal, tray) |
| `ui/*.slint` | GUI |
| `crates/reprotocol` | peerseal P2P (nie edytować — tylko integrować) |
| `crates/talkyss-bot` | Bot SDK |
| `crates/talkyss-mobile` | Android client |
| `server/talkyss-relay` | Relay peerseal |

## Uruchomienie

```bash
npm install
npx convex dev   # terminal 1 — wdraża schema + functions
cargo run        # terminal 2 — desktop
```

Mobile APK: zobacz `crates/talkyss-mobile/README.md`.

## Nowe API (skrót)

- `channels:*` — categories, overwrites, mute, channel perms
- `messages:search` — wyszukiwanie plaintext w historii
- `prefs:listSessions` / `revokeSession` / `touchSession`
- `push:tokensForConversationNotify` — respektuje mute

## Model bezpieczeństwa

Własny system logowania (token sesji w argumencie). Do produkcji: 2FA,
twardsza rotacja tokenów, FCM z podpisem. E2EE DM przez peerseal; historia
Convex jest opcjonalna (`storeChatHistory` / per-chat store).
