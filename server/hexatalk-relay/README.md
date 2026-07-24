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
| `❌ invalid token` / `❌ room full (max N)` / `❌ invalid room id` / `❌ server room limit reached, try again later` / `❌ too many rooms from your address` | błąd krytyczny — klient przerywa bez retry |

Po handshake'u każda **binarna ramka WebSocket** od jednego peera jest przekazywana bez zmian do pozostałych członków pokoju. Obsługiwanych jest wiele pokoi naraz i wielu peerów w pokoju (np. grupowy voice). Pokój żyje tak długo, jak długo ma peerów.

**Ramki tekstowe od klientów są odrzucane** (liczone w logu `peer left` jako `text_dropped`). Legalny klient wysyła ciphertext wyłącznie jako ramki binarne, a forwardowanie tekstu pozwalałoby złośliwemu peerowi podrzucać innym sesjom sfałszowane linie `❌`/`✅`/`⏳` — jedyny tekst na kablu to linie statusowe generowane przez serwer.

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
RELAY_TOKEN=TWOJ_SEKRET hexatalk-relay --bind 0.0.0.0:9000
```

**Produkcyjnie token jest WYMAGANY** — bez niego relay jest otwarty dla każdego (serwer loguje wtedy ostrzeżenie przy starcie). Sekret przekazuj zmienną środowiskową **`RELAY_TOKEN`** (niewidoczna w `ps aux` ani w pliku unitu systemd, w przeciwieństwie do `--token`). Flaga `--token` istnieje dla wygody lokalnej i **nadpisuje** `RELAY_TOKEN`. Porównanie tokenu jest constant-time (bez wczesnego wyjścia na pierwszym różnym bajcie).

Opcje:

| Opcja | Domyślnie | Opis |
|---|---|---|
| `--bind <ADDR>` | `0.0.0.0:9000` | adres nasłuchu |
| `--token <TOKEN>` | *(brak)* | wspólny sekret wymagany jako `?token=`; nadpisuje `RELAY_TOKEN` |
| `--max-peers <N>` | `16` | maks. peerów w pokoju |
| `--max-frame <BYTES>` | `1048576` | maks. rozmiar ramki WS (klient chunkuje do ~60 KiB) |
| `--max-conn-per-ip <N>` | `32` | maks. aktywnych połączeń z jednego IP (nadmiar → HTTP `429`) |
| `--max-rooms <N>` | `10000` | maks. pokoi ogółem (nadmiar → `❌ server room limit reached, try again later`) |
| `--max-rooms-per-ip <N>` | `64` | maks. pokoi tworzonych z jednego IP (nadmiar → `❌ too many rooms from your address`) |

Zmienna środowiskowa: **`RELAY_TOKEN`** — wspólny sekret, używany gdy nie podano `--token`.

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
EnvironmentFile=/etc/hexatalk-relay.env
ExecStart=/usr/local/bin/hexatalk-relay --bind 0.0.0.0:9000
Restart=always
RestartSec=2
Environment=RUST_LOG=info
NoNewPrivileges=true
ProtectSystem=strict
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Sekret trzymaj w `/etc/hexatalk-relay.env` (nie w `ExecStart` — tam byłby widoczny w `ps aux` i w samym pliku unitu):

```bash
# /etc/hexatalk-relay.env
RELAY_TOKEN=ZMNIEN_TEN_SEKRET
```

```bash
echo 'RELAY_TOKEN=ZMNIEN_TEN_SEKRET' | sudo tee /etc/hexatalk-relay.env
sudo chown root:hexatalk /etc/hexatalk-relay.env
sudo chmod 0640 /etc/hexatalk-relay.env
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

Klient dołoży sam ścieżkę: `ws://1.2.3.4:9000/v1/room/{room_id}?token={token}`. Uwaga: klient wysyła w `?token=` **token pokoju z invite**, więc serwerowy `--token`/`RELAY_TOKEN` musi mu odpowiadać — sensowne tylko wtedy, gdy kontrolujesz obie strony (własny serwer + rozdawane invite). Na produkcji: **token WYMAGANY + `wss://` przez reverse proxy** (patrz niżej).

### TLS / wss przez reverse proxy (WYMAGANE na produkcji)

Serwer celowo nie mówi TLS — terminuj go na reverse proxy (wtedy możesz też użyć gołej domeny w `PEERSEAL_RELAY`). Postaw przed relayem np. **Caddy**, który sam ogarnie certyfikat Let's Encrypt:

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

⚠️ **Limity per-IP za proxy:** gdy relay stoi za reverse proxy, wszystkie połączenia przychodzą z adresu proxy (np. `127.0.0.1`), więc `--max-conn-per-ip` / `--max-rooms-per-ip` dotyczą wtedy proxy jako całości. Za proxy albo podnieś te limity (`--max-conn-per-ip` odpowiednio do ruchu), albo tnij flood na samym proxy.

(Uwaga: `is_transient_relay_error` w aplikacji traktuje m.in. `close_notify` / `unexpected EOF` jako retryable — częste przy proxy zrywających idle WSS; nasz serwer wysyła ping co 25 s, co temu zapobiega.)

## Inwariant bezpieczeństwa: relay nigdy nie widzi treści

Relay jest **ślepy na treść** (ciphertext-blind) — to nie jest opis "na oko", tylko sprawdzana testem właściwość:

- Każda ramka `Binary` od jednego peera trafia do pozostałych **bez żadnego parsowania/deserializacji** — patrz `broadcast()` w `main.rs`, wywoływane bezpośrednio z pętli odczytu.
- Ramki `Text` **od klienta** są zawsze odrzucane (licznik `text_dropped`), więc złośliwy peer nie może wstrzyknąć fałszywej linii statusu (`✅`/`⏳`/`❌`) do cudzej sesji — jedyny tekst na kablu generuje sam serwer.
- Logi (`debug!`/`info!`/`warn!`) niosą wyłącznie metadane — `peer_id`, `room_id`, liczniki (`count`, `bytes_in`, `remaining`, `text_dropped`) — **nigdy treść ramki**. Żadne wywołanie logujące nie formatuje bajtów payloadu.

Test `tests::relay_forwards_binary_verbatim_drops_client_text_never_logs_payload` (`src/main.rs`) to weryfikuje end-to-end na realnym listenerze i dwóch klientach WebSocket: wstrzykuje unikalny "kanarek" w ramkę binarną (musi dotrzeć 1:1 do drugiego peera) i w ramkę tekstową (nie może dotrzeć wcale), po czym przechwytuje cały output `tracing` z tego przebiegu i asercjonuje, że kanarek nigdze w logach się nie pojawił. Uruchom: `cargo test` w tym katalogu.

## Szybki test po wdrożeniu

```bash
curl http://IP_VPS:9000/healthz      # -> ok
curl http://IP_VPS:9000/v1/limits    # -> JSON z limitami
```
