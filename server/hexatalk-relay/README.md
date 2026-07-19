# hexatalk-relay

Samodzielny (standalone) serwer relay kompatybilny z protokołem **peerseal** używanym przez aplikację HexaTalk. Jeden binarny plik, zero bazy danych, zero zależności systemowych. Relay widzi wyłącznie zaszyfrowany ciphertext (E2EE Noise) — nigdy nie loguje treści.

## Jak działa protokół

Klient (`crates/reprotocol/src/transport/relay.rs`) łączy się przez WebSocket pod adres:

```
ws(s)://HOST/v1/room/{room_id}?token={token}
```

Serwer przypisuje peer-id i wysyła **tekstowe linie statusu**, których klient oczekuje:

| Linia wysyłana przez serwer | Znaczenie dla klienta |
|---|---|
| `⏳ waiting for peer (1/16 slots)` | pierwszy peer w pokoju — czekaj |
| `✅ peer joined (N peers in room)` | peer gotowy — klient zaczyna wysyłać dane |
| `⏳ peer left (N peers remaining)` | peer się rozłączył (informacyjne) |
| `❌ invalid token` / `❌ room full (max N)` / `❌ invalid room id` | błąd krytyczny — klient przerywa bez retry |

Po handshake'u każda **binarna ramka WebSocket** od jednego peera jest przekazywana bez zmian do pozostałych członków pokoju. Obsługiwanych jest wiele pokoi naraz i wielu peerów w pokoju (np. grupowy voice). Pokój żyje tak długo, jak długo ma peerów.

Dodatkowe endpointy HTTP:

- `GET /v1/limits` — JSON z limitami (zgodnie z `crates/reprotocol/RELAY.md`)
- `GET /healthz` — `200 ok` (monitoring)

## Budowanie na Windows

W katalogu `server/hexatalk-relay`:

```powershell
cargo build --release
```

Binarka: `target\release\hexatalk-relay.exe`.

> Crate jest **celowo poza workspace'm** repo (ma własny pusty `[workspace]`), więc buduje się niezależnie.

## Budowanie na Linuxie (VPS)

**Najprościej: zbudować bezpośrednio na VPS-ie** (brak problemów z cross-linkowaniem z Windows):

```bash
# na VPS (Debian/Ubuntu)
sudo apt update && sudo apt install -y build-essential pkg-config curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# skopiuj źródła z Windowsa (z katalogu repo)
scp -r server/hexatalk-relay user@IP_VPS:/opt/hexatalk-relay-src

# na VPS
cd /opt/hexatalk-relay-src
cargo build --release
sudo install -m 0755 target/release/hexatalk-relay /usr/local/bin/hexatalk-relay
```

Alternatywnie — cross-kompilacja przez **WSL2** na tym samym Windowsie:

```bash
# w WSL2
rustup target add x86_64-unknown-linux-gnu
cd /mnt/c/Users/ratko/Desktop/talkyss/server/hexatalk-relay
cargo build --release --target x86_64-unknown-linux-gnu
# binarka: target/x86_64-unknown-linux-gnu/release/hexatalk-relay
scp target/x86_64-unknown-linux-gnu/release/hexatalk-relay user@IP_VPS:/tmp/
```

## Uruchomienie

```bash
hexatalk-relay --bind 0.0.0.0:9000 --token TWOJ_SEKRET
```

Opcje:

| Opcja | Domyślnie | Opis |
|---|---|---|
| `--bind <ADDR>` | `0.0.0.0:9000` | adres nasłuchu |
| `--token <TOKEN>` | *(brak)* | wspólny sekret wymagany jako `?token=` |
| `--max-peers <N>` | `16` | maks. peerów w pokoju |
| `--max-frame <BYTES>` | `1048576` | maks. rozmiar ramki WS (klient chunkuje do ~60 KiB) |

Logi na stdout (systemd zbiera je przez journald); poziom przez `RUST_LOG=debug`.

