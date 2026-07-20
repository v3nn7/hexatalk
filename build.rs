//! Bakes a handful of deployment-specific values into the binary at build
//! time (read from `.env.local` or `.env` in the crate root, where
//! `npx convex dev` writes `CONVEX_URL`) so the compiled .exe works
//! standalone -- e.g. copied to the Desktop with no `.env.local` sitting
//! next to it -- instead of only working when launched from a directory
//! that happens to contain that file. `main.rs`/`call.rs` still check the
//! runtime environment first, so a `.env.local` next to the exe (or the
//! working directory) can still override these for switching deployments
//! or adding a TURN relay without rebuilding.
//!
//! `TURN_URL`/`TURN_USERNAME`/`TURN_CREDENTIAL` are optional: STUN-only
//! (direct P2P) is used when they're absent, which is the default and
//! works for most network pairs. They only need to be set if a self-hosted
//! or third-party TURN relay is added later for the networks where direct
//! P2P is impossible (e.g. both sides behind symmetric NAT).

use std::fs;

fn env_value(key: &str) -> String {
    let mut value = String::new();
    for filename in [".env.local", ".env"] {
        let Ok(content) = fs::read_to_string(filename) else {
            continue;
        };
        let prefix = format!("{key}=");
        for line in content.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let found = rest.split('#').next().unwrap_or(rest).trim();
            if !found.is_empty() {
                value = found.to_string();
            }
        }
        if !value.is_empty() {
            break;
        }
    }
    value
}

fn main() {
    println!("cargo:rerun-if-changed=.env.local");
    println!("cargo:rerun-if-changed=.env");

    slint_build::compile("ui/main.slint").expect("failed to compile ui/main.slint");

    emit_obfuscated_secrets();

    #[cfg(windows)]
    embed_app_icon();
}

/// XOR key for the baked-in values below. This is obfuscation, not
/// encryption: the point is that `strings.exe` / a hex viewer no longer
/// shows the deployment URL, TURN credentials, relay host or update
/// endpoints in plaintext, not to withstand a determined reverser.
///
/// Hard limits, because the key sits right here in the repo: anyone with
/// the source (or ~15 minutes with a disassembler) can recover every baked
/// value, so this is an anti-`strings` measure only — it does NOT make a
/// value secret. Rule of thumb: never bake anything here whose leak would
/// cost money or access — no paid-API keys, no TURN credentials with
/// billing attached, no signing keys. Baked values should be things that
/// are acceptable to treat as public-but-not-greppable (deployment URLs,
/// hosts, the update public key). Real secrets belong in runtime env vars
/// or a server-side component, never in this list.
const OBF_KEY: [u8; 32] = [
    0xA7, 0x3C, 0x59, 0xE1, 0x06, 0x8B, 0xD4, 0x2F, 0x71, 0x9E, 0x42, 0xB8, 0x1D, 0xF3, 0x65, 0xC0,
    0x38, 0xAA, 0x0B, 0x97, 0x54, 0xDC, 0x21, 0x7F, 0xE8, 0x4A, 0x13, 0xB5, 0x6E, 0x90, 0x27, 0xFB,
];

/// Mirrors `deobfuscate` in src/obf.rs — keep the two in sync.
fn obfuscate(plain: &str) -> Vec<u8> {
    plain
        .bytes()
        .enumerate()
        .map(|(i, b)| b ^ OBF_KEY[i % OBF_KEY.len()] ^ (i as u8).wrapping_mul(0x5A))
        .collect()
}

