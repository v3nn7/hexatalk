// No console window on Windows for release builds (debug builds keep it,
// since it's the only place panics/eprintln! diagnostics show up).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod call;
mod convex_parse;
mod crypto;
mod g711;
mod history;
mod message;
mod notify;
mod peer;
mod room_voice;
mod screenshare;
mod session_store;
mod style;
mod subscriptions;
mod tray;
mod types;
mod update;
mod update_check;
mod view;

// Re-exported at crate root so every module can keep writing `use crate::*;`
// regardless of which file a given type/style-fn/message-variant physically
// lives in.
pub(crate) use app::App;
pub(crate) use convex_parse::*;
pub(crate) use message::Message;
pub(crate) use notify::*;
pub(crate) use session_store::*;
pub(crate) use style::*;
pub(crate) use subscriptions::*;
pub(crate) use types::*;
pub(crate) use update_check::*;

use std::env;

use iced::widget::scrollable;
use iced::widget::scrollable::RelativeOffset;
use iced::{Task, Theme};

const ONLINE_THRESHOLD_MS: f64 = 15_000.0;

fn chat_scroll_id() -> scrollable::Id {
    scrollable::Id::new("chat-history")
}

fn scroll_chat_to_bottom<T: 'static>() -> Task<T> {
    scrollable::snap_to(chat_scroll_id(), RelativeOffset::END)
}

const AVATAR_PALETTE: [&str; 8] = [
    "#3FB36B", "#2E9E6B", "#7FCBA0", "#2F8F57", "#A9B85E", "#5FB98C", "#27814F", "#9FD3B5",
];

// Must match `REACTION_EMOJIS` in convex/messages.ts -- the server rejects
// any emoji outside this allow-list.
const QUICK_REACT_EMOJIS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// Control payload sent over the live peerseal channel so the remote side
/// also wipes its local encrypted vault for this DM. Never shown in the UI.
const PEER_CLEAR_HISTORY_CTRL: &str = "\u{001e}TALKYSS_CLEAR_HISTORY\u{001e}";

/// Defensive cap on how many background peerseal sessions run at once (one
/// per online friend) — bounds concurrent Noise/relay connections for
/// accounts with very large friends lists.
const MAX_BACKGROUND_PEER_SESSIONS: usize = 25;





// ---------- Entry point ----------

fn main() -> iced::Result {
    dotenvy::from_filename(".env.local").ok();
    dotenvy::dotenv().ok();

    // rustls 0.23 needs an explicit crypto provider for peerseal WSS relay.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Prefer a real .env.local/.env next to the exe or in the working
    // directory (handy for pointing a dev build at a different deployment
    // without rebuilding); otherwise fall back to the URL `build.rs` baked
    // into the binary at compile time, so a standalone .exe copied
    // somewhere with no .env file still knows where to connect.
    const BAKED_IN_CONVEX_URL: &str = env!("CONVEX_URL");
    let deployment_url = env::var("CONVEX_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| BAKED_IN_CONVEX_URL.to_string());

    if deployment_url.is_empty() {
        eprintln!(
            "Missing CONVEX_URL in .env.local. Run `npx convex dev` and rebuild."
        );
        std::process::exit(1);
    }

    // Bundle UI fonts. Color emoji comes from the OS when available (Segoe UI
    // Emoji on Windows has COLR/CPAL). Bundled NotoEmoji is monochrome and
    // must NOT be registered alongside a color face — cosmic-text/swash will
    // prefer the mono outlines for the same codepoints and paint grey glyphs.
    const ROBOTO_REGULAR: &[u8] = include_bytes!("../assets/fonts/Roboto-Regular.ttf");
    const ROBOTO_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Roboto-Medium.ttf");
    const NOTO_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoEmoji.ttf");

    let mut app = iced::application("Talkyss", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_state| Theme::Dark)
        .antialiasing(true)
        .font(ROBOTO_REGULAR)
        .font(ROBOTO_MEDIUM)
        .default_font(iced::Font::with_name("Roboto"));

    // Load at most one color-emoji face first. Skip monochrome Noto when a
    // color font is present so fallback actually picks the color glyphs.
    let mut color_emoji_loaded = false;
    for path in system_color_emoji_font_paths() {
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                app = app.font(bytes);
                color_emoji_loaded = true;
                break;
            }
            _ => continue,
        }
    }
    if !color_emoji_loaded {
        app = app.font(NOTO_EMOJI);
    }

    app
        // Hide-to-tray only makes sense where the tray icon actually exists
        // (Windows for now -- see src/tray.rs). Elsewhere, closing the
        // window quits normally.
        .exit_on_close_request(cfg!(not(windows)))
        .window(iced::window::Settings {
            size: iced::Size::new(1180.0, 760.0),
            min_size: Some(iced::Size::new(980.0, 600.0)),
            icon: iced::window::icon::from_file_data(
                include_bytes!("../assets/textures/talkyssicon.png"),
                None,
            )
            .ok(),
            ..Default::default()
        })
        .run_with(move || App::new(deployment_url))
}

/// OS color-emoji font candidates (COLR/CBDT/sbix). First readable file wins.
/// Bundled monochrome Noto Emoji is only used when none of these exist.
fn system_color_emoji_font_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    {
        if let Ok(windir) = env::var("WINDIR") {
            let fonts = std::path::Path::new(&windir).join("Fonts");
            // Color COLR/CPAL face — do NOT list seguisym.ttf (mono symbols).
            paths.push(fonts.join("seguiemj.ttf"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(std::path::PathBuf::from(
            "/System/Library/Fonts/Apple Color Emoji.ttc",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(std::path::PathBuf::from(
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        ));
        paths.push(std::path::PathBuf::from(
            "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        ));
        paths.push(std::path::PathBuf::from(
            "/usr/share/fonts/truetype/noto-color-emoji/NotoColorEmoji.ttf",
        ));
    }
    paths
}
