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

    println!("cargo:rustc-env=CONVEX_URL={}", env_value("CONVEX_URL"));
    println!("cargo:rustc-env=TURN_URL={}", env_value("TURN_URL"));
    println!("cargo:rustc-env=TURN_USERNAME={}", env_value("TURN_USERNAME"));
    println!(
        "cargo:rustc-env=TURN_CREDENTIAL={}",
        env_value("TURN_CREDENTIAL")
    );

    // peerseal WebSocket relay (ciphertext-only). Env PEERSEAL_RELAY wins;
    // otherwise the production Railway host from the protocol docs.
    let relay = env_value("PEERSEAL_RELAY");
    let relay = if relay.is_empty() {
        "relay-production-eb30.up.railway.app".to_string()
    } else {
        relay
    };
    println!("cargo:rustc-env=PEERSEAL_RELAY={relay}");

    #[cfg(windows)]
    embed_app_icon();
}

/// Renders `assets/textures/talkyssicon.png` down to the standard Windows
/// icon sizes and links the result into the .exe's own PE resources, so the
/// file itself shows the app icon in Explorer/taskbar/pinned shortcuts —
/// separate from (and in addition to) the runtime window icon set in
/// `main.rs`, which only takes effect once the app is already running.
#[cfg(windows)]
fn embed_app_icon() {
    println!("cargo:rerun-if-changed=assets/textures/talkyssicon.png");

    let source = image::open("assets/textures/talkyssicon.png")
        .expect("assets/textures/talkyssicon.png must be a valid image")
        .to_rgba8();

    let sizes = [256u32, 128, 64, 48, 32, 16];
    let mut frames = Vec::new();
    for size in sizes {
        let resized = image::imageops::resize(
            &source,
            size,
            size,
            image::imageops::FilterType::Lanczos3,
        );
        frames.push(
            image::codecs::ico::IcoFrame::as_png(&resized, size, size, image::ColorType::Rgba8.into())
                .expect("failed to encode ICO frame"),
        );
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let ico_path = std::path::Path::new(&out_dir).join("talkyssicon.ico");
    let ico_file = std::fs::File::create(&ico_path).expect("failed to create .ico in OUT_DIR");
    image::codecs::ico::IcoEncoder::new(ico_file)
        .encode_images(&frames)
        .expect("failed to write .ico file");

    let rc_path = std::path::Path::new(&out_dir).join("app_icon.rc");
    std::fs::write(
        &rc_path,
        format!("IDI_ICON1 ICON \"{}\"\n", ico_path.display().to_string().replace('\\', "\\\\")),
    )
    .expect("failed to write .rc file");

    embed_resource::compile(&rc_path, embed_resource::NONE);
}
