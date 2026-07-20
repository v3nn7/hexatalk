# HexaTalk Mobile (Rust + **Slint**)

Natywny klient Android w **Rust**. UI = **Slint** (ten sam toolkit co desktop),
backend = **Convex** HTTP.

**egui zostało usunięte** — nie ma stabilnego long-press paste na Androidzie.
Slint używa systemowego IME → long-press → Paste działa jak w zwykłej apce.

## Build APK

```powershell
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
$env:ANDROID_NDK_ROOT = (Get-ChildItem "$env:ANDROID_HOME\ndk" | Sort-Object Name -Descending | Select-Object -First 1).FullName
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

cd crates\hexatalk-mobile
cargo apk build --release --target aarch64-linux-android --lib

# output:
# target\release\apk\HexaTalk.apk
```

```powershell
adb install -r target\release\apk\HexaTalk.apk
```

## Preview na Windows

```powershell
cd crates\hexatalk-mobile
cargo run --example desktop
```

## Funkcje

- Login / rejestracja (email wymagany przy sign-up)
- Chats / Friends / Servers (poll ~1.5 s)
- Wysyłanie wiadomości, typing
- Kanały tekstowe serwera
- Profil (display name / status / bio)
- Violet Night (jak desktop)
- **Native TextInput** → long-press Paste

Package id: `com.hexatalk.mobile`