## systemd

Plik `/etc/systemd/system/hexatalk-relay.service`:

```ini
[Unit]
Description=HexaTalk peerseal-compatible relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=hexatalk
Group=hexatalk
ExecStart=/usr/local/bin/hexatalk-relay --bind 0.0.0.0:9000 --token ZMNIEN_TEN_SEKRET
Restart=always
RestartSec=2
Environment=RUST_LOG=info
NoNewPrivileges=true
ProtectSystem=strict
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Aktywacja:

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin hexatalk
sudo systemctl daemon-reload
sudo systemctl enable --now hexatalk-relay
sudo systemctl status hexatalk-relay
journalctl -u hexatalk-relay -f
```

## Firewall

```bash
sudo ufw allow 9000/tcp
sudo ufw status
```

## Podpięcie aplikacji HexaTalk pod ten serwer

Aplikacja czyta zmienną środowiskową **`PEERSEAL_RELAY`** (patrz `resolve_relay()` w `src/peer.rs`; wartość runtime nadpisuje tę wkompilowaną). Wartość przechodzi przez `normalize_relay_url()` w `crates/reprotocol/src/util.rs`, który działa tak:

| Wpiszesz | Zostanie znormalizowane do |
|---|---|
| `ws://1.2.3.4:9000` | `ws://1.2.3.4:9000` |
| `http://1.2.3.4:9000` | `ws://1.2.3.4:9000` |
| `wss://relay.twojadomena.pl` | `wss://relay.twojadomena.pl` |
| `https://relay.twojadomena.pl` | `wss://relay.twojadomena.pl` |
| **`1.2.3.4:9000`** (goły host) | **`wss://1.2.3.4:9000`** ⚠️ |

⚠️ **Ważne:** goły `IP:PORT` jest zamieniany na `wss://` (TLS). Ten serwer mówi czystym WebSocket **bez TLS**, więc przy łączeniu wprost po IP **musisz jawnie podać prefiks `ws://`**:

```powershell
# Windows (bieżąca sesja PowerShell)
$env:PEERSEAL_RELAY = "ws://1.2.3.4:9000"

# Windows (na stałe, dla użytkownika — odpal nową sesję/restart aplikacji)
setx PEERSEAL_RELAY "ws://1.2.3.4:9000"
```

Klient dołoży sam ścieżkę: `ws://1.2.3.4:9000/v1/room/{room_id}?token={token}` (token pokoju pochodzi z invite — to **nie** jest `--token` serwera; sekret serwera weryfikuj po swojej stronie tylko jeśli chcesz dodatkową warstwę, pamiętając że klient wysyła w `?token=` token pokoju, więc `--token` serwera musi mu odpowiadać — najprościej nie ustawiać `--token`, chyba że kontrolujesz obie strony).

### TLS / wss przez reverse proxy (opcjonalnie, zalecane dla domeny)

Jeśli chcesz `wss://` (i wtedy możesz użyć gołej domeny w `PEERSEAL_RELAY`), postaw przed relayem np. **Caddy**, który sam ogarnie certyfikat Let's Encrypt:

```
# /etc/caddy/Caddyfile
relay.twojadomena.pl {
    reverse_proxy 127.0.0.1:9000
}
```

Wtedy relay bindować lokalnie: `--bind 127.0.0.1:9000`, a w aplikacji:

```powershell
setx PEERSEAL_RELAY "wss://relay.twojadomena.pl"
```

(Uwaga: `is_transient_relay_error` w aplikacji traktuje m.in. `close_notify` / `unexpected EOF` jako retryable — częste przy proxy zrywających idle WSS; nasz serwer wysyła ping co 25 s, co temu zapobiega.)

## Szybki test po wdrożeniu

```bash
curl http://IP_VPS:9000/healthz      # -> ok
curl http://IP_VPS:9000/v1/limits    # -> JSON z limitami
```
