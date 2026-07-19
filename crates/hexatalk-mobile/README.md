# HexaTalk Mobile (Rust / egui)

Natywny klient Android w **Rust** — ten sam Convex co desktop, bez lagów Jetpack Compose.

## Artefakty

| Co | Gdzie |
|----|--------|
| **APK (arm64)** | `target/release/apk/HexaTalk.apk` (~4 MB) |
| Kod | `crates/hexatalk-mobile/` |

Package id: `com.hexatalk.mobile` (osobny od starego Kotlin `com.talkyss.android`).

## Funkcje

- Login / rejestracja + sesja na dysku  
- Czaty, znajomi, serwery (poll ~1.5 s, HTTP + rustls)  
- Wysyłanie wiadomości, typing  
- Kanały tekstowe serwera  
- Ciemny green UI jak desktop  

## Build APK

```powershell
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
$env:ANDROID_NDK_ROOT = (Get-ChildItem "$env:ANDROID_HOME\ndk" | Sort-Object Name -Descending | Select-Object -First 1).FullName
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

cd crates\hexatalk-mobile
cargo apk build --release --target aarch64-linux-android

adb install -r target\release\apk\HexaTalk.apk
```

Wymaga: NDK, `rustup target add aarch64-linux-android`, `cargo install cargo-apk`.

## Preview UI na Windows

```powershell
cd crates\hexatalk-mobile
cargo run --example desktop
```

Opcjonalnie ustaw `CONVEX_URL` (domyślnie ten sam deployment co desktop).

## Stary Kotlin

Folder `android/` (Compose) zostaje jako legacy. Preferuj ten crate.

## Dlaczego nie laguje jak Compose

- Immediate-mode **egui** (brak drzewa recomposition)  
- **2 wątki** tokio + poll HTTP (bez ciężkiego AAR Convex JNI)  
- Tylko **arm64-v8a**, bez MIPS / multi-arch bloat  
- Brak OpenSSL (czysty **rustls**)  