/// Writes every deployment value baked into the exe as XOR-obfuscated byte
/// arrays (`OUT_DIR/obf_secrets.rs`, included by src/obf.rs) instead of
/// `cargo:rustc-env=` + `env!()`, which embedded them as plain UTF-8
/// strings visible to anyone running `strings` on the binary. Runtime
/// environment variables still override these at launch (unchanged
/// behavior), so a `.env.local` next to the exe keeps working.
fn emit_obfuscated_secrets() {
    // peerseal WebSocket relay (ciphertext-only). Env PEERSEAL_RELAY wins;
    // otherwise the production Railway host from the protocol docs.
    let relay = env_value("PEERSEAL_RELAY");
    let relay = if relay.is_empty() {
        "relay-production-eb30.up.railway.app".to_string()
    } else {
        relay
    };

    // Update-host endpoints + the ed25519 release public key (moved here
    // from src/update_check.rs so they go through the same obfuscation).
    let values: [(&str, String); 11] = [
        ("CONVEX_URL", env_value("CONVEX_URL")),
        ("TURN_URL", env_value("TURN_URL")),
        ("TURN_USERNAME", env_value("TURN_USERNAME")),
        ("TURN_CREDENTIAL", env_value("TURN_CREDENTIAL")),
        ("PEERSEAL_RELAY", relay),
        ("UPDATE_VERSION_URL", "https://astrakit.pro/version.txt".to_string()),
        ("UPDATE_DOWNLOAD_URL", "https://astrakit.pro/HexaTalk.exe".to_string()),
        (
            "UPDATE_SIGNATURE_URL",
            "https://astrakit.pro/HexaTalk.exe.sig".to_string(),
        ),
        (
            "UPDATE_PUBLIC_KEY_B64",
            "0cJIouMNtQV708XWpDinnsSjevzQm8bQ2mxpe6/s9eg=".to_string(),
        ),
        // Directory (trailing slash) that bsdiff-compatible incremental
        // update patches are uploaded to, named `HexaTalk-<from>-<to>.delta`
        // (see src/update_check.rs). Missing/mismatched deltas just fall
        // back to the full-exe download above, so this doesn't need to be
        // kept as tightly in sync as the other update endpoints.
        (
            "UPDATE_DELTA_BASE_URL",
            "https://astrakit.pro/deltas/".to_string(),
        ),
        // AES-256 key (base64 of 32 raw bytes) used to decrypt HTD1-framed
        // delta files on download. Must match RELEASE_DELTA_KEY_HEX used by
        // scripts/encrypt_delta.py / release.ps1. Override via env for
        // local builds that ship against a different key.
        (
            "UPDATE_DELTA_KEY_B64",
            {
                let from_env = env_value("UPDATE_DELTA_KEY_B64");
                if from_env.is_empty() {
                    "wU4ytiPCMY7mv2fMhdiLpUaiILJp4a9SSH7sICdKVU8=".to_string()
                } else {
                    from_env
                }
            },
        ),
    ];

    let mut out = String::from(
        "// @generated by build.rs (emit_obfuscated_secrets) — do not edit.\n\
         // XOR-obfuscated baked-in deployment values; decode via src/obf.rs.\n\n",
    );
    out.push_str(&format!("pub(crate) const OBF_KEY: [u8; 32] = {OBF_KEY:?};\n"));
    for (name, value) in &values {
        out.push_str(&format!(
            "pub(crate) static OBF_{name}: &[u8] = &{:?};\n",
            obfuscate(value)
        ));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    std::fs::write(std::path::Path::new(&out_dir).join("obf_secrets.rs"), out)
        .expect("failed to write obf_secrets.rs");
}

/// Renders `assets/textures/hexatalkicon.png` down to the standard Windows
/// icon sizes and links the result into the .exe's own PE resources, so the
/// file itself shows the app icon in Explorer/taskbar/pinned shortcuts —
/// separate from (and in addition to) the runtime window icon set in
/// `main.rs`, which only takes effect once the app is already running.
#[cfg(windows)]
fn embed_app_icon() {
    println!("cargo:rerun-if-changed=assets/textures/hexatalkicon.png");

    let source = image::open("assets/textures/hexatalkicon.png")
        .expect("assets/textures/hexatalkicon.png must be a valid image")
        .to_rgba8();

    let sizes = [256u32, 128, 64, 48, 32, 16];
    let mut frames = Vec::new();
    for size in sizes {
        let resized =
            image::imageops::resize(&source, size, size, image::imageops::FilterType::Lanczos3);
        frames.push(
            image::codecs::ico::IcoFrame::as_png(
                &resized,
                size,
                size,
                image::ColorType::Rgba8.into(),
            )
            .expect("failed to encode ICO frame"),
        );
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let ico_path = std::path::Path::new(&out_dir).join("hexatalkicon.ico");
    let ico_file = std::fs::File::create(&ico_path).expect("failed to create .ico in OUT_DIR");
    image::codecs::ico::IcoEncoder::new(ico_file)
        .encode_images(&frames)
        .expect("failed to write .ico file");

    let rc_path = std::path::Path::new(&out_dir).join("app_icon.rc");
    std::fs::write(
        &rc_path,
        format!(
            "IDI_ICON1 ICON \"{}\"\n",
            ico_path.display().to_string().replace('\\', "\\\\")
        ),
    )
    .expect("failed to write .rc file");

    embed_resource::compile(&rc_path, embed_resource::NONE);
}
