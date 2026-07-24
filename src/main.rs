// No console window on Windows for release builds (debug builds keep it,
// since it's the only place panics/eprintln! diagnostics show up).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod crypto;
mod media;
mod net;
mod obf;
mod state;
mod tray;
mod ui;
mod update_check;

use crate::media::img_cache;
use crate::media::screenshare;
use crate::net::rt::{SubscriptionRegistry, Task, WindowAction};
use crate::state::app::App;
use crate::state::history;
use crate::state::message::Message;
use crate::state::types::{
    AdminUserRow, BlockedUser, BotSummary, ChannelSummary, ChatMessage, ConversationSummary,
    Friend, FriendSuggestion, FriendsFilter, IncomingRequest, MyCallInfo, OutgoingRequest,
    PERM_CONNECT_VOICE, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_ROLES,
    PERM_MANAGE_SERVER, PERM_SEND_MESSAGES, PERM_SPEAK, PERM_VIEW_CHANNELS, PeopleHit, ProfileView,
    ResizePanel, ServerMemberRow, ServerRoleRow, ServerSettingsCategory, ServerSummary, Session,
    SettingsCategory, SidebarTab, SocialStats, VoiceUserRow, is_online,
};
use crate::ui::utils::{
    friend_request_privacy_label, normalize_presence, presence_is_online_like, presence_label,
    typing_label,
};
use crate::ui::viewmodel;
use crate::update_check::CURRENT_APP_VERSION;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slint::ComponentHandle;
use slint::Model;
use slint::winit_030::{WinitWindowAccessor, winit};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

// Generated Slint bindings, kept in their own module (not glob-exported at
// crate root) so `slint_ui::AuthMode`/`slint_ui::Screen` never collide with
// the business-logic types of the same name in `state::types`.
mod slint_ui {
    slint::include_modules!();
}

/// Set by `scroll_chat_to_bottom()`, consumed (and cleared) by the chat
/// screen's UI sync step, which pulses the message list's scroll-to-end.
static CHAT_SCROLL_PENDING: AtomicBool = AtomicBool::new(false);

/// Monotonic counter bumped once per consumed CHAT_SCROLL_PENDING. The
/// Slint side watches the exported `chat_scroll_pulse` property for changes
/// (an increment = "scroll the message list to the end") -- a plain bool
/// couldn't signal two scrolls in a row with no intervening state change.
static CHAT_SCROLL_PULSE: AtomicU32 = AtomicU32::new(0);

fn scroll_chat_to_bottom<T: Send + 'static>() -> Task<T> {
    CHAT_SCROLL_PENDING.store(true, Ordering::Relaxed);
    Task::none()
}

const AVATAR_PALETTE: [&str; 8] = [
    "#3FB36B", "#2E9E6B", "#7FCBA0", "#2F8F57", "#A9B85E", "#5FB98C", "#27814F", "#9FD3B5",
];

// Must match `REACTION_EMOJIS` in convex/messages.ts -- the server rejects
// any emoji outside this allow-list.
const QUICK_REACT_EMOJIS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

/// Control payload sent over the live peerseal channel so the remote side
/// also wipes its local encrypted vault for this DM. Never shown in the UI.
/// NOTE: wire-protocol constant — must stay byte-identical across client
/// versions, so it keeps the pre-rebrand name on purpose.
const PEER_CLEAR_HISTORY_CTRL: &str = "\u{001e}TALKYSS_CLEAR_HISTORY\u{001e}";

/// Marks a message body as a voice note. The `messages:list` REST shape
/// (see `net/api/dispatch_conv.rs` module doc) has no attachment
/// content-type field at all -- only `attachmentUrl` -- so there is no
/// backend-provided way to tell a voice note apart from any other
/// attachment on the receiving end. Same trick as
/// `PEER_CLEAR_HISTORY_CTRL` above: encode it in the one field that *is*
/// guaranteed to round-trip end-to-end, wrapped in a control character so
/// it can never collide with real user text.
const VOICE_NOTE_BODY_TAG: &str = "\u{1}HEXATALK_VOICE_NOTE\u{1}";

/// Defensive cap on how many background peerseal sessions run at once (one
/// per online friend) — bounds concurrent Noise/relay connections for
/// accounts with very large friends lists.
const MAX_BACKGROUND_PEER_SESSIONS: usize = 25;

/// Validates `API_URL` at startup. Shipped builds must only talk to HTTPS
/// backends; a plaintext/HTTP override is rejected so a rogue `.env.local`
/// cannot silently downgrade traffic.
fn validate_api_url(raw: &str) -> Result<&str, &'static str> {
    let trimmed = raw.trim();
    if !(trimmed.starts_with("https://") || trimmed.starts_with("http://localhost:") || trimmed.starts_with("http://127.0.0.1:")) {
        return Err("URL must start with https:// (local dev defaults to https://api.vyrapp.pro)");
    }
    if let Err(_) = url::Url::parse(trimmed) {
        return Err("invalid URL");
    }
    Ok(trimmed)
}

/// Loopback port used purely as a single-instance lock + IPC channel for
/// `vyrapp://` deep links -- whichever process wins the bind is "the" running
/// instance; a second launch forwards its URL here and exits. Picked high
/// and specific enough that a collision with an unrelated local service is
/// unlikely.
const DEEPLINK_PORT: u16 = 47812;

/// Registers `vyrapp://` so the OS routes join deep links to this binary.
/// Best-effort: failures are logged and swallowed — must never block startup.
fn register_url_protocol() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    #[cfg(windows)]
    {
        let open_command = format!("\"{}\" \"%1\"", exe.display());
        let steps: [&[&str]; 3] = [
            &["add", r"HKCU\Software\Classes\vyrapp", "/ve", "/d", "URL:Vyr Protocol", "/f"],
            &["add", r"HKCU\Software\Classes\vyrapp", "/v", "URL Protocol", "/d", "", "/f"],
            &[
                "add",
                r"HKCU\Software\Classes\vyrapp\shell\open\command",
                "/ve",
                "/d",
                open_command.as_str(),
                "/f",
            ],
        ];
        for args in steps {
            let mut cmd = std::process::Command::new("reg.exe");
            cmd.args(args);
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
            if let Err(err) = cmd.output() {
                eprintln!("warning: failed to register vyrapp:// protocol: {err}");
                return;
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // XDG: ~/.local/share/applications/hexatalk.desktop + mime default.
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let apps = std::path::PathBuf::from(home)
            .join(".local/share/applications");
        if let Err(err) = std::fs::create_dir_all(&apps) {
            eprintln!("warning: cannot create applications dir: {err}");
            return;
        }
        let desktop_path = apps.join("hexatalk-vyrapp.desktop");
        let exe_s = exe.display().to_string();
        let body = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=HexaTalk\n\
             Comment=Open HexaTalk deep links\n\
             Exec=\"{exe}\" %u\n\
             Terminal=false\n\
             Categories=Network;InstantMessaging;\n\
             MimeType=x-scheme-handler/vyrapp;\n\
             NoDisplay=true\n\
             StartupNotify=false\n",
            exe = exe_s.replace('"', "\\\"")
        );
        if let Err(err) = std::fs::write(&desktop_path, body) {
            eprintln!("warning: failed to write .desktop for vyrapp://: {err}");
            return;
        }
        // Best-effort: register as default handler (needs xdg-utils).
        let _ = std::process::Command::new("xdg-mime")
            .args([
                "default",
                "hexatalk-vyrapp.desktop",
                "x-scheme-handler/vyrapp",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&apps)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `vyrapp://` argv, if this process was launched with one (deep-link click,
/// cold start or otherwise).
fn deep_link_arg() -> Option<String> {
    std::env::args()
        .skip(1)
        .find(|a| a.starts_with("vyrapp://"))
}

/// Single-instance gate for deep links: binds the loopback lock port. `Ok`
/// means this process is the primary instance (caller should later spawn
/// `run_deeplink_listener` on the returned listener); `Err` means another
/// instance already owns it, so this process forwards its own `vyrapp://`
/// argv (if any) over the socket and the caller should exit immediately
/// without creating a window.
fn claim_single_instance_or_forward() -> Option<std::net::TcpListener> {
    match std::net::TcpListener::bind(("127.0.0.1", DEEPLINK_PORT)) {
        Ok(listener) => Some(listener),
        Err(_) => {
            if let Some(url) = deep_link_arg() {
                if let Ok(mut stream) =
                    std::net::TcpStream::connect(("127.0.0.1", DEEPLINK_PORT))
                {
                    use std::io::Write;
                    let _ = writeln!(stream, "{url}");
                    let _ = stream.flush();
                }
            }
            None
        }
    }
}

/// Runs on a background thread for the lifetime of the primary instance:
/// accepts one connection per forwarded deep link, reads the URL line, and
/// pushes it into the update loop exactly like any other background-job
/// result (see `Message::DeepLinkReceived`).
fn run_deeplink_listener(listener: std::net::TcpListener, tx: UnboundedSender<Message>) {
    use std::io::BufRead;
    for stream in listener.incoming().flatten() {
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let url = line.trim().to_string();
            if !url.is_empty() {
                let _ = tx.send(Message::DeepLinkReceived(url));
            }
        }
    }
}

// ---------- Entry point ----------

/// One-time rebrand migration: move the per-user data dir
/// `%APPDATA%/Talkyss` to `%APPDATA%/HexaTalk` when only the legacy one
/// exists, so the session token, E2EE identity keys, ratchet state and the
/// encrypted history vault all survive the rename. Runs before anything
/// reads the new dir.
///
/// If `%APPDATA%/HexaTalk` already exists (e.g. this device ran the app
/// once right after the rebrand, before ever seeing a `Talkyss` dir to
/// migrate from — which silently generated a *fresh* peerseal identity),
/// the plain rename above never fires again, since it only renames when the
/// destination doesn't exist at all. That orphaned the real identity key
/// (and history vault, session, ...) in the legacy folder forever: every
/// group/channel key package sealed to the old identity's public key then
/// fails to unseal with the new one ("wrong identity?"). So on top of the
/// rename, do a recursive per-file merge: copy any legacy file that's
/// missing on the new side over, without ever touching a file that's
/// already there. Safe to run every launch — once both sides agree, it's a
/// no-op walk.
fn migrate_legacy_data_dir() {
    let Ok(base) = env::var("APPDATA") else {
        return;
    };
    let base = std::path::Path::new(&base);
    let legacy = base.join("Talkyss");
    let current = base.join("HexaTalk");
    if !legacy.is_dir() {
        return;
    }
    if !current.exists() {
        if std::fs::rename(&legacy, &current).is_ok() {
            return;
        }
        // Cross-device rename etc. can fail; fall through to the merge copy.
    }
    merge_missing_files(&legacy, &current);
}

/// Recursively copies every file under `src` that has no counterpart under
/// `dst` yet (by relative path), creating parent directories as needed.
/// Never overwrites an existing `dst` file.
fn merge_missing_files(src: &std::path::Path, dst: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let _ = std::fs::create_dir_all(&dst_path);
            merge_missing_files(&src_path, &dst_path);
        } else if file_type.is_file() && !dst_path.exists() {
            let _ = std::fs::copy(&src_path, &dst_path);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    register_url_protocol();

    // Must run before any UI/tokio setup: if another instance already owns
    // the lock port, this process's only job is forwarding its `vyrapp://`
    // argv (if any) and exiting -- opening a second window would be wrong
    // regardless of whether a link was actually clicked.
    let Some(deeplink_listener) = claim_single_instance_or_forward() else {
        std::process::exit(0);
    };

    migrate_legacy_data_dir();

    dotenvy::from_filename(".env.local").ok();
    dotenvy::dotenv().ok();

    // rustls 0.23 needs an explicit crypto provider for peerseal WSS relay.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Prefer a real .env.local/.env next to the exe or in the working
    // directory (handy for pointing a dev build at a different deployment
    // without rebuilding); otherwise fall back to the URL `build.rs` baked
    // (obfuscated, see src/obf.rs) into the binary at compile time, so a
    // standalone .exe copied somewhere with no .env file still knows where
    // to connect.
    // Production default is baked by build.rs (api.vyrapp.pro). Runtime
    // API_URL / .env.local only override for deliberate experiments — the
    // shipped app must talk to production.
    let deployment_url = env::var("API_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| obf::api_url().to_string());

    if deployment_url.is_empty() {
        eprintln!("Missing API_URL (baked at compile time from .env.local). Rebuild the app.");
        std::process::exit(1);
    }
    let deployment_url = match validate_api_url(&deployment_url) {
        Ok(u) => u.to_string(),
        Err(err) => {
            eprintln!("Bad API_URL: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("HexaTalk backend: {deployment_url}");

    // UI fonts are embedded at Slint compile time (see the `import "*.ttf"`
    // lines at the top of ui/main.slint) -- no runtime registration needed.
    let ui = slint_ui::AppWindow::new()?;

    // Force the initial window size explicitly instead of relying purely on
    // `preferred-width`/`preferred-height` in main.slint. Those are only a
    // hint winit applies while creating the platform window, and in a
    // release build (much faster to reach the first frame than an
    // unoptimized debug build) content has been observed rendering at the
    // wrong size -- consistent with the first paint racing ahead of that
    // initial resize. Setting the size explicitly here is a direct call
    // into the window adapter rather than a hint, so it doesn't depend on
    // that timing.
    ui.window()
        .set_size(slint::WindowSize::Logical(slint::LogicalSize::new(
            1180.0, 760.0,
        )));

    // Slint owns the main thread's event loop. A dedicated background
    // thread owns a tokio runtime and drives `App::update`/background jobs
    // exactly like iced's Elm-architecture runtime used to -- see
    // `run_pump` below and `src/rt.rs` for the `Task`/`Job` shim.
    let (tx, rx) = unbounded_channel::<Message>();
    wire_callbacks(&ui, tx.clone());

    // Same funnel for a deep link that arrives via someone else's forwarded
    // argv (see `run_deeplink_listener`) and one on this process's own cold
    // start -- both just become a `Message::DeepLinkReceived` on `tx`.
    {
        let tx = tx.clone();
        std::thread::spawn(move || run_deeplink_listener(deeplink_listener, tx));
    }
    if let Some(url) = deep_link_arg() {
        let _ = tx.send(Message::DeepLinkReceived(url));
    }

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        // A dead pump thread leaves the window alive but the app brain-dead
        // -- every UI action (including window close, which routes through
        // the pump) silently does nothing and the user has to kill the
        // process. Fail visibly instead: log and quit the event loop.
        let tokio_rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("fatal: failed to start tokio runtime: {err}");
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
                return;
            }
        };
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio_rt.block_on(run_pump(deployment_url, rx, tx, ui_weak));
        }));
        if let Err(payload) = panicked {
            eprintln!("fatal: update loop panicked: {payload:?}");
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
    });

    // `ui.run()` would end the Slint event loop as soon as the last window
    // hides -- and the whole process with it, killing the tray icon, the
    // pump thread and every subscription. `run_event_loop_until_quit()`
    // keeps the loop alive while hidden to tray; the explicit
    // `slint::quit_event_loop()` (WindowAction::Exit) still ends it.
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

/// The update loop: mirrors iced's Elm-architecture runtime. Every `Message`
/// -- from a Slint UI callback or a background job -- goes through
/// `App::update`, spawns whatever `Task` it returned, reconciles background
/// jobs against the new state, then pushes a fresh snapshot to the UI.
async fn run_pump(
    deployment_url: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
    tx: UnboundedSender<Message>,
    ui_weak: slint::Weak<slint_ui::AppWindow>,
) {
    let (mut app, boot_task) = App::new(deployment_url);
    let mut registry = SubscriptionRegistry::new();

    boot_task.spawn(&tx);
    registry.reconcile(app.subscription(tx.clone()));
    sync_ui(&app, &ui_weak);

    while let Some(message) = rx.recv().await {
        // Tick/HeartbeatFinished fire every few seconds. They used to be
        // UI-noop except toast expiry — now Tick also refreshes detected
        // app activity (profile + settings Privacy panel), so we resync
        // when those surfaces need it.
        let is_heartbeat = matches!(message, Message::Tick | Message::HeartbeatFinished);
        let toast_before = app.toast.is_some();
        let activity_before = app.current_activity.clone();
        let was_tick = matches!(message, Message::Tick);

        let task = app.update(message);
        task.spawn(&tx);
        registry.reconcile(app.subscription(tx.clone()));
        apply_window_action(&mut app, &ui_weak);

        let activity_changed = was_tick && app.current_activity != activity_before;
        let need_activity_ui = was_tick
            && (app.settings_open
                || app
                    .viewing_profile
                    .as_ref()
                    .is_some_and(|p| app.session.as_ref().is_some_and(|s| s.user_id == p.user_id)));
        if !is_heartbeat
            || app.toast.is_some() != toast_before
            || activity_changed
            || need_activity_ui
        {
            sync_ui(&app, &ui_weak);
        }
    }
}

fn apply_window_action(app: &mut App, ui_weak: &slint::Weak<slint_ui::AppWindow>) {
    let Some(action) = app.pending_window_action.take() else {
        return;
    };
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || match action {
        WindowAction::HideToTray => {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
        }
        WindowAction::ShowAndFocus => {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().show();
                // `show()` alone can leave the window minimized or buried
                // behind others on Windows -- tray "Open" should raise and
                // focus it like clicking the taskbar icon does.
                let _ = ui.window().with_winit_window(|window| {
                    window.set_minimized(false);
                    window.focus_window();
                });
            }
        }
        WindowAction::Exit => {
            let _ = slint::quit_event_loop();
        }
    });
}

/// A plain-data snapshot of whatever `App` state the UI needs, built on the
/// pump thread (where `App` lives) and applied on the Slint UI thread via
/// `invoke_from_event_loop`. Only plain owned data (`String`/`bool`/`Vec`/
/// domain structs, never `slint::Image`/generated `slint_ui::*Row` values) is
/// allowed to cross the thread boundary here -- Slint's own types aren't
/// guaranteed `Send`, so the `slint_ui::*Row` conversion (see `src/viewmodel.rs`)
/// happens inside `apply()`, which only ever runs on the Slint UI thread.
///
/// `image_cache` carries the raw avatar/attachment bytes (`Arc<[u8]>`, so
/// `Send`) fetched on the pump thread. `slint::Image` itself can't cross the
/// boundary, so decoding happens on the UI thread (see `src/img_cache.rs`).
struct UiSnapshot {
    screen: slint_ui::Screen,
    auth_mode: slint_ui::AuthMode,
    username_input: String,
    password_input: String,
    display_name_input: String,
    email_input: String,
    password_confirm_input: String,
    password_reset_code_input: String,
    password_reset_code_sent: bool,
    auth_error: String,
    auth_username_error: String,
    auth_password_error: String,
    auth_email_error: String,
    auth_busy: bool,
    email_verify_input: String,
    email_verify_code_input: String,
    email_verify_code_sent: bool,
    email_verify_error: String,
    email_verify_busy: bool,
    connect_status: String,
    app_version_line: String,
    image_cache: std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    chat: Option<ChatRaw>,
    profile: Option<ProfileRaw>,
    settings: Option<SettingsRaw>,
    server_settings: Option<ServerSettingsRaw>,
    command_palette_open: bool,
    command_palette_query: String,
    command_palette_results: Vec<String>,
    /// Ephemeral status toast (`App::toast`), rendered as a floating pill;
    /// Rust clears it itself after 3s on the Tick.
    toast: Option<String>,
    /// Full-screen attachment lightbox target (`App::attachment_preview_url`).
    attachment_preview_url: Option<String>,
    /// Scroll-to-bottom pulse for the chat message list (see
    /// CHAT_SCROLL_PULSE); filled by `sync_ui`, not `from_app`.
    chat_scroll_pulse: i32,
    /// `vyrapp://join/<slug>` confirmation dialog (`App::show_join_dialog`).
    join_dialog_open: bool,
    join_dialog_server_name: String,
    join_dialog_server_icon_url: String,
    join_dialog_invites_paused: bool,
}

struct ServerSettingsRaw {
    server: ServerSummary,
    server_icon_url: String,
    is_platform_admin: bool,
    category: ServerSettingsCategory,
    channels: Vec<ChannelSummary>,
    server_members: Vec<ServerMemberRow>,
    server_roles: Vec<ServerRoleRow>,
    my_server_permissions: u32,
    rename_server_input: String,
    custom_slug_input: String,
    server_status: Option<String>,
    server_icon_busy: bool,
    new_channel_name_input: String,
    new_channel_is_voice: bool,
    renaming_channel_id: Option<String>,
    rename_channel_input: String,
    channel_perms_channel_id: Option<String>,
    channel_perms_role_id: Option<String>,
    channel_overwrites: std::collections::HashMap<String, (u32, u32)>,
    member_role_picker_open: Option<String>,
    new_role_name_input: String,
    editing_role_id: Option<String>,
    role_name_edit_input: String,
    confirm_delete_role_id: Option<String>,
    confirm_delete_server: bool,
    invite_qr_visible: bool,
}

struct SettingsRaw {
    session: Session,
    avatar_url: String,
    pending_attachment_preview: Option<std::sync::Arc<[u8]>>,
    category: SettingsCategory,
    settings_display_name_input: String,
    settings_status_input: String,
    settings_bio_input: String,
    settings_avatar_color: String,
    settings_profile_status: Option<(String, bool)>,
    settings_current_password_input: String,
    settings_new_password_input: String,
    settings_confirm_password_input: String,
    settings_password_status: Option<(String, bool)>,
    settings_input_devices: Vec<String>,
    settings_output_devices: Vec<String>,
    settings_input_device: Option<String>,
    settings_output_device: Option<String>,
    avatar_upload_busy: bool,
    my_bots: Vec<BotSummary>,
    new_bot_name_input: String,
    bot_invite_username_input: String,
    bot_status: Option<(String, bool)>,
    bot_token_reveal: Option<String>,
    noise_gate: f32,
    ui_scale: f32,
    update_check_status: Option<String>,
    update_ready: bool,
    ping_status: Option<String>,
    plus_busy_status: Option<String>,
    plus_checkout_busy: bool,
    share_activity: bool,
    current_activity: String,
    e2ee_pad_messages: bool,
}

struct ProfileRaw {
    avatar_url: String,
    loading: bool,
    error: Option<String>,
    profile: Option<ProfileView>,
    my_user_id: Option<String>,
    friend_request_busy: bool,
    blocked: Vec<BlockedUser>,
    confirm_block_user_id: Option<String>,
    selected_server_name: Option<String>,
    member: Option<ServerMemberRow>,
    my_server_permissions: u32,
}

/// Raw (unconverted) chat-screen state, cloned out of `App` on the pump
/// thread. See `UiSnapshot` docs for why this can't hold `slint_ui::*` types.
struct ChatRaw {
    session: Session,
    my_avatar_url: String,
    peer_avatar_url: String,
    pending_attachment_preview: Option<std::sync::Arc<[u8]>>,
    /// When true, DM history uses length-padded TKR3 envelopes.
    e2ee_pad_messages: bool,
    servers: Vec<ServerSummary>,
    selected_server: Option<ServerSummary>,
    channels: Vec<ChannelSummary>,
    server_add_menu_open: bool,
    new_server_name_input: String,
    join_server_code_input: String,
    server_status: Option<String>,
    sidebar_tab: SidebarTab,
    social_stats: SocialStats,
    incoming_requests: Vec<IncomingRequest>,
    outgoing_requests: Vec<OutgoingRequest>,
    friends: Vec<Friend>,
    friends_filter: FriendsFilter,
    friends_filter_input: String,
    add_friend_input: String,
    add_friend_note: String,
    add_friend_status: Option<String>,
    friend_request_busy: bool,
    people_hits: Vec<PeopleHit>,
    suggestions: Vec<FriendSuggestion>,
    blocked: Vec<BlockedUser>,
    confirm_block_user_id: Option<String>,
    conversations: Vec<ConversationSummary>,
    new_group_open: bool,
    new_group_name_input: String,
    new_group_selected: std::collections::BTreeSet<String>,
    group_create_status: Option<String>,
    new_channel_open: bool,
    new_channel_name_input: String,
    new_channel_is_voice: bool,
    my_server_permissions: u32,
    admin_search_input: String,
    admin_status: Option<String>,
    admin_users: Vec<AdminUserRow>,
    admin_stats: Option<crate::state::types::AdminStats>,
    admin_reports: Vec<crate::state::types::MessageReport>,
    admin_reports_status: Option<String>,
    admin_ban_reason: String,
    admin_reports_filter: String,
    admin_custom_days: String,
    admin_user_detail: Option<crate::state::types::AdminUserDetail>,
    reporting_message_id: Option<String>,
    new_channel_category_id: Option<String>,
    server_categories: Vec<crate::state::types::CategorySummary>,
    active_conversation: Option<String>,
    active_conversation_kind: Option<String>,
    active_conversation_peer_id: Option<String>,
    active_peer_name: Option<String>,
    peer_connected: std::collections::HashMap<String, bool>,
    peer_status: std::collections::HashMap<String, String>,
    peer_sas: std::collections::HashMap<String, String>,
    peer_transport: std::collections::HashMap<String, String>,
    peer_remote_fp: std::collections::HashMap<String, String>,
    peer_trust_badge: std::collections::HashMap<String, crate::state::trust::TrustBadge>,
    qr_scan_active: bool,
    qr_scan_preview: Option<std::sync::Arc<[u8]>>,
    qr_scan_error: Option<String>,
    chat_ttl_seconds: std::collections::HashMap<String, i64>,
    voice_note_recording: bool,
    chat_store_enabled: bool,
    chat_store_allowed: bool,
    clear_chat_busy: bool,
    clear_chat_confirm: bool,
    active_voice_channel: Option<String>,
    room_voice_status: Option<String>,
    voice_users: Vec<VoiceUserRow>,
    voice_gains: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, f32>>>,
    messages: Vec<ChatMessage>,
    peer_live_messages: std::collections::HashMap<String, Vec<ChatMessage>>,
    message_input: String,
    mention_suggestions: Vec<String>,
    has_pending_attachment: bool,
    pending_reply: Option<(String, String, String)>,
    chat_error: Option<String>,
    editing_message_id: Option<String>,
    my_call: Option<MyCallInfo>,
    call_muted: bool,
    call_output_muted: bool,
    call_status_text: Option<String>,
    is_sharing: bool,
    share_system_audio: bool,
    remote_stream_muted: bool,
    share_stats_line: String,
    share_picker_open: bool,
    share_targets: Vec<screenshare::ShareTarget>,
    has_remote_share_frame: bool,
    /// Raw JPEG bytes of the latest remote share frame (Send-safe, decoded
    /// to `slint::Image` on the UI thread -- same pattern as `image_cache`).
    remote_share_frame: Option<std::sync::Arc<[u8]>>,
    share_view_expanded: bool,
    server_members: Vec<ServerMemberRow>,
    members_panel_width: f32,
    channel_list_width: f32,
    typing_names: Vec<String>,
    /// Header "pinned" panel: open flag + the live listPinned rows.
    pins_panel_open: bool,
    pinned_messages: Vec<ChatMessage>,
    /// Image URLs whose fetch failed (drives "[image unavailable]" rows).
    image_load_failed: std::collections::HashSet<String>,
}

impl UiSnapshot {
    fn from_app(app: &App) -> Self {
        let screen = if app.session.is_none() {
            slint_ui::Screen::Auth
        } else if app.session.as_ref().is_some_and(|s| !s.email_verified) {
            slint_ui::Screen::EmailVerify
        } else if app.viewing_profile.is_some() || app.profile_error.is_some() {
            slint_ui::Screen::Profile
        } else if app.settings_open {
            slint_ui::Screen::Settings
        } else if app.server_settings_open && app.selected_server.is_some() {
            slint_ui::Screen::ServerSettings
        } else {
            slint_ui::Screen::Chat
        };
        let server_settings = app
            .selected_server
            .as_ref()
            .map(|server| ServerSettingsRaw {
                server: server.clone(),
                server_icon_url: server.icon_url.clone(),
                is_platform_admin: app.session.as_ref().is_some_and(|s| s.is_admin),
                category: app.server_settings_category,
                channels: app.channels.clone(),
                server_members: app.server_members.clone(),
                server_roles: app.server_roles.clone(),
                my_server_permissions: app.my_server_permissions,
                rename_server_input: app.rename_server_input.clone(),
                custom_slug_input: app.custom_slug_input.clone(),
                server_status: app.server_status.clone(),
                server_icon_busy: app.server_icon_busy,
                new_channel_name_input: app.new_channel_name_input.clone(),
                new_channel_is_voice: app.new_channel_is_voice,
                renaming_channel_id: app.renaming_channel_id.clone(),
                rename_channel_input: app.rename_channel_input.clone(),
                channel_perms_channel_id: app.channel_perms_channel_id.clone(),
                channel_perms_role_id: app.channel_perms_role_id.clone(),
                channel_overwrites: app.channel_overwrites.clone(),
                member_role_picker_open: app.member_role_picker_open.clone(),
                new_role_name_input: app.new_role_name_input.clone(),
                editing_role_id: app.editing_role_id.clone(),
                role_name_edit_input: app.role_name_edit_input.clone(),
                confirm_delete_role_id: app.confirm_delete_role_id.clone(),
                confirm_delete_server: app.confirm_delete_server,
                invite_qr_visible: app.invite_qr_visible,
            });
        let settings = app.session.as_ref().map(|session| SettingsRaw {
            session: session.clone(),
            avatar_url: session.avatar_image_url.clone(),
            pending_attachment_preview: app
                .pending_attachment
                .as_ref()
                .map(|p| Arc::from(p.bytes.clone())),
            category: app.settings_category,
            settings_display_name_input: app.settings_display_name_input.clone(),
            settings_status_input: app.settings_status_input.clone(),
            settings_bio_input: app.settings_bio_input.clone(),
            settings_avatar_color: app.settings_avatar_color.clone(),
            settings_profile_status: app.settings_profile_status.clone(),
            settings_current_password_input: app.settings_current_password_input.clone(),
            settings_new_password_input: app.settings_new_password_input.clone(),
            settings_confirm_password_input: app.settings_confirm_password_input.clone(),
            settings_password_status: app.settings_password_status.clone(),
            settings_input_devices: app.settings_input_devices.clone(),
            settings_output_devices: app.settings_output_devices.clone(),
            settings_input_device: app.settings_input_device.clone(),
            settings_output_device: app.settings_output_device.clone(),
            avatar_upload_busy: app.avatar_upload_busy,
            my_bots: app.my_bots.clone(),
            new_bot_name_input: app.new_bot_name_input.clone(),
            bot_invite_username_input: app.bot_invite_username_input.clone(),
            bot_status: app.bot_status.clone(),
            bot_token_reveal: app.bot_token_reveal.clone(),
            noise_gate: f32::from_bits(app.noise_gate.load(Ordering::Relaxed)),
            ui_scale: app.ui_scale,
            update_check_status: app.update_check_status.clone(),
            update_ready: app.pending_update_path.is_some(),
            ping_status: app.ping_status.clone(),
            plus_busy_status: app.plus_busy_status.clone(),
            plus_checkout_busy: app.plus_checkout_busy,
            share_activity: app.share_activity,
            current_activity: app.current_activity.clone(),
            e2ee_pad_messages: app.e2ee_pad_messages,
        });
        let profile = app.session.as_ref().map(|session| ProfileRaw {
            avatar_url: app
                .viewing_profile
                .as_ref()
                .map(|p| p.avatar_image_url.clone())
                .unwrap_or_default(),
            loading: app.viewing_profile.is_none() && app.profile_error.is_none(),
            error: app.profile_error.clone(),
            profile: app.viewing_profile.clone(),
            my_user_id: Some(session.user_id.clone()),
            friend_request_busy: app.friend_request_busy,
            blocked: app.blocked.clone(),
            confirm_block_user_id: app.confirm_block_user_id.clone(),
            selected_server_name: app.selected_server.as_ref().map(|s| s.name.clone()),
            member: app.viewing_profile.as_ref().and_then(|p| {
                app.server_members
                    .iter()
                    .find(|m| m.user_id == p.user_id)
                    .cloned()
            }),
            my_server_permissions: app.my_server_permissions,
        });
        let chat = app.session.as_ref().map(|session| ChatRaw {
            session: session.clone(),
            my_avatar_url: session.avatar_image_url.clone(),
            peer_avatar_url: app
                .active_conversation_peer_id
                .as_ref()
                .and_then(|id| app.friends.iter().find(|f| &f.user_id == id))
                .map(|f| f.avatar_image_url.clone())
                .unwrap_or_default(),
            pending_attachment_preview: app
                .pending_attachment
                .as_ref()
                .map(|p| Arc::from(p.bytes.clone())),
            e2ee_pad_messages: app.e2ee_pad_messages,
            servers: app.servers.clone(),
            selected_server: app.selected_server.clone(),
            channels: app.channels.clone(),
            server_add_menu_open: app.server_add_menu_open,
            new_server_name_input: app.new_server_name_input.clone(),
            join_server_code_input: app.join_server_code_input.clone(),
            server_status: app.server_status.clone(),
            sidebar_tab: app.sidebar_tab,
            social_stats: app.social_stats.clone(),
            incoming_requests: app.incoming_requests.clone(),
            outgoing_requests: app.outgoing_requests.clone(),
            friends: app.friends.clone(),
            friends_filter: app.friends_filter,
            friends_filter_input: app.friends_filter_input.clone(),
            add_friend_input: app.add_friend_input.clone(),
            add_friend_note: app.add_friend_note.clone(),
            add_friend_status: app.add_friend_status.clone(),
            friend_request_busy: app.friend_request_busy,
            people_hits: app.people_hits.clone(),
            suggestions: app.suggestions.clone(),
            blocked: app.blocked.clone(),
            confirm_block_user_id: app.confirm_block_user_id.clone(),
            conversations: app.conversations.clone(),
            new_group_open: app.new_group_open,
            new_group_name_input: app.new_group_name_input.clone(),
            new_group_selected: app.new_group_selected.clone(),
            group_create_status: app.group_create_status.clone(),
            new_channel_open: app.new_channel_open,
            new_channel_name_input: app.new_channel_name_input.clone(),
            new_channel_is_voice: app.new_channel_is_voice,
            new_channel_category_id: app.new_channel_category_id.clone(),
            server_categories: app.server_categories.clone(),
            my_server_permissions: app.my_server_permissions,
            admin_search_input: app.admin_search_input.clone(),
            admin_status: app.admin_status.clone(),
            admin_users: app.admin_users.clone(),
            admin_stats: app.admin_stats.clone(),
            admin_reports: app.admin_reports.clone(),
            admin_reports_status: app.admin_reports_status.clone(),
            admin_ban_reason: app.admin_ban_reason.clone(),
            admin_reports_filter: app.admin_reports_filter.clone(),
            admin_custom_days: app.admin_custom_days.clone(),
            admin_user_detail: app.admin_user_detail.clone(),
            reporting_message_id: app.reporting_message_id.clone(),
            active_conversation: app.active_conversation.clone(),
            active_conversation_kind: app.active_conversation_kind.clone(),
            active_conversation_peer_id: app.active_conversation_peer_id.clone(),
            active_peer_name: app.active_peer_name.clone(),
            peer_connected: app.peer_connected.clone(),
            peer_status: app.peer_status.clone(),
            peer_sas: app.peer_sas.clone(),
            peer_transport: app.peer_transport.clone(),
            peer_remote_fp: app.peer_remote_fp.clone(),
            peer_trust_badge: app.peer_trust_badge.clone(),
            qr_scan_active: app.qr_scan_active,
            qr_scan_preview: app.qr_scan_preview.clone(),
            qr_scan_error: app.qr_scan_error.clone(),
            chat_ttl_seconds: app.chat_ttl_seconds.clone(),
            voice_note_recording: app.voice_note_recording,
            chat_store_enabled: app.chat_store_enabled,
            chat_store_allowed: app.chat_store_allowed,
            clear_chat_busy: app.clear_chat_busy,
            clear_chat_confirm: app.clear_chat_confirm,
            active_voice_channel: app.active_voice_channel.clone(),
            room_voice_status: app.room_voice_status.clone(),
            voice_users: app.voice_users.clone(),
            voice_gains: app.voice_gains.clone(),
            messages: app.messages.clone(),
            peer_live_messages: app.peer_live_messages.clone(),
            message_input: app.message_input.clone(),
            mention_suggestions: app.mention_suggestions.clone(),
            has_pending_attachment: app.pending_attachment.is_some(),
            pending_reply: app.pending_reply.clone(),
            chat_error: app.chat_error.clone(),
            editing_message_id: app.editing_message_id.clone(),
            my_call: app.my_call.clone(),
            call_muted: app.call_muted.load(Ordering::Relaxed),
            call_output_muted: app.call_output_muted.load(Ordering::Relaxed),
            call_status_text: app.call_status_text.clone(),
            is_sharing: app.is_sharing,
            share_system_audio: app.share_system_audio,
            remote_stream_muted: app.remote_stream_muted,
            share_stats_line: app.share_stats_line.clone(),
            share_picker_open: app.share_picker_open,
            share_targets: app.share_targets.clone(),
            has_remote_share_frame: app.remote_share_frame.is_some(),
            remote_share_frame: app.remote_share_frame.clone(),
            share_view_expanded: app.share_view_expanded,
            server_members: app.server_members.clone(),
            members_panel_width: app.members_panel_width,
            channel_list_width: app.channel_list_width,
            typing_names: app.typing_names.clone(),
            pins_panel_open: app.pins_panel_open,
            pinned_messages: app.pinned_messages.clone(),
            image_load_failed: app.avatar_image_failed.clone(),
        });
        Self {
            screen,
            auth_mode: match app.auth_mode {
                crate::state::types::AuthMode::Login => slint_ui::AuthMode::Login,
                crate::state::types::AuthMode::Register => slint_ui::AuthMode::Register,
                crate::state::types::AuthMode::ForgotPassword => slint_ui::AuthMode::ForgotPassword,
            },
            username_input: app.username_input.clone(),
            image_cache: app.avatar_image_cache.clone(),
            password_input: app.password_input.clone(),
            display_name_input: app.display_name_input.clone(),
            email_input: app.email_input.clone(),
            password_confirm_input: app.password_confirm_input.clone(),
            password_reset_code_input: app.password_reset_code_input.clone(),
            password_reset_code_sent: app.password_reset_code_sent,
            auth_error: app.auth_error.clone().unwrap_or_default(),
            auth_username_error: app.auth_username_error.clone().unwrap_or_default(),
            auth_password_error: app.auth_password_error.clone().unwrap_or_default(),
            auth_email_error: app.auth_email_error.clone().unwrap_or_default(),
            auth_busy: app.auth_busy,
            email_verify_input: app.email_verify_input.clone(),
            email_verify_code_input: app.email_verify_code_input.clone(),
            email_verify_code_sent: app.email_verify_code_sent,
            email_verify_error: app.email_verify_error.clone().unwrap_or_default(),
            email_verify_busy: app.email_verify_busy,
            connect_status: app.connect_status.clone(),
            app_version_line: format!("v{CURRENT_APP_VERSION} · E2EE · P2P CALLS"),
            chat,
            profile,
            settings,
            server_settings,
            command_palette_open: app.command_palette_open,
            command_palette_query: app.command_palette_query.clone(),
            command_palette_results: app
                .command_palette_hits
                .iter()
                .map(|(_, line, _)| line.clone())
                .collect(),
            toast: app.toast.as_ref().map(|(message, _)| message.clone()),
            attachment_preview_url: app.attachment_preview_url.clone(),
            chat_scroll_pulse: 0,
            join_dialog_open: app.show_join_dialog,
            join_dialog_server_name: app.pending_join_server_name.clone(),
            join_dialog_server_icon_url: app.pending_join_server_icon.clone(),
            join_dialog_invites_paused: app.pending_join_invites_paused,
        }
    }

    fn apply(&self, ui: &slint_ui::AppWindow) {
        ui.set_current_screen(self.screen);
        ui.set_auth_mode(self.auth_mode);
        ui.set_username_input(self.username_input.clone().into());
        ui.set_password_input(self.password_input.clone().into());
        ui.set_display_name_input(self.display_name_input.clone().into());
        ui.set_email_input(self.email_input.clone().into());
        ui.set_password_confirm_input(self.password_confirm_input.clone().into());
        ui.set_password_reset_code_input(self.password_reset_code_input.clone().into());
        ui.set_password_reset_code_sent(self.password_reset_code_sent);
        ui.set_auth_error(self.auth_error.clone().into());
        ui.set_auth_username_error(self.auth_username_error.clone().into());
        ui.set_auth_password_error(self.auth_password_error.clone().into());
        ui.set_auth_email_error(self.auth_email_error.clone().into());
        ui.set_auth_busy(self.auth_busy);
        ui.set_email_verify_input(self.email_verify_input.clone().into());
        ui.set_email_verify_code_input(self.email_verify_code_input.clone().into());
        ui.set_email_verify_code_sent(self.email_verify_code_sent);
        ui.set_email_verify_error(self.email_verify_error.clone().into());
        ui.set_email_verify_busy(self.email_verify_busy);
        ui.set_connect_status(self.connect_status.clone().into());
        ui.set_app_version_line(self.app_version_line.clone().into());
        ui.set_command_palette_open(self.command_palette_open);
        ui.set_command_palette_query(self.command_palette_query.clone().into());
        ui.set_command_palette_results(
            self.command_palette_results
                .iter()
                .map(|s| slint::SharedString::from(s.as_str()))
                .collect::<Vec<_>>()
                .as_slice()
                .into(),
        );
        ui.set_toast_text(self.toast.clone().unwrap_or_default().into());
        ui.set_attachment_preview_open(self.attachment_preview_url.is_some());
        ui.set_attachment_preview(
            self.attachment_preview_url
                .as_deref()
                .and_then(|url| img_cache::image_for(&self.image_cache, url))
                .unwrap_or_default(),
        );
        ui.set_chat_scroll_pulse(self.chat_scroll_pulse);
        ui.set_join_dialog_open(self.join_dialog_open);
        ui.set_join_dialog_server_name(self.join_dialog_server_name.clone().into());
        ui.set_join_dialog_server_initial(
            self.join_dialog_server_name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "#".to_string())
                .into(),
        );
        ui.set_join_dialog_server_icon(
            img_cache::image_for(&self.image_cache, &self.join_dialog_server_icon_url)
                .unwrap_or_default(),
        );
        ui.set_join_dialog_invites_paused(self.join_dialog_invites_paused);
        if let Some(chat) = &self.chat {
            apply_chat(chat, &self.image_cache, ui);
        }
        if let Some(profile) = &self.profile {
            apply_profile(profile, &self.image_cache, ui);
        }
        if let Some(settings) = &self.settings {
            apply_settings(settings, &self.image_cache, ui);
        }
        if let Some(server_settings) = &self.server_settings {
            apply_server_settings(server_settings, &self.image_cache, ui);
        }
    }
}

const ROLE_PERM_LABELS: [(u32, &str); 9] = [
    (PERM_VIEW_CHANNELS, "View channels"),
    (PERM_SEND_MESSAGES, "Send messages"),
    (PERM_MANAGE_CHANNELS, "Manage channels"),
    (PERM_KICK_MEMBERS, "Kick members"),
    (PERM_MANAGE_ROLES, "Manage roles"),
    (PERM_MANAGE_SERVER, "Manage server"),
    (PERM_CONNECT_VOICE, "Connect to voice"),
    (PERM_SPEAK, "Speak"),
    (crate::state::types::PERM_ANNOUNCE, "Post announcements"),
];

fn apply_server_settings(
    s: &ServerSettingsRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &slint_ui::AppWindow,
) {
    let server = &s.server;
    ui.set_ss_category(match s.category {
        ServerSettingsCategory::Overview => slint_ui::ServerSettingsCategory::Overview,
        ServerSettingsCategory::Channels => slint_ui::ServerSettingsCategory::Channels,
        ServerSettingsCategory::Members => slint_ui::ServerSettingsCategory::Members,
        ServerSettingsCategory::Roles => slint_ui::ServerSettingsCategory::Roles,
        ServerSettingsCategory::Invites => slint_ui::ServerSettingsCategory::Invites,
        ServerSettingsCategory::Danger => slint_ui::ServerSettingsCategory::Danger,
    });
    ui.set_ss_server_name(server.name.clone().into());
    ui.set_ss_server_initial(
        server
            .name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "#".to_string())
            .into(),
    );
    ui.set_ss_server_icon(img_cache::image_for(cache, &s.server_icon_url).unwrap_or_default());
    ui.set_ss_header_meta(
        format!(
            "{} channels · {} members{}",
            s.channels.len(),
            s.server_members.len(),
            if server.custom_slug.is_empty() {
                String::new()
            } else {
                format!(" · /{}", server.custom_slug)
            }
        )
        .into(),
    );
    ui.set_ss_is_owner(server.is_owner);
    ui.set_ss_is_platform_admin(s.is_platform_admin);
    ui.set_ss_server_icon_busy(s.server_icon_busy);
    ui.set_ss_rename_server_input(s.rename_server_input.clone().into());
    let status = s.server_status.clone().unwrap_or_default();
    let status_lower = status.to_lowercase();
    ui.set_ss_server_status(status.into());
    ui.set_ss_server_status_danger(
        status_lower.contains("only the")
            || status_lower.contains("failed")
            || status_lower.contains("error")
            || status_lower.contains("must"),
    );
    ui.set_ss_custom_slug_input(s.custom_slug_input.clone().into());
    ui.set_ss_custom_slug_display(server.custom_slug.clone().into());

    let can_manage_channels = s.my_server_permissions & PERM_MANAGE_CHANNELS != 0;
    let can_manage_roles = s.my_server_permissions & PERM_MANAGE_ROLES != 0;
    let can_kick = s.my_server_permissions & PERM_KICK_MEMBERS != 0;
    ui.set_ss_can_manage_channels(can_manage_channels);
    ui.set_ss_new_channel_name_input(s.new_channel_name_input.clone().into());
    ui.set_ss_new_channel_is_voice(s.new_channel_is_voice);
    ui.set_ss_can_delete_channel(s.channels.len() > 1);
    ui.set_ss_rename_channel_input(s.rename_channel_input.clone().into());

    // Movable peers: non-system channels of the same type, in list order.
    let text_movable: Vec<&ChannelSummary> = s
        .channels
        .iter()
        .filter(|c| c.channel_type != "voice" && !c.is_system && !c.is_announcement)
        .collect();
    let voice_movable: Vec<&ChannelSummary> = s
        .channels
        .iter()
        .filter(|c| c.channel_type == "voice" && !c.is_system && !c.is_announcement)
        .collect();
    ui.set_ss_channels(
        s.channels
            .iter()
            .map(|c| {
                let movable = if c.channel_type == "voice" {
                    &voice_movable
                } else {
                    &text_movable
                };
                let idx = movable
                    .iter()
                    .position(|m| m.conversation_id == c.conversation_id);
                let can_move = idx.is_some() && !c.is_system && !c.is_announcement;
                slint_ui::SSChannelRow {
                    conversation_id: c.conversation_id.clone().into(),
                    name: c.name.clone().into(),
                    is_voice: c.channel_type == "voice",
                    is_renaming: s.renaming_channel_id.as_deref()
                        == Some(c.conversation_id.as_str()),
                    is_editing_perms: s.channel_perms_channel_id.as_deref()
                        == Some(c.conversation_id.as_str()),
                    is_system: c.is_system,
                    is_announcement: c.is_announcement,
                    can_move_up: can_move && idx.map(|i| i > 0).unwrap_or(false),
                    can_move_down: can_move && idx.map(|i| i + 1 < movable.len()).unwrap_or(false),
                }
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );

    let perms_name = s
        .channel_perms_channel_id
        .as_ref()
        .and_then(|id| s.channels.iter().find(|c| &c.conversation_id == id))
        .map(|c| c.name.clone())
        .unwrap_or_default();
    ui.set_ss_channel_perms_channel_id(
        s.channel_perms_channel_id
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_ss_channel_perms_channel_name(perms_name.into());
    ui.set_ss_channel_perm_roles(
        s.server_roles
            .iter()
            .map(|r| slint_ui::SSChannelRolePick {
                role_id: r.role_id.clone().into(),
                name: r.name.clone().into(),
                selected: s.channel_perms_role_id.as_deref() == Some(r.role_id.as_str()),
                has_overwrite: s.channel_overwrites.contains_key(&r.role_id),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    let (allow, deny) = s
        .channel_perms_role_id
        .as_ref()
        .and_then(|id| s.channel_overwrites.get(id).copied())
        .unwrap_or((0, 0));
    ui.set_ss_channel_overwrite_perms(
        ROLE_PERM_LABELS
            .iter()
            .map(|(bit, label)| {
                let mode = if allow & bit != 0 {
                    1
                } else if deny & bit != 0 {
                    2
                } else {
                    0
                };
                slint_ui::SSChannelOverwritePermRow {
                    bit: *bit as i32,
                    label: (*label).into(),
                    mode,
                }
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );

    ui.set_ss_can_manage_roles(can_manage_roles);
    ui.set_ss_can_kick(can_kick);
    let assignable_roles: Vec<&ServerRoleRow> =
        s.server_roles.iter().filter(|r| r.position != 0).collect();
    ui.set_ss_members(
        s.server_members
            .iter()
            .map(|m| {
                let role_label = if m.is_owner {
                    "Owner".to_string()
                } else if m.roles.is_empty() {
                    "Member".to_string()
                } else {
                    m.roles
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                slint_ui::SSMemberRow {
                    user_id: m.user_id.clone().into(),
                    display_name: m.display_name.clone().into(),
                    username: m.username.clone().into(),
                    initial: viewmodel::initial(&m.display_name),
                    avatar_color: viewmodel::hex_color(&m.avatar_color),
                    photo: Default::default(),
                    photo_url: m.avatar_image_url.clone().into(),
                    is_owner: m.is_owner,
                    role_label: role_label.into(),
                    picker_open: s.member_role_picker_open.as_deref() == Some(m.user_id.as_str()),
                    assignable_roles: assignable_roles
                        .iter()
                        .map(|r| slint_ui::SSAssignableRole {
                            role_id: r.role_id.clone().into(),
                            name: r.name.clone().into(),
                            assigned: m.roles.iter().any(|t| t.role_id == r.role_id),
                        })
                        .collect::<Vec<_>>()
                        .as_slice()
                        .into(),
                }
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );

    ui.set_ss_new_role_name_input(s.new_role_name_input.clone().into());
    ui.set_ss_editing_role_id(s.editing_role_id.clone().unwrap_or_default().into());
    ui.set_ss_role_name_edit_input(s.role_name_edit_input.clone().into());
    ui.set_ss_roles(
        s.server_roles
            .iter()
            .map(|r| slint_ui::SSRoleRow {
                role_id: r.role_id.clone().into(),
                name: r.name.clone().into(),
                color: viewmodel::hex_color(&r.color),
                is_editing: s.editing_role_id.as_deref() == Some(r.role_id.as_str()),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    if let Some(editing) = s
        .editing_role_id
        .as_deref()
        .and_then(|id| s.server_roles.iter().find(|r| r.role_id == id))
    {
        ui.set_ss_role_edit_color(editing.color.clone().into());
        ui.set_ss_editing_role_is_default(editing.position == 0);
        ui.set_ss_role_permissions(
            ROLE_PERM_LABELS
                .iter()
                .map(|(bit, label)| slint_ui::SSPermRow {
                    bit: *bit as i32,
                    label: (*label).into(),
                    enabled: editing.permissions & bit != 0,
                })
                .collect::<Vec<_>>()
                .as_slice()
                .into(),
        );
    } else {
        ui.set_ss_role_permissions(Default::default());
        ui.set_ss_editing_role_is_default(false);
    }
    ui.set_ss_confirm_delete_role(s.confirm_delete_role_id.is_some());

    ui.set_ss_invite_code(server.invite_code.clone().into());
    ui.set_ss_invite_qr_visible(s.invite_qr_visible);
    // Rendered on this (the Slint UI) thread -- `slint::Image` isn't `Send`,
    // same constraint as `img_cache::decode`. Only computed while the panel
    // is actually open; encoding a short invite code as a QR is cheap
    // (microseconds), so no cache is needed here unlike the avatar/
    // attachment path in img_cache.rs.
    ui.set_ss_invite_qr_image(
        if s.invite_qr_visible && !server.invite_code.is_empty() {
            crate::media::qr::render_invite_qr(&format!("hexatalk://invite/{}", server.invite_code))
                .unwrap_or_default()
        } else {
            Default::default()
        },
    );
    ui.set_ss_confirm_delete_server(s.confirm_delete_server);
}

fn apply_settings(
    s: &SettingsRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &slint_ui::AppWindow,
) {
    let session = &s.session;
    ui.set_settings_category(match s.category {
        SettingsCategory::Account => slint_ui::SettingsCategory::Account,
        SettingsCategory::Privacy => slint_ui::SettingsCategory::Privacy,
        SettingsCategory::Plus => slint_ui::SettingsCategory::Plus,
        SettingsCategory::Bots => slint_ui::SettingsCategory::Bots,
        SettingsCategory::Voice => slint_ui::SettingsCategory::Voice,
        SettingsCategory::Appearance => slint_ui::SettingsCategory::Appearance,
        SettingsCategory::About => slint_ui::SettingsCategory::About,
    });
    ui.set_settings_avatar_initial(viewmodel::initial(&session.display_name));
    ui.set_settings_avatar_color(viewmodel::hex_color(&s.settings_avatar_color));
    ui.set_settings_avatar_photo(img_cache::image_for(cache, &s.avatar_url).unwrap_or_default());
    ui.set_settings_has_photo(!session.avatar_image_url.is_empty());
    ui.set_settings_avatar_upload_busy(s.avatar_upload_busy);
    ui.set_settings_display_name_input(s.settings_display_name_input.clone().into());
    ui.set_settings_status_input(s.settings_status_input.clone().into());
    ui.set_settings_bio_input(s.settings_bio_input.clone().into());
    ui.set_settings_selected_avatar_index(
        AVATAR_PALETTE
            .iter()
            .position(|c| *c == s.settings_avatar_color)
            .map(|i| i as i32)
            .unwrap_or(-1),
    );
    let (profile_status, profile_status_is_error) = s
        .settings_profile_status
        .clone()
        .unwrap_or_else(|| (String::new(), false));
    ui.set_settings_profile_status(profile_status.into());
    ui.set_settings_profile_status_is_error(profile_status_is_error);
    ui.set_settings_current_password_input(s.settings_current_password_input.clone().into());
    ui.set_settings_new_password_input(s.settings_new_password_input.clone().into());
    ui.set_settings_confirm_password_input(s.settings_confirm_password_input.clone().into());
    let (password_status, password_status_is_error) = s
        .settings_password_status
        .clone()
        .unwrap_or_else(|| (String::new(), false));
    ui.set_settings_password_status(password_status.into());
    ui.set_settings_password_status_is_error(password_status_is_error);
    let (badge_text, badge_bg, badge_fg) = if session.platform_role == "owner" {
        viewmodel::badge_for_platform_role("owner")
    } else if session.is_admin {
        viewmodel::badge_for_platform_role("admin")
    } else if session.is_moderator {
        viewmodel::badge_for_platform_role("moderator")
    } else if session.plus_active {
        (
            "PLUS".into(),
            slint::Color::from_rgb_u8(201, 162, 39),
            slint::Color::from_rgb_u8(26, 20, 0),
        )
    } else {
        viewmodel::badge_for_platform_role("user")
    };
    ui.set_settings_my_badge_text(badge_text);
    ui.set_settings_my_badge_bg(badge_bg);
    ui.set_settings_my_badge_fg(badge_fg);
    ui.set_settings_store_chat_history(session.store_chat_history);
    ui.set_settings_hide_online_status(session.hide_online_status);
    ui.set_settings_share_activity(s.share_activity);
    ui.set_settings_current_activity(s.current_activity.clone().into());
    ui.set_settings_e2ee_pad_messages(s.e2ee_pad_messages);
    ui.set_settings_friends_only_dms(session.friends_only_dms);
    ui.set_settings_discoverable(session.discoverable);
    ui.set_settings_friend_request_privacy_label(
        friend_request_privacy_label(&session.friend_request_privacy).into(),
    );
    ui.set_settings_presence_status_label(presence_label(&session.presence_status).into());
    ui.set_settings_is_staff(session.is_admin);
    ui.set_settings_new_bot_name_input(s.new_bot_name_input.clone().into());
    ui.set_settings_bot_token_reveal(s.bot_token_reveal.clone().unwrap_or_default().into());
    ui.set_settings_my_bots(
        s.my_bots
            .iter()
            .map(|b| slint_ui::BotRow {
                bot_id: b.bot_id.clone().into(),
                display_name: b.display_name.clone().into(),
                username: b.username.clone().into(),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    ui.set_settings_bot_invite_username_input(s.bot_invite_username_input.clone().into());
    let (bot_status, bot_status_is_error) = s
        .bot_status
        .clone()
        .unwrap_or_else(|| (String::new(), false));
    ui.set_settings_bot_status(bot_status.into());
    ui.set_settings_bot_status_is_error(bot_status_is_error);
    let mut input_devices = vec![slint_ui::DeviceRow {
        name: "System default".into(),
        id: "".into(),
        selected: s.settings_input_device.is_none(),
    }];
    input_devices.extend(
        s.settings_input_devices
            .iter()
            .map(|d| slint_ui::DeviceRow {
                name: crate::media::call::friendly_device_name(d).into(),
                id: d.clone().into(),
                selected: s.settings_input_device.as_deref() == Some(d.as_str()),
            }),
    );
    ui.set_settings_input_devices(input_devices.as_slice().into());
    let mut output_devices = vec![slint_ui::DeviceRow {
        name: "System default".into(),
        id: "".into(),
        selected: s.settings_output_device.is_none(),
    }];
    output_devices.extend(
        s.settings_output_devices
            .iter()
            .map(|d| slint_ui::DeviceRow {
                name: crate::media::call::friendly_device_name(d).into(),
                id: d.clone().into(),
                selected: s.settings_output_device.as_deref() == Some(d.as_str()),
            }),
    );
    ui.set_settings_output_devices(output_devices.as_slice().into());
    ui.set_settings_noise_gate(s.noise_gate);
    ui.set_settings_noise_gate_label(
        if s.noise_gate <= 0.0005 {
            "Off".to_string()
        } else {
            format!("{:.3}", s.noise_gate)
        }
        .into(),
    );
    ui.set_settings_ui_scale(s.ui_scale);
    ui.set_settings_version_line(format!("HexaTalk v{CURRENT_APP_VERSION}").into());
    ui.set_settings_vault_hint(history::vault_root_display(&session.user_id).into());
    ui.set_settings_update_check_status(s.update_check_status.clone().unwrap_or_default().into());
    ui.set_settings_update_ready(s.update_ready);
    ui.set_settings_ping_status(s.ping_status.clone().unwrap_or_default().into());
    ui.set_settings_plus_active(session.plus_active);
    ui.set_settings_plus_status_line(
        if session.plus_active {
            if session.plus_expires_at > 0 {
                let dt = chrono::DateTime::from_timestamp_millis(session.plus_expires_at)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "active".into());
                format!("Active — renews / ends around {dt}")
            } else {
                "Active".into()
            }
        } else {
            "Not subscribed — unlock cosmetic perks".into()
        }
        .into(),
    );
    ui.set_settings_plus_busy_status(s.plus_busy_status.clone().unwrap_or_default().into());
    ui.set_settings_plus_checkout_busy(s.plus_checkout_busy);
}

fn apply_profile(
    p: &ProfileRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &slint_ui::AppWindow,
) {
    ui.set_profile_loading(p.loading);
    ui.set_profile_error_text(p.error.clone().unwrap_or_default().into());
    let Some(profile) = &p.profile else {
        return;
    };
    ui.set_profile_user_id(profile.user_id.clone().into());
    ui.set_profile_username(profile.username.clone().into());
    ui.set_profile_display_name(profile.display_name.clone().into());
    ui.set_profile_initial(viewmodel::initial(&profile.display_name));
    ui.set_profile_avatar_color(viewmodel::hex_color(&profile.avatar_color));
    ui.set_profile_photo(img_cache::image_for(cache, &p.avatar_url).unwrap_or_default());
    let presence = normalize_presence(&profile.presence, profile.last_seen_at);
    let online_like = presence_is_online_like(&presence, profile.last_seen_at);
    ui.set_profile_online(online_like && presence == "online");
    ui.set_profile_idle(presence == "idle");
    ui.set_profile_dnd(presence == "dnd");
    ui.set_profile_presence_label(presence_label(&presence).into());
    ui.set_profile_status_message(profile.status_message.clone().into());
    ui.set_profile_activity(profile.activity.clone().into());
    ui.set_profile_activity_icon(profile.activity_icon.clone().into());
    ui.set_profile_bio(profile.bio.clone().into());
    ui.set_profile_is_staff(profile.is_staff);
    ui.set_profile_is_plus(profile.plus_active);
    ui.set_profile_has_banner(
        profile.plus_active && !profile.profile_banner_url.is_empty(),
    );
    ui.set_profile_banner_photo(
        img_cache::image_for(cache, &profile.profile_banner_url).unwrap_or_default(),
    );
    let viewing_self = p.my_user_id.as_deref() == Some(profile.user_id.as_str());
    ui.set_profile_show_support_dm(profile.can_support_dm && !viewing_self);
    ui.set_profile_is_friend(profile.is_friend);
    ui.set_profile_favorite(profile.favorite);
    ui.set_profile_relation(profile.relation.clone().into());
    ui.set_profile_request_id(profile.request_id.clone().into());
    ui.set_profile_friend_request_busy(p.friend_request_busy);
    ui.set_profile_can_moderate(profile.relation != "self" && !viewing_self);
    ui.set_profile_is_blocked(p.blocked.iter().any(|b| b.user_id == profile.user_id));
    ui.set_profile_confirm_block(
        p.confirm_block_user_id.as_deref() == Some(profile.user_id.as_str()),
    );
    ui.set_profile_mutual_servers_line(
        if profile.mutual_servers.is_empty() {
            String::new()
        } else {
            format!("Servers in common: {}", profile.mutual_servers.join(", "))
        }
        .into(),
    );
    if let (Some(server_name), Some(member)) = (&p.selected_server_name, &p.member) {
        ui.set_profile_has_role_info(true);
        ui.set_profile_role_section_title(format!("Role in {server_name}").into());
        ui.set_profile_role_is_owner(member.is_owner);
        ui.set_profile_role_badges(
            member
                .roles
                .iter()
                .map(|r| slint_ui::RoleTagRow {
                    name: r.name.clone().into(),
                    color: viewmodel::hex_color(&r.color),
                })
                .collect::<Vec<_>>()
                .as_slice()
                .into(),
        );
    } else {
        ui.set_profile_has_role_info(false);
    }
}

// ---- Model diff caches --------------------------------------------------
// Replacing a Slint model recreates every `for`-delegate, which resets
// TouchArea hover state (visible as list flicker) and costs layout work.
// These caches let apply_chat() skip re-setting list models whose contents
// have not actually changed since the last sync.
thread_local! {
    static CONVO_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::ConversationRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static SERVER_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::ServerRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TEXT_CHANNEL_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::ChannelRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static VOICE_CHANNEL_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::ChannelRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MSG_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::ChatMessageRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static GROUP_CANDIDATE_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::GroupCandidateRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static FRIEND_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::FriendRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MEMBER_ONLINE_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::MemberRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MEMBER_OFFLINE_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::MemberRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MEMBER_BOT_ROWS_CACHE: std::cell::RefCell<Vec<slint_ui::MemberRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn rows_eq<T>(a: &[T], b: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| eq(x, y))
}

fn convo_row_eq(a: &slint_ui::ConversationRow, b: &slint_ui::ConversationRow) -> bool {
    a.id == b.id && a.title == b.title && a.unread == b.unread && a.active == b.active
}

fn server_row_eq(a: &slint_ui::ServerRow, b: &slint_ui::ServerRow) -> bool {
    a.server_id == b.server_id
        && a.name == b.name
        && a.initial == b.initial
        && a.icon_url == b.icon_url
        && a.active == b.active
}

fn group_candidate_row_eq(
    a: &slint_ui::GroupCandidateRow,
    b: &slint_ui::GroupCandidateRow,
) -> bool {
    a.user_id == b.user_id && a.label == b.label && a.selected == b.selected
}

fn channel_row_eq(a: &slint_ui::ChannelRow, b: &slint_ui::ChannelRow) -> bool {
    // Compare every field: `muted`/`is_announcement` are folded into `label`,
    // but `can_send` and `category_id` are not -- a permission or category
    // change must still push fresh rows to the model.
    a.conversation_id == b.conversation_id
        && a.label == b.label
        && a.is_voice == b.is_voice
        && a.active == b.active
        && a.is_announcement == b.is_announcement
        && a.muted == b.muted
        && a.can_send == b.can_send
        && a.category_id == b.category_id
}

/// Header-button label for the disappearing-messages TTL cycle
/// (`Message::CycleChatTtl`). Kept as a small lookup rather than dynamic
/// formatting so the four values always match the cycle exactly.
fn ttl_label(seconds: i64) -> &'static str {
    match seconds {
        3600 => "Disappear: 1h",
        86_400 => "Disappear: 24h",
        604_800 => "Disappear: 7d",
        _ => "Disappear: off",
    }
}

fn friend_row_eq(a: &slint_ui::FriendRow, b: &slint_ui::FriendRow) -> bool {
    // `photo` is patched into the live model after the rows are set (see
    // fill_model_photos) and stays empty in both the fresh rows and the
    // cache, so it's deliberately excluded -- same convention as the other
    // row comparisons.
    a.user_id == b.user_id
        && a.label == b.label
        && a.subtitle == b.subtitle
        && a.meta == b.meta
        && a.initial == b.initial
        && a.avatar_color == b.avatar_color
        && a.photo_url == b.photo_url
        && a.online == b.online
        && a.favorite == b.favorite
        && a.trust_badge == b.trust_badge
}

fn member_row_eq(a: &slint_ui::MemberRow, b: &slint_ui::MemberRow) -> bool {
    let roles_same = a.roles.row_count() == b.roles.row_count()
        && a
            .roles
            .iter()
            .zip(b.roles.iter())
            .all(|(x, y)| x.name == y.name && x.color == y.color);
    a.user_id == b.user_id
        && a.display_name == b.display_name
        && a.initial == b.initial
        && a.avatar_color == b.avatar_color
        && a.photo_url == b.photo_url
        && a.online == b.online
        && a.is_bot == b.is_bot
        && a.is_plus == b.is_plus
        && a.badge_text == b.badge_text
        && a.badge_bg == b.badge_bg
        && a.badge_fg == b.badge_fg
        && roles_same
}

fn msg_row_eq(a: &slint_ui::ChatMessageRow, b: &slint_ui::ChatMessageRow) -> bool {
    // Images decode asynchronously on the UI thread, so compare their sizes
    // too -- otherwise a freshly loaded avatar/attachment would never push.
    let photo_same = a.author_photo.size() == b.author_photo.size();
    let att_same = a.attachment.size() == b.attachment.size();
    let reactions_same = a.reactions.row_count() == b.reactions.row_count()
        && a.reactions.iter().zip(b.reactions.iter()).all(|(x, y)| {
            x.emoji == y.emoji && x.count == y.count && x.reacted_by_me == y.reacted_by_me
        });
    a.id == b.id
        && a.author_id == b.author_id
        && a.author_name == b.author_name
        && a.author_initial == b.author_initial
        && a.author_avatar_color == b.author_avatar_color
        && a.author_photo_url == b.author_photo_url
        && a.is_bot == b.is_bot
        && a.is_plus == b.is_plus
        && a.mine == b.mine
        && a.encrypted == b.encrypted
        && a.is_call_log == b.is_call_log
        && a.grouped == b.grouped
        && a.meta == b.meta
        && a.reply_line == b.reply_line
        && a.body == b.body
        && a.body_danger == b.body_danger
        && a.has_attachment == b.has_attachment
        && a.attachment_loading == b.attachment_loading
        && a.attachment_url == b.attachment_url
        && a.can_edit == b.can_edit
        && a.can_delete == b.can_delete
        && a.can_purge == b.can_purge
        && a.can_react == b.can_react
        && a.can_report == b.can_report
        && a.reporting == b.reporting
        && a.mentions_me == b.mentions_me
        && a.mentions_everyone == b.mentions_everyone
        && a.ping_label == b.ping_label
        && photo_same
        && att_same
        && reactions_same
}

/// Pushes `rows` to `set` only when they differ from the cached copy. The
/// cache takes ownership of the fresh rows and the model is fed straight
/// from the cache, so a changed list is cloned exactly zero extra times.
fn set_rows_if_changed<T>(
    cache: &'static std::thread::LocalKey<std::cell::RefCell<Vec<T>>>,
    rows: Vec<T>,
    eq: impl Fn(&T, &T) -> bool,
    set: impl FnOnce(&[T]),
) {
    let changed = cache.with(|c| {
        let mut c = c.borrow_mut();
        if rows_eq(&c, &rows, &eq) {
            false
        } else {
            *c = rows;
            true
        }
    });
    if changed {
        cache.with(|c| set(&c.borrow()));
    }
}

/// Message-list variant of `set_rows_if_changed`: when the row count is
/// unchanged (reaction toggles, edits, delete flags, reporting highlight),
/// patch only the rows that actually differ via `set_row_data` instead of
/// replacing the whole model. A full replace recreates every delegate,
/// which resets hover state on all rows and flickers; row-level patches
/// keep untouched delegates alive.
fn set_msg_rows(ui: &slint_ui::AppWindow, rows: Vec<slint_ui::ChatMessageRow>) {
    MSG_ROWS_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if rows_eq(&c, &rows, msg_row_eq) {
            return;
        }
        let model = ui.get_chat_messages();
        if c.len() == rows.len() && model.row_count() == rows.len() {
            for (i, row) in rows.iter().enumerate() {
                if !msg_row_eq(&c[i], row) {
                    // Fresh rows are built off-thread with empty images.
                    // Keep already-decoded avatars/attachments on the live
                    // model so a reaction/edit patch doesn't flash the
                    // colored-initial fallback and reset hover.
                    let mut row = row.clone();
                    if let Some(old) = model.row_data(i) {
                        if row.author_photo_url == old.author_photo_url
                            && old.author_photo.size().width > 0
                        {
                            row.author_photo = old.author_photo;
                        }
                        if row.attachment_url == old.attachment_url
                            && old.attachment.size().width > 0
                        {
                            row.attachment = old.attachment;
                            row.attachment_loading = false;
                        }
                    }
                    model.set_row_data(i, row);
                }
            }
        } else {
            ui.set_chat_messages(rows.as_slice().into());
        }
        *c = rows;
    });
}

/// Decodes the latest remote screenshare JPEG into a `slint::Image`,
/// memoized by `Arc` identity. Holding a strong ref to the previous frame's
/// bytes guarantees that address can't be recycled for a *different* frame,
/// so `Arc::ptr_eq` is a safe same-frame check -- this avoids re-decoding
/// the same JPEG on every unrelated UI resync (typing indicators, etc.)
/// while still picking up each new frame as it arrives.
fn share_frame_image(bytes: &std::sync::Arc<[u8]>) -> Option<slint::Image> {
    use std::cell::RefCell;
    thread_local! {
        static LAST_FRAME: RefCell<Option<(std::sync::Arc<[u8]>, slint::Image)>> =
            RefCell::new(None);
    }
    LAST_FRAME.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((prev_bytes, img)) = slot.as_ref() {
            if std::sync::Arc::ptr_eq(prev_bytes, bytes) {
                return Some(img.clone());
            }
        }
        let img = img_cache::decode(bytes)?;
        *slot = Some((std::sync::Arc::clone(bytes), img.clone()));
        Some(img)
    })
}

fn apply_chat(
    c: &ChatRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &slint_ui::AppWindow,
) {
    let session = &c.session;
    let unread_count = c.conversations.iter().filter(|conv| conv.unread).count() as i32;
    let home_active = c.selected_server.is_none()
        && matches!(
            c.sidebar_tab,
            SidebarTab::Chats | SidebarTab::Friends | SidebarTab::Requests | SidebarTab::Admin
        );
    let show_admin = session.is_admin || session.is_moderator;
    let effective_tab = if c.selected_server.is_some()
        && !matches!(
            c.sidebar_tab,
            SidebarTab::Admin | SidebarTab::Friends | SidebarTab::Requests
        ) {
        SidebarTab::Servers
    } else {
        c.sidebar_tab
    };
    let tab_title = match effective_tab {
        SidebarTab::Chats => "Direct".to_string(),
        SidebarTab::Friends => "Friends".to_string(),
        SidebarTab::Requests => "Invites".to_string(),
        SidebarTab::Servers => c
            .selected_server
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Server".to_string()),
        SidebarTab::Admin => "Admin".to_string(),
    };
    let ui_tab = match effective_tab {
        SidebarTab::Chats => slint_ui::SidebarTab::Chats,
        SidebarTab::Friends => slint_ui::SidebarTab::Friends,
        SidebarTab::Requests => slint_ui::SidebarTab::Requests,
        SidebarTab::Servers => slint_ui::SidebarTab::Servers,
        SidebarTab::Admin => slint_ui::SidebarTab::Admin,
    };

    // ---- Rail ----
    ui.set_chat_home_active(home_active);
    ui.set_chat_unread_count(unread_count);
    set_rows_if_changed(
        &SERVER_ROWS_CACHE,
        viewmodel::server_rows(
            &c.servers,
            c.selected_server.as_ref().map(|s| s.server_id.as_str()),
        ),
        server_row_eq,
        |rows| ui.set_chat_servers(rows.into()),
    );
    ui.set_chat_add_menu_open(c.server_add_menu_open);
    ui.set_chat_friends_online(c.social_stats.friends_online as i32);
    ui.set_chat_friends_active(effective_tab == SidebarTab::Friends);
    ui.set_chat_incoming_count(c.incoming_requests.len() as i32);
    ui.set_chat_requests_active(effective_tab == SidebarTab::Requests);
    ui.set_chat_show_admin_tab(show_admin);
    ui.set_chat_admin_active(effective_tab == SidebarTab::Admin);

    // ---- Sidebar ----
    ui.set_chat_tab(ui_tab);
    ui.set_chat_tab_title(tab_title.into());
    ui.set_chat_selected_server(c.selected_server.is_some());
    ui.set_chat_sidebar_width(c.channel_list_width);
    ui.set_chat_new_server_name(c.new_server_name_input.clone().into());
    ui.set_chat_join_server_code(c.join_server_code_input.clone().into());
    ui.set_chat_qr_scan_active(c.qr_scan_active);
    ui.set_chat_qr_scan_error(c.qr_scan_error.clone().unwrap_or_default().into());
    ui.set_chat_qr_scan_preview(
        c.qr_scan_preview
            .as_deref()
            .and_then(img_cache::decode)
            .unwrap_or_default(),
    );
    ui.set_chat_server_status(c.server_status.clone().unwrap_or_default().into());
    ui.set_chat_new_group_open(c.new_group_open);
    ui.set_chat_new_group_name(c.new_group_name_input.clone().into());
    set_rows_if_changed(
        &GROUP_CANDIDATE_ROWS_CACHE,
        viewmodel::group_candidate_rows(&c.friends, &c.new_group_selected),
        group_candidate_row_eq,
        |rows| ui.set_chat_group_candidates(rows.into()),
    );
    ui.set_chat_group_create_status(c.group_create_status.clone().unwrap_or_default().into());
    set_rows_if_changed(
        &CONVO_ROWS_CACHE,
        viewmodel::conversation_rows(&c.conversations, c.active_conversation.as_deref()),
        convo_row_eq,
        |rows| ui.set_chat_conversations(rows.into()),
    );
    ui.set_chat_friends_summary(
        format!(
            "{} friends · {} online · {} in · {} out",
            c.social_stats.friends_total,
            c.social_stats.friends_online,
            c.social_stats.incoming_pending,
            c.social_stats.outgoing_pending
        )
        .into(),
    );
    ui.set_chat_add_friend_input(c.add_friend_input.clone().into());
    ui.set_chat_add_friend_note(c.add_friend_note.clone().into());
    ui.set_chat_add_friend_status(c.add_friend_status.clone().unwrap_or_default().into());
    ui.set_chat_friends_filter(match c.friends_filter {
        FriendsFilter::All => 0,
        FriendsFilter::Online => 1,
        FriendsFilter::Favorites => 2,
    });
    ui.set_chat_people_hits(viewmodel::people_hit_rows(&c.people_hits).as_slice().into());
    ui.set_chat_suggestions(viewmodel::suggestion_rows(&c.suggestions).as_slice().into());
    ui.set_chat_friends_filter_input(c.friends_filter_input.clone().into());
    let q = c.friends_filter_input.to_lowercase();
    let filtered_friends: Vec<Friend> = c
        .friends
        .iter()
        .filter(|f| match c.friends_filter {
            FriendsFilter::All => true,
            FriendsFilter::Online => f.is_online_like(),
            FriendsFilter::Favorites => f.favorite,
        })
        .filter(|f| {
            q.is_empty()
                || f.label().to_lowercase().contains(&q)
                || f.username.to_lowercase().contains(&q)
                || f.display_name.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();
    ui.set_chat_friends(
        viewmodel::friend_rows(&filtered_friends, &c.peer_trust_badge)
            .as_slice()
            .into(),
    );
    ui.set_chat_blocked(viewmodel::blocked_rows(&c.blocked).as_slice().into());
    ui.set_chat_confirm_block_user_id(c.confirm_block_user_id.clone().unwrap_or_default().into());
    ui.set_chat_friend_request_busy(c.friend_request_busy);
    ui.set_chat_incoming_requests(
        viewmodel::incoming_request_rows(&c.incoming_requests)
            .as_slice()
            .into(),
    );
    ui.set_chat_outgoing_requests(
        viewmodel::outgoing_request_rows(&c.outgoing_requests)
            .as_slice()
            .into(),
    );
    if let Some(server) = &c.selected_server {
        ui.set_chat_server_name(server.name.clone().into());
        ui.set_chat_server_meta(
            if server.custom_slug.is_empty() {
                format!("{} channels", c.channels.len())
            } else {
                format!("/{} · {} channels", server.custom_slug, c.channels.len())
            }
            .into(),
        );
        ui.set_chat_can_manage_server(server.is_owner || session.is_admin);
        ui.set_chat_invite_code(server.invite_code.clone().into());
    } else {
        ui.set_chat_server_name("".into());
        ui.set_chat_server_meta("".into());
        ui.set_chat_can_manage_server(false);
        ui.set_chat_invite_code("".into());
    }
    let can_manage_channels = c.my_server_permissions & PERM_MANAGE_CHANNELS != 0;
    ui.set_chat_can_manage_channels(can_manage_channels);
    ui.set_chat_new_channel_open(c.new_channel_open);
    ui.set_chat_new_channel_name(c.new_channel_name_input.clone().into());
    ui.set_chat_new_channel_is_voice(c.new_channel_is_voice);
    let sel_cat = c.new_channel_category_id.clone().unwrap_or_default();
    ui.set_chat_new_channel_category_id(sel_cat.clone().into());
    let cat_rows: Vec<slint_ui::CategoryPickRow> = c
        .server_categories
        .iter()
        .map(|cat| slint_ui::CategoryPickRow {
            category_id: cat.category_id.clone().into(),
            name: cat.name.clone().into(),
            selected: cat.category_id == sel_cat,
        })
        .collect();
    ui.set_chat_new_channel_categories(cat_rows.as_slice().into());
    set_rows_if_changed(
        &TEXT_CHANNEL_ROWS_CACHE,
        viewmodel::channel_rows(&c.channels, c.active_conversation.as_deref(), false),
        channel_row_eq,
        |rows| ui.set_chat_text_channels(rows.into()),
    );
    set_rows_if_changed(
        &VOICE_CHANNEL_ROWS_CACHE,
        viewmodel::channel_rows(&c.channels, c.active_conversation.as_deref(), true),
        channel_row_eq,
        |rows| ui.set_chat_voice_channels(rows.into()),
    );
    ui.set_chat_admin_search(c.admin_search_input.clone().into());
    ui.set_chat_admin_status(c.admin_status.clone().unwrap_or_default().into());
    ui.set_chat_admin_users(
        viewmodel::admin_user_rows(&c.admin_users, &c.admin_search_input, &session.username)
            .as_slice()
            .into(),
    );
    // Prefer live API stats; if not loaded yet, derive rough KPIs from the
    // filtered admin user list so the bar never shows stale "fake" zeros
    // while real users are already on screen.
    let stats = c.admin_stats.clone().unwrap_or_else(|| {
        let users = &c.admin_users;
        crate::state::types::AdminStats {
            total_users: users.len() as i64,
            online: 0,
            banned: users.iter().filter(|u| u.banned).count() as i64,
            staff: users
                .iter()
                .filter(|u| matches!(u.role.as_str(), "admin" | "moderator" | "owner"))
                .count() as i64,
            bots: 0,
            servers: c.servers.len() as i64,
        }
    });
    ui.set_chat_admin_total_users(stats.total_users as i32);
    ui.set_chat_admin_online(stats.online as i32);
    ui.set_chat_admin_staff(stats.staff as i32);
    ui.set_chat_admin_banned(stats.banned as i32);
    ui.set_chat_admin_bots(stats.bots as i32);
    ui.set_chat_admin_servers(stats.servers as i32);
    ui.set_chat_admin_reports(viewmodel::report_rows(&c.admin_reports).as_slice().into());
    ui.set_chat_admin_reports_status(c.admin_reports_status.clone().unwrap_or_default().into());
    ui.set_chat_admin_ban_reason(c.admin_ban_reason.clone().into());
    ui.set_chat_admin_reports_filter(c.admin_reports_filter.clone().into());
    ui.set_chat_admin_custom_days(c.admin_custom_days.clone().into());
    ui.set_chat_admin_detail(viewmodel::admin_detail_view(c.admin_user_detail.as_ref()));
    ui.set_chat_is_admin(session.is_admin);
    ui.set_chat_my_display_name(session.display_name.clone().into());
    ui.set_chat_my_initial(viewmodel::initial(&session.display_name));
    ui.set_chat_my_avatar_color(viewmodel::hex_color(&session.avatar_color));
    ui.set_chat_my_photo(img_cache::image_for(cache, &c.my_avatar_url).unwrap_or_default());
    let (badge_text, badge_bg, badge_fg) = if session.platform_role == "owner" {
        viewmodel::badge_for_platform_role("owner")
    } else if session.is_admin {
        viewmodel::badge_for_platform_role("admin")
    } else if session.is_moderator {
        viewmodel::badge_for_platform_role("moderator")
    } else if session.plus_active {
        (
            "PLUS".into(),
            slint::Color::from_rgb_u8(201, 162, 39),
            slint::Color::from_rgb_u8(26, 20, 0),
        )
    } else {
        viewmodel::badge_for_platform_role("user")
    };
    ui.set_chat_my_badge_text(badge_text);
    ui.set_chat_my_badge_bg(badge_bg);
    ui.set_chat_my_badge_fg(badge_fg);

    // ---- Chat area ----
    // The chat area only shows a conversation that actually belongs to the
    // tab currently on screen. Previously `active_conversation.is_some()` was
    // enough, so switching tabs left the old chat visible (the main screen
    // never followed the sidebar).
    let has_conversation = c.active_conversation.is_some()
        && match effective_tab {
            SidebarTab::Chats => matches!(
                c.active_conversation_kind.as_deref(),
                Some("direct") | Some("group")
            ),
            SidebarTab::Servers => matches!(
                c.active_conversation_kind.as_deref(),
                Some("channel") | Some("voice")
            ),
            SidebarTab::Friends | SidebarTab::Requests | SidebarTab::Admin => false,
        };
    ui.set_chat_has_conversation(has_conversation);
    let peer_friend = c
        .active_conversation_peer_id
        .as_ref()
        .and_then(|id| c.friends.iter().find(|f| &f.user_id == id));
    let is_channel_icon = peer_friend.is_none();
    ui.set_chat_is_channel_icon(is_channel_icon);
    ui.set_chat_peer_title(
        c.active_peer_name
            .clone()
            .unwrap_or_else(|| "Chat".into())
            .into(),
    );
    if let Some(friend) = peer_friend {
        ui.set_chat_peer_initial(viewmodel::initial(friend.label()));
        ui.set_chat_peer_avatar_color(viewmodel::hex_color(&friend.avatar_color));
        // Prefer server-computed presence (online/idle/dnd) when available;
        // fall back to last_seen window for stale subscription payloads.
        ui.set_chat_peer_online(friend.is_online_like());
        ui.set_chat_peer_photo(img_cache::image_for(cache, &c.peer_avatar_url).unwrap_or_default());
    } else {
        ui.set_chat_peer_initial("#".into());
        ui.set_chat_peer_online(false);
        ui.set_chat_peer_photo(Default::default());
    }
    let is_direct = c.active_conversation_kind.as_deref() == Some("direct");
    ui.set_chat_is_direct(is_direct);
    let cur_peer_id = c.active_conversation_peer_id.as_deref();
    let peer_connected_now = cur_peer_id
        .and_then(|id| c.peer_connected.get(id))
        .copied()
        .unwrap_or(false);
    ui.set_chat_peer_connected(peer_connected_now);
    if is_direct {
        let label = if peer_connected_now {
            let fp = cur_peer_id
                .and_then(|id| c.peer_remote_fp.get(id))
                .map(String::as_str)
                .unwrap_or("…");
            let tr = cur_peer_id
                .and_then(|id| c.peer_transport.get(id))
                .map(String::as_str)
                .unwrap_or("?");
            // Live path is Noise peerseal; history is TKR3 (+ optional pad).
            if c.e2ee_pad_messages {
                format!("E2EE · pad · peerseal · {tr} · {fp}")
            } else {
                format!("E2EE · peerseal · {tr} · {fp}")
            }
        } else {
            cur_peer_id
                .and_then(|id| c.peer_status.get(id))
                .cloned()
                .unwrap_or_else(|| "Connecting secure channel…".to_string())
        };
        ui.set_chat_connection_label(label.into());
        ui.set_chat_sas_label(
            cur_peer_id
                .and_then(|id| c.peer_sas.get(id))
                .cloned()
                .unwrap_or_default()
                .into(),
        );
        ui.set_chat_trust_badge(
            match cur_peer_id.and_then(|id| c.peer_trust_badge.get(id)) {
                Some(crate::state::trust::TrustBadge::Verified) => "verified",
                Some(crate::state::trust::TrustBadge::FingerprintChanged { .. }) => "changed",
                Some(crate::state::trust::TrustBadge::Unverified) | None => "",
            }
            .into(),
        );
    } else {
        ui.set_chat_connection_label("".into());
        ui.set_chat_sas_label("".into());
        ui.set_chat_trust_badge("".into());
    }
    ui.set_chat_voice_note_recording(c.voice_note_recording);
    ui.set_chat_ttl_label(
        c.active_conversation
            .as_deref()
            .map(|id| c.chat_ttl_seconds.get(id).copied().unwrap_or(0))
            .map(ttl_label)
            .unwrap_or("Disappear: off")
            .into(),
    );
    ui.set_chat_show_call_button(is_direct && c.my_call.is_none());
    let is_server_channel = matches!(
        c.active_conversation_kind.as_deref(),
        Some("channel") | Some("voice")
    );
    ui.set_chat_is_server_channel(is_server_channel);
    let channel_muted = c
        .active_conversation
        .as_ref()
        .and_then(|id| c.channels.iter().find(|ch| &ch.conversation_id == id))
        .map(|ch| ch.muted)
        .unwrap_or(false);
    ui.set_chat_channel_muted(channel_muted);
    ui.set_chat_store_enabled(c.chat_store_enabled);
    ui.set_chat_store_allowed(c.chat_store_allowed);
    ui.set_chat_clear_chat_busy(c.clear_chat_busy);
    ui.set_chat_clear_chat_confirm(c.clear_chat_confirm);
    let can_voice = matches!(
        c.active_conversation_kind.as_deref(),
        Some("voice") | Some("group")
    );
    ui.set_chat_can_voice(can_voice);
    let in_voice =
        c.active_voice_channel.as_deref() == c.active_conversation.as_deref() && can_voice;
    ui.set_chat_in_voice(in_voice);
    ui.set_chat_room_voice_status(c.room_voice_status.clone().unwrap_or_default().into());
    ui.set_chat_voice_users_label(
        c.voice_users
            .iter()
            .map(|u| u.display_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
            .into(),
    );
    let volume_rows: Vec<slint_ui::VoiceUserVolumeRow> = {
        let gains = c.voice_gains.lock().ok();
        c.voice_users
            .iter()
            .map(|u| slint_ui::VoiceUserVolumeRow {
                user_id: u.user_id.clone().into(),
                name: u.display_name.clone().into(),
                volume: gains
                    .as_ref()
                    .and_then(|m| m.get(&u.user_id).copied())
                    .unwrap_or(1.0),
            })
            .collect()
    };
    ui.set_chat_voice_users(volume_rows.as_slice().into());
    // Mention context for the row highlight: who "me" is, and whether
    // @everyone pings in this conversation (channels/groups, not 1:1 DMs).
    let my_mention_names: Vec<String> = {
        let mut v = vec![session.display_name.clone(), session.username.clone()];
        v.retain(|s| !s.trim().is_empty());
        v
    };
    let everyone_allowed = matches!(
        c.active_conversation_kind.as_deref(),
        Some("channel") | Some("group")
    );
    // Patch-in-place when possible: a full model replace rebuilds every
    // MessageRow delegate and resets TouchArea hover — that made the
    // floating action bar flash off on every mouse move / image decode.
    set_msg_rows(
        ui,
        viewmodel::chat_message_rows(
            &c.messages,
            c.active_conversation_peer_id
                .as_ref()
                .and_then(|id| c.peer_live_messages.get(id))
                .map(Vec::as_slice),
            &session.user_id,
            session.is_admin,
            &my_mention_names,
            everyone_allowed,
            c.reporting_message_id.as_deref(),
        ),
    );
    ui.set_chat_quick_emojis(
        QUICK_REACT_EMOJIS
            .iter()
            .map(|e| slint::SharedString::from(*e))
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    let is_editing = c.editing_message_id.is_some();
    ui.set_chat_is_editing(is_editing);
    let channel_can_send = c
        .active_conversation
        .as_ref()
        .and_then(|id| c.channels.iter().find(|ch| &ch.conversation_id == id))
        .map(|ch| ch.can_send)
        .unwrap_or(true);
    let mut placeholder = if is_editing {
        "Edit message...".to_string()
    } else if !channel_can_send {
        "Only staff can post in announcements".to_string()
    } else {
        "Type a message...".to_string()
    };
    if is_direct && !peer_connected_now {
        placeholder = "Waiting for secure channel…".to_string();
    }
    ui.set_chat_input_placeholder(placeholder.into());
    ui.set_chat_send_label(if is_editing { "Save" } else { "Send" }.into());
    let crypto_ready = (!is_direct || peer_connected_now) && channel_can_send;
    ui.set_chat_crypto_ready(crypto_ready);
    ui.set_chat_message_input(c.message_input.clone().into());
    ui.set_chat_mention_suggestions(
        c.mention_suggestions
            .iter()
            .map(|s| slint::SharedString::from(s.as_str()))
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    ui.set_chat_has_pending_attachment(c.has_pending_attachment);
    if let Some(bytes) = &c.pending_attachment_preview {
        if let Some(img) = img_cache::decode(bytes) {
            ui.set_chat_pending_attachment_preview(img);
        }
    }
    ui.set_chat_has_pending_reply(c.pending_reply.is_some());
    ui.set_chat_pending_reply_line(
        c.pending_reply
            .as_ref()
            .map(|(_, author, snippet)| format!("↩  Replying to {author}: {snippet}"))
            .unwrap_or_default()
            .into(),
    );
    ui.set_chat_typing_line(typing_label(&c.typing_names).unwrap_or_default().into());
    ui.set_chat_error_line(c.chat_error.clone().unwrap_or_default().into());
    ui.set_chat_warning_line(
        if c.chat_error.is_none() && is_direct && !peer_connected_now {
            cur_peer_id
                .and_then(|id| c.peer_status.get(id))
                .cloned()
                .unwrap_or_else(|| "Connecting secure channel…".to_string())
        } else {
            String::new()
        }
        .into(),
    );

    // ---- Members drawer ----
    let w = if c.members_panel_width < 100.0 {
        28.0
    } else {
        220.0_f32.min(c.members_panel_width.max(180.0))
    };
    ui.set_chat_members_collapsed(w <= 32.0);
    ui.set_chat_members_width(w);
    ui.set_chat_members_total(c.server_members.len() as i32);
    let online_members: Vec<ServerMemberRow> = c
        .server_members
        .iter()
        .filter(|m| !m.is_bot && is_online(m.last_seen_at))
        .cloned()
        .collect();
    let offline_members: Vec<ServerMemberRow> = c
        .server_members
        .iter()
        .filter(|m| !m.is_bot && !is_online(m.last_seen_at))
        .cloned()
        .collect();
    let bot_members: Vec<ServerMemberRow> = c
        .server_members
        .iter()
        .filter(|m| m.is_bot)
        .cloned()
        .collect();
    ui.set_chat_members_online(online_members.len() as i32);
    ui.set_chat_members_online_list(viewmodel::member_rows(&online_members).as_slice().into());
    ui.set_chat_members_offline_list(viewmodel::member_rows(&offline_members).as_slice().into());
    ui.set_chat_members_bot_list(viewmodel::member_rows(&bot_members).as_slice().into());

    // ---- Pinned-messages panel (header pin icon) ----
    ui.set_chat_pins_open(c.pins_panel_open);
    ui.set_chat_pinned_messages(viewmodel::pinned_rows(&c.pinned_messages).as_slice().into());

    // ---- User panel voice state (sidebar mic/deafen buttons) ----
    // Same Arc<AtomicBool>s the call/room-voice audio pipelines read, so
    // these reflect (and drive) the real capture/playback mute state.
    ui.set_chat_mic_muted(c.call_muted);
    ui.set_chat_deafened(c.call_output_muted);

    // ---- Call banner ----
    ui.set_banner_peer_volume(
        c.voice_gains
            .lock()
            .ok()
            .and_then(|m| m.get("*").copied())
            .unwrap_or(1.0),
    );
    if let Some(call) = &c.my_call {
        let is_ringing = call.status == "ringing";
        ui.set_chat_call_visible(true);
        ui.set_chat_call_ringing(is_ringing);
        ui.set_chat_call_incoming(is_ringing && !call.is_caller);
        ui.set_chat_call_active(call.status == "active");
        let label = match call.status.as_str() {
            "ringing" if !call.is_caller => {
                format!("Incoming call from {}", call.peer_display_name)
            }
            "ringing" => format!("Calling {}…", call.peer_display_name),
            "active" => c
                .call_status_text
                .clone()
                .unwrap_or_else(|| format!("On call with {}", call.peer_display_name)),
            _ => String::new(),
        };
        ui.set_chat_call_label(label.into());
        ui.set_chat_call_muted(c.call_muted);
        ui.set_chat_call_all_muted(c.call_muted && c.call_output_muted);
        ui.set_chat_is_sharing(c.is_sharing);
        ui.set_chat_share_picker_open(c.share_picker_open);
        ui.set_chat_share_targets(
            viewmodel::share_target_rows(&c.share_targets)
                .as_slice()
                .into(),
        );
        // The actual remote share image: decode the JPEG here on the UI
        // thread (memoized -- see below) and hand it to the banner. Without
        // this the banner showed "peer's screen" with a permanently blank
        // image, i.e. viewing a share never worked.
        let remote_frame_img = c.remote_share_frame.as_ref().and_then(share_frame_image);
        ui.set_chat_remote_frame(remote_frame_img.clone().unwrap_or_default());
        ui.set_chat_has_remote_frame(c.has_remote_share_frame && remote_frame_img.is_some());
        ui.set_chat_remote_frame_title(format!("{}'s screen", call.peer_display_name).into());
        ui.set_chat_share_expanded(c.share_view_expanded);
        ui.set_chat_share_stats_line(c.share_stats_line.clone().into());
        ui.set_chat_remote_stream_muted(c.remote_stream_muted);
        ui.set_chat_share_system_audio(c.share_system_audio);
    } else {
        ui.set_chat_call_visible(false);
        ui.set_chat_has_remote_frame(false);
        ui.set_chat_share_stats_line("".into());
    }

    // Patch avatar/attachment images now that we're on the UI thread and
    // `slint::Image` is safe to construct. Rows whose image hasn't been
    // fetched yet keep their colored-initial fallback until it arrives
    // (the next resync, triggered by `AvatarImageLoaded`, fills them in).
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_servers,
        cache,
        |r: &slint_ui::ServerRow| r.icon_url.to_string(),
        |r, img| r.icon = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_people_hits,
        cache,
        |r: &slint_ui::PeopleHitRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_suggestions,
        cache,
        |r: &slint_ui::SuggestionRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_friends,
        cache,
        |r: &slint_ui::FriendRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_incoming_requests,
        cache,
        |r: &slint_ui::IncomingRequestRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_outgoing_requests,
        cache,
        |r: &slint_ui::OutgoingRequestRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_members_online_list,
        cache,
        |r: &slint_ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_members_offline_list,
        cache,
        |r: &slint_ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        slint_ui::AppWindow::get_chat_members_bot_list,
        cache,
        |r: &slint_ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    {
        let model = ui.get_chat_messages();
        let n = model.row_count();
        for i in 0..n {
            if let Some(mut row) = model.row_data(i) {
                let mut dirty = false;
                if let Some(img) = img_cache::image_for(cache, &row.author_photo_url) {
                    // Only push when decode size changed -- set_row_data on
                    // every resync rebuilt the delegate and killed hover.
                    if row.author_photo.size() != img.size() {
                        row.author_photo = img;
                        dirty = true;
                    }
                }
                if !row.attachment_url.is_empty() && !row.is_voice_note {
                    if let Some(img) = img_cache::image_for(cache, &row.attachment_url) {
                        if row.attachment.size() != img.size() || row.attachment_loading {
                            row.attachment = img;
                            row.attachment_loading = false;
                            dirty = true;
                        }
                    } else if c.image_load_failed.contains(row.attachment_url.as_str())
                        && row.attachment_loading
                    {
                        // Fetch failed (see AvatarImageLoaded Err) -- stop
                        // showing "[loading image...]", the row falls back
                        // to "[image unavailable]".
                        row.attachment_loading = false;
                        dirty = true;
                    }
                }
                if dirty {
                    model.set_row_data(i, row);
                }
            }
        }
    }
}

// Window icon variants: the normal mark plus the "unread" one shown while
// `App::has_unread_alerts()` holds. Both are decoded lazily on the Slint UI
// thread (first access happens inside an `invoke_from_event_loop` closure)
// and cached in a thread local; the winit icon handle is applied through
// `WinitWindowAccessor` since `slint::Window` exposes no icon API.
thread_local! {
    static WINDOW_ICONS: Option<(winit::window::Icon, winit::window::Icon)> = {
        let normal = decode_window_icon(include_bytes!("../assets/textures/hexatalkicon.png"));
        let unread = decode_window_icon(include_bytes!("../assets/textures/hexatalkiconmessage.png"));
        normal.zip(unread)
    };
}

/// Decodes an embedded PNG for the window icon. Returns `None` on any
/// failure so a bad asset degrades to the exe's embedded PE icon instead of
/// panicking on the UI thread.
fn decode_window_icon(png: &[u8]) -> Option<winit::window::Icon> {
    let rgba = image::load_from_memory(png).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    winit::window::Icon::from_rgba(rgba.into_raw(), width, height).ok()
}

/// Swaps the taskbar/window icon between the normal and "unread" variants.
/// The last applied state is kept per-UI-thread so the window manager isn't
/// asked to repaint the icon on every sync, only on actual transitions.
/// Default (pre-first-alert) state stays the exe's embedded PE icon.
fn apply_window_icon(ui: &slint_ui::AppWindow, has_alerts: bool) {
    thread_local! {
        static LAST_APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if LAST_APPLIED.with(|last| last.get()) == has_alerts {
        return;
    }
    WINDOW_ICONS.with(|icons| {
        let Some((normal, unread)) = icons else {
            // Decoding failed -- the exe's PE icon stays; don't retry.
            LAST_APPLIED.with(|last| last.set(has_alerts));
            return;
        };
        let icon = if has_alerts { unread } else { normal };
        // `with_winit_window` yields `None` until the winit window exists
        // (early syncs during startup) -- only record the state once the
        // icon has actually been applied, so the next sync retries.
        let applied = ui
            .window()
            .with_winit_window(|window| window.set_window_icon(Some(icon.clone())))
            .is_some();
        if applied {
            LAST_APPLIED.with(|last| last.set(has_alerts));
        }
    });
}

fn sync_ui(app: &App, ui_weak: &slint::Weak<slint_ui::AppWindow>) {
    let mut snapshot = UiSnapshot::from_app(app);
    // Consume a pending scroll-to-bottom request: bump the pulse counter so
    // the Slint `changed chat_scroll_pulse` handler fires even for two
    // scrolls in a row.
    if CHAT_SCROLL_PENDING.swap(false, Ordering::Relaxed) {
        snapshot.chat_scroll_pulse = CHAT_SCROLL_PULSE.fetch_add(1, Ordering::Relaxed) as i32 + 1;
    }
    let has_unread_alerts = app.has_unread_alerts();
    tray::set_unread_alerts(has_unread_alerts);
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            snapshot.apply(&ui);
            apply_window_icon(&ui, has_unread_alerts);
        }
    });
}

/// Patch `photo`/`attachment` on already-set Slint list models. The rows were
/// built on the pump thread with empty images (because `slint::Image` can't
/// cross threads); here -- on the UI thread -- we decode the bytes for each
/// row's URL from the snapshot's `image_cache` and assign the result. Rows
/// whose image hasn't been fetched yet keep their colored-initial fallback.
fn fill_model_photos<T: Clone + 'static>(
    ui: &slint_ui::AppWindow,
    get: impl Fn(&slint_ui::AppWindow) -> slint::ModelRc<T>,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    url_of: impl Fn(&T) -> String,
    mut set_photo: impl FnMut(&mut T, slint::Image),
) {
    let model = get(ui);
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i) {
            if let Some(img) = img_cache::image_for(cache, &url_of(&row)) {
                set_photo(&mut row, img);
                model.set_row_data(i, row);
            }
        }
    }
}

/// Wires every Slint UI callback to send the matching `Message` into the
/// update loop -- the Slint-side equivalent of iced's `.on_press(Message::X)`.
fn wire_callbacks(ui: &slint_ui::AppWindow, tx: UnboundedSender<Message>) {
    let t = tx.clone();
    ui.on_auth_switch_mode(move |mode| {
        let mode = match mode {
            slint_ui::AuthMode::Login => crate::state::types::AuthMode::Login,
            slint_ui::AuthMode::Register => crate::state::types::AuthMode::Register,
            slint_ui::AuthMode::ForgotPassword => crate::state::types::AuthMode::ForgotPassword,
        };
        let _ = t.send(Message::SwitchAuthMode(mode));
    });

    let t = tx.clone();
    ui.on_auth_username_changed(move |text| {
        let _ = t.send(Message::UsernameInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_password_changed(move |text| {
        let _ = t.send(Message::PasswordInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_display_name_changed(move |text| {
        let _ = t.send(Message::DisplayNameInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_password_confirm_changed(move |text| {
        let _ = t.send(Message::PasswordConfirmInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_password_reset_code_changed(move |text| {
        let _ = t.send(Message::PasswordResetCodeInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_email_changed(move |text| {
        let _ = t.send(Message::EmailInputChanged(text.to_string()));
    });

    let t = tx.clone();
    ui.on_auth_submit(move || {
        let _ = t.send(Message::SubmitAuth);
    });

    let t = tx.clone();
    ui.on_auth_retry(move || {
        let _ = t.send(Message::RetryConnect);
    });

    let t = tx.clone();
    ui.on_toast_dismissed(move || {
        let _ = t.send(Message::ClearToast);
    });

    let t = tx.clone();
    ui.on_attachment_preview_close(move || {
        let _ = t.send(Message::CloseAttachmentPreview);
    });

    let t = tx.clone();
    ui.on_email_verify_email_changed(move |text| {
        let _ = t.send(Message::EmailVerifyInputChanged(text.to_string()));
    });
    let t = tx.clone();
    ui.on_email_verify_send_code(move || {
        let _ = t.send(Message::RequestEmailVerification);
    });
    let t = tx.clone();
    ui.on_email_verify_code_changed(move |text| {
        let _ = t.send(Message::EmailVerifyCodeInputChanged(text.to_string()));
    });
    let t = tx.clone();
    ui.on_email_verify_submit_code(move || {
        let _ = t.send(Message::SubmitEmailVerificationCode);
    });
    let t = tx.clone();
    ui.on_email_verify_change_email(move || {
        let _ = t.send(Message::ChangeEmailVerifyAddress);
    });
    let t = tx.clone();
    ui.on_email_verify_log_out(move || {
        let _ = t.send(Message::LogOut);
    });

    let t = tx.clone();
    ui.on_escape_pressed(move || {
        let _ = t.send(Message::EscapePressed);
    });
    let t = tx.clone();
    ui.on_open_command_palette(move || {
        let _ = t.send(Message::OpenCommandPalette);
    });
    let t = tx.clone();
    ui.on_command_palette_close(move || {
        let _ = t.send(Message::CloseCommandPalette);
    });
    let t = tx.clone();
    ui.on_command_palette_query_changed(move |q| {
        let _ = t.send(Message::CommandPaletteQueryChanged(q.to_string()));
    });
    let t = tx.clone();
    ui.on_command_palette_pick(move |i| {
        let _ = t.send(Message::CommandPalettePick(i as usize));
    });

    let t = tx.clone();
    ui.on_join_dialog_confirm(move || {
        let _ = t.send(Message::ConfirmJoinDeepLink);
    });
    let t = tx.clone();
    ui.on_join_dialog_dismiss(move || {
        let _ = t.send(Message::DismissJoinDialog);
    });

    wire_chat_callbacks(ui, &tx);
    wire_profile_callbacks(ui, &tx);
    wire_settings_callbacks(ui, &tx);
    wire_server_settings_callbacks(ui, &tx);

    let t = tx.clone();
    ui.window().on_close_requested(move || {
        let _ = t.send(Message::WindowCloseRequested);
        slint::CloseRequestResponse::KeepWindowShown
    });
}

/// Wires every `chat_*` Slint callback (rail, sidebar, chat area, members
/// drawer, call banner) to the matching `Message`. Mechanical 1:1 mapping,
/// same role as `.on_press(Message::X)` throughout the old
/// src/view/chat.rs.
fn wire_chat_callbacks(ui: &slint_ui::AppWindow, tx: &UnboundedSender<Message>) {
    macro_rules! on0 {
        ($setter:ident, $msg:expr) => {{
            let t = tx.clone();
            ui.$setter(move || {
                let _ = t.send($msg);
            });
        }};
    }
    macro_rules! on1 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a| {
                let _ = t.send($f(a));
            });
        }};
    }
    macro_rules! on2 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a, b| {
                let _ = t.send($f(a, b));
            });
        }};
    }
    macro_rules! on3 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a, b, c| {
                let _ = t.send($f(a, b, c));
            });
        }};
    }

    // ---- Rail ----
    on0!(on_chat_go_home, Message::GoHome);
    on1!(on_chat_select_server, |id: slint::SharedString| {
        Message::SelectServer(id.to_string())
    });
    on0!(on_chat_toggle_add_menu, Message::ToggleServerAddMenu);
    on0!(
        on_chat_open_friends,
        Message::SidebarTabChanged(SidebarTab::Friends)
    );
    on0!(
        on_chat_open_requests,
        Message::SidebarTabChanged(SidebarTab::Requests)
    );
    on0!(
        on_chat_open_admin,
        Message::SidebarTabChanged(SidebarTab::Admin)
    );
    on0!(
        on_chat_close_admin,
        Message::SidebarTabChanged(SidebarTab::Chats)
    );

    // ---- Sidebar: add-server / join-server ----
    on1!(on_chat_new_server_name_changed, |t: slint::SharedString| {
        Message::NewServerNameChanged(t.to_string())
    });
    on0!(on_chat_create_server, Message::CreateServer);
    on1!(
        on_chat_join_server_code_changed,
        |t: slint::SharedString| Message::JoinServerCodeChanged(t.to_string())
    );
    on0!(on_chat_join_server, Message::JoinServer);
    on0!(on_chat_start_qr_scan, Message::StartQrScan);
    on0!(on_chat_stop_qr_scan, Message::StopQrScan);

    // ---- Sidebar: Chats tab ----
    on0!(on_chat_toggle_group_panel, Message::ToggleGroupPanel);
    on1!(on_chat_group_name_changed, |t: slint::SharedString| {
        Message::GroupNameInputChanged(t.to_string())
    });
    on1!(on_chat_toggle_group_member, |id: slint::SharedString| {
        Message::ToggleGroupMember(id.to_string())
    });
    on0!(on_chat_create_group, Message::CreateGroup);
    on1!(on_chat_open_conversation, |id: slint::SharedString| {
        Message::OpenConversationDirect(id.to_string())
    });

    // ---- Sidebar: Friends tab ----
    on1!(
        on_chat_add_friend_input_changed,
        |t: slint::SharedString| Message::AddFriendInputChanged(t.to_string())
    );
    on1!(on_chat_add_friend_note_changed, |t: slint::SharedString| {
        Message::AddFriendNoteChanged(t.to_string())
    });
    on0!(on_chat_send_friend_request, Message::SendFriendRequest);
    on1!(on_chat_set_friends_filter, |i: i32| {
        Message::SetFriendsFilter(match i {
            1 => FriendsFilter::Online,
            2 => FriendsFilter::Favorites,
            _ => FriendsFilter::All,
        })
    });
    on1!(
        on_chat_friends_filter_input_changed,
        |t: slint::SharedString| Message::FriendsFilterChanged(t.to_string())
    );
    on1!(on_chat_open_profile, |id: slint::SharedString| {
        Message::OpenProfile(id.to_string())
    });
    on1!(on_chat_send_friend_request_to, |u: slint::SharedString| {
        Message::SendFriendRequestToUser(u.to_string())
    });
    on1!(on_chat_message_friend, |id: slint::SharedString| {
        Message::OpenConversationWithFriend(id.to_string())
    });
    on1!(on_chat_toggle_favorite, |id: slint::SharedString| {
        Message::ToggleFavorite(id.to_string())
    });
    on1!(on_chat_remove_friend, |id: slint::SharedString| {
        Message::RemoveFriend(id.to_string())
    });
    on1!(on_chat_confirm_block, |id: slint::SharedString| {
        Message::ConfirmBlockUser(id.to_string())
    });
    on0!(on_chat_cancel_block, Message::CancelBlockUser);
    on1!(on_chat_block_user, |id: slint::SharedString| {
        Message::BlockUser(id.to_string())
    });
    on1!(on_chat_unblock_user, |id: slint::SharedString| {
        Message::UnblockUser(id.to_string())
    });

    // ---- Sidebar: Requests tab ----
    {
        let t = tx.clone();
        ui.on_chat_respond_request(move |request_id, _from_user_id, accept| {
            let _ = t.send(Message::RespondRequest(request_id.to_string(), accept));
        });
    }
    on0!(on_chat_accept_all, Message::RespondAllIncoming(true));
    on0!(on_chat_decline_all, Message::RespondAllIncoming(false));
    on1!(on_chat_cancel_outgoing, |id: slint::SharedString| {
        Message::CancelOutgoingRequest(id.to_string())
    });

    // ---- Sidebar: Servers tab ----
    on0!(
        on_chat_toggle_server_settings,
        Message::ToggleServerSettings
    );
    on1!(on_chat_copy_invite_link, |code: slint::SharedString| {
        Message::CopyInviteLink(code.to_string())
    });
    on0!(on_chat_toggle_new_channel, Message::ToggleNewChannelInput);
    on1!(
        on_chat_new_channel_name_changed,
        |t: slint::SharedString| Message::NewChannelNameChanged(t.to_string())
    );
    on0!(on_chat_create_channel, Message::CreateChannel);
    on0!(
        on_chat_toggle_new_channel_voice,
        Message::ToggleNewChannelIsVoice
    );
    // Radio cards in the create-channel modal set the type explicitly
    // (no blind toggle from the Slint side).
    on1!(on_chat_set_new_channel_voice, |v: bool| {
        Message::NewChannelIsVoice(v)
    });
    on1!(on_chat_open_channel, |id: slint::SharedString| {
        Message::OpenChannel(id.to_string())
    });

    // ---- Sidebar: Admin tab ----
    on1!(on_chat_admin_search_changed, |t: slint::SharedString| {
        Message::AdminSearchInputChanged(t.to_string())
    });
    on2!(
        on_chat_admin_set_role,
        |id: slint::SharedString, role: slint::SharedString| {
            Message::AdminSetPlatformRole(id.to_string(), role.to_string())
        }
    );
    on2!(
        on_chat_admin_ban,
        |id: slint::SharedString, hours: i32| {
            Message::AdminSetBanned {
                user_id: id.to_string(),
                banned: true,
                duration_hours: if hours > 0 {
                    Some(hours as u32)
                } else {
                    None
                },
            }
        }
    );
    on1!(on_chat_admin_unban, |id: slint::SharedString| {
        Message::AdminSetBanned {
            user_id: id.to_string(),
            banned: false,
            duration_hours: None,
        }
    });
    on2!(
        on_chat_admin_mute,
        |id: slint::SharedString, hours: i32| {
            Message::AdminSetMuted {
                user_id: id.to_string(),
                muted: true,
                duration_hours: if hours > 0 {
                    Some(hours as u32)
                } else {
                    None
                },
            }
        }
    );
    on1!(on_chat_admin_unmute, |id: slint::SharedString| {
        Message::AdminSetMuted {
            user_id: id.to_string(),
            muted: false,
            duration_hours: None,
        }
    });
    on1!(on_chat_admin_ban_reason_changed, |t: slint::SharedString| {
        Message::AdminBanReasonChanged(t.to_string())
    });
    on1!(
        on_chat_admin_reports_filter_changed,
        |t: slint::SharedString| Message::AdminReportsFilterChanged(t.to_string())
    );
    on1!(on_chat_admin_open_user_detail, |id: slint::SharedString| {
        Message::ToggleAdminUserDetail(id.to_string())
    });
    on0!(on_chat_admin_close_user_detail, Message::CloseAdminUserDetail);
    on1!(on_chat_admin_custom_days_changed, |t: slint::SharedString| {
        Message::AdminCustomDaysChanged(t.to_string())
    });
    on1!(on_chat_admin_ban_custom_days, |id: slint::SharedString| {
        Message::AdminBanCustomDays(id.to_string())
    });
    on1!(on_chat_admin_mute_custom_days, |id: slint::SharedString| {
        Message::AdminMuteCustomDays(id.to_string())
    });
    on2!(
        on_chat_admin_grant_plus,
        |id: slint::SharedString, days: i32| {
            Message::AdminGrantPlus {
                user_id: id.to_string(),
                days: days.max(1) as u32,
            }
        }
    );
    on1!(on_chat_admin_revoke_plus, |id: slint::SharedString| {
        Message::AdminRevokePlus(id.to_string())
    });
    on1!(
        on_chat_new_channel_category_changed,
        |id: slint::SharedString| Message::NewChannelCategoryChanged(id.to_string())
    );
    on2!(
        on_chat_admin_resolve_report,
        |id: slint::SharedString, status: slint::SharedString| {
            Message::AdminResolveReport(id.to_string(), status.to_string())
        }
    );

    // ---- Sidebar: account footer + resize ----
    on0!(on_chat_open_settings, Message::OpenSettings);
    on0!(
        on_chat_sidebar_resize_started,
        Message::PanelResizeStarted(ResizePanel::ChannelList)
    );
    on1!(on_chat_sidebar_resize_moved, |x: f32| {
        Message::PanelResizeMoved(x)
    });
    on0!(on_chat_sidebar_resize_ended, Message::PanelResizeEnded);

    // ---- Chat area ----
    on0!(on_chat_start_call, Message::StartCall);
    on0!(on_chat_confirm_sas_verified, Message::ConfirmSasVerified);
    on0!(on_chat_cycle_ttl, Message::CycleChatTtl);
    on0!(on_chat_toggle_store, Message::ToggleStoreHistoryThisChat);
    on0!(on_chat_toggle_channel_mute, Message::ToggleChannelMute);
    on0!(
        on_chat_toggle_clear_confirm,
        Message::ToggleClearChatConfirm
    );
    on0!(on_chat_confirm_clear, Message::ConfirmClearChat);
    on0!(on_chat_join_voice, Message::JoinVoiceChannel);
    on0!(on_chat_leave_voice, Message::LeaveVoiceChannel);
    on0!(on_chat_toggle_pins, Message::TogglePinsPanel);
    on0!(on_chat_toggle_mute, Message::ToggleMute);
    on0!(on_chat_toggle_deafen, Message::ToggleDeafen);
    on1!(on_chat_message_input_edited, |t: slint::SharedString| {
        Message::MessageInputChanged(t.to_string())
    });
    on1!(on_chat_mention_pick, |name: slint::SharedString| {
        Message::MentionSuggestionPicked(name.to_string())
    });
    on0!(on_chat_send, Message::SendMessage);
    on0!(on_chat_pick_attachment, Message::PickAttachmentImage);
    on0!(on_chat_start_voice_note, Message::VoiceNoteRecordStart);
    on0!(on_chat_stop_voice_note, Message::VoiceNoteRecordStop);
    on0!(on_chat_remove_attachment, Message::RemovePendingAttachment);
    on0!(on_chat_cancel_edit, Message::CancelEdit);
    on0!(on_chat_cancel_reply, Message::CancelReply);
    on2!(
        on_chat_react,
        |id: slint::SharedString, emoji: slint::SharedString| {
            Message::ToggleReaction(id.to_string(), emoji.to_string())
        }
    );
    on3!(
        on_chat_reply,
        |id: slint::SharedString, author: slint::SharedString, snippet: slint::SharedString| {
            Message::ReplyToMessage(id.to_string(), author.to_string(), snippet.to_string())
        }
    );
    on1!(on_chat_copy, |t: slint::SharedString| Message::CopyMessage(
        t.to_string()
    ));
    on3!(on_chat_edit, |id: slint::SharedString,
                        body: slint::SharedString,
                        enc: bool| {
        Message::EditMessage(id.to_string(), body.to_string(), enc)
    });
    on1!(on_chat_delete, |id: slint::SharedString| {
        Message::DeleteMessage(id.to_string())
    });
    on1!(on_chat_purge, |id: slint::SharedString| {
        Message::PurgeMessage(id.to_string())
    });
    on1!(on_chat_open_attachment, |url: slint::SharedString| {
        Message::OpenAttachmentPreview(url.to_string())
    });
    on1!(on_chat_play_voice_note, |url: slint::SharedString| {
        Message::PlayVoiceNoteAttachment(url.to_string())
    });
    on1!(on_chat_report, |id: slint::SharedString| {
        Message::ArmReportMessage(id.to_string())
    });
    on3!(
        on_chat_report_reason,
        |id: slint::SharedString, body: slint::SharedString, reason: slint::SharedString| {
            Message::SubmitMessageReport(id.to_string(), body.to_string(), reason.to_string())
        }
    );
    on1!(on_chat_report_cancel, |_id: slint::SharedString| {
        Message::CancelReportMessage
    });
    on2!(
        on_chat_voice_volume_changed,
        |id: slint::SharedString, v: f32| Message::VoiceVolumeChanged(id.to_string(), v)
    );

    // ---- Members drawer ----
    on0!(on_chat_toggle_members, Message::ToggleMembersPanel);
    on0!(
        on_chat_members_resize_started,
        Message::PanelResizeStarted(ResizePanel::Members)
    );
    on1!(on_chat_members_resize_moved, |x: f32| {
        Message::PanelResizeMoved(x)
    });
    on0!(on_chat_members_resize_ended, Message::PanelResizeEnded);

    // ---- Call banner ----
    on0!(on_chat_call_accept, Message::AcceptCall);
    on0!(on_chat_call_decline, Message::DeclineCall);
    on0!(on_chat_call_hang_up, Message::HangUp);
    on0!(on_chat_call_toggle_mute, Message::ToggleMute);
    on0!(on_chat_call_toggle_mute_all, Message::ToggleMuteAll);
    on0!(on_chat_toggle_share_picker, Message::ToggleSharePicker);
    on0!(on_chat_stop_share, Message::StopShare);
    on1!(on_chat_start_share, |id: slint::SharedString| {
        Message::StartShare(id.to_string())
    });
    on0!(on_chat_toggle_share_size, Message::ToggleShareViewSize);
    on0!(on_chat_toggle_stream_mute, Message::ToggleStreamMute);
    on0!(
        on_chat_toggle_share_system_audio,
        Message::ToggleShareSystemAudio
    );
}

/// Wires the `profile_*` Slint callbacks -- port of src/view/profile.rs's
/// button handlers.
fn wire_profile_callbacks(ui: &slint_ui::AppWindow, tx: &UnboundedSender<Message>) {
    macro_rules! on0 {
        ($setter:ident, $msg:expr) => {{
            let t = tx.clone();
            ui.$setter(move || {
                let _ = t.send($msg);
            });
        }};
    }
    macro_rules! on1 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a| {
                let _ = t.send($f(a));
            });
        }};
    }

    on0!(on_profile_back, Message::CloseProfile);
    on1!(on_profile_support_dm, |id: slint::SharedString| {
        Message::OpenSupportDm(id.to_string())
    });
    on1!(on_profile_message_friend, |id: slint::SharedString| {
        Message::OpenConversationWithFriend(id.to_string())
    });
    on1!(on_profile_toggle_favorite, |id: slint::SharedString| {
        Message::ToggleFavorite(id.to_string())
    });
    on1!(on_profile_respond_request, |id: slint::SharedString| {
        Message::RespondRequest(id.to_string(), true)
    });
    on1!(on_profile_send_friend_request, |u: slint::SharedString| {
        Message::SendFriendRequestToUser(u.to_string())
    });
    on1!(on_profile_unblock, |id: slint::SharedString| {
        Message::UnblockUser(id.to_string())
    });
    on1!(on_profile_confirm_block_click, |id: slint::SharedString| {
        Message::ConfirmBlockUser(id.to_string())
    });
    on1!(on_profile_block, |id: slint::SharedString| {
        Message::BlockUser(id.to_string())
    });
    on0!(on_profile_cancel_block, Message::CancelBlockUser);
}

/// Wires the `settings_*` Slint callbacks -- port of src/view/settings.rs's
/// button handlers.
fn wire_settings_callbacks(ui: &slint_ui::AppWindow, tx: &UnboundedSender<Message>) {
    macro_rules! on0 {
        ($setter:ident, $msg:expr) => {{
            let t = tx.clone();
            ui.$setter(move || {
                let _ = t.send($msg);
            });
        }};
    }
    macro_rules! on1 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a| {
                let _ = t.send($f(a));
            });
        }};
    }

    on0!(on_settings_close, Message::CloseSettings);
    {
        let t = tx.clone();
        ui.on_settings_category_changed(move |cat| {
            let cat = match cat {
                slint_ui::SettingsCategory::Account => SettingsCategory::Account,
                slint_ui::SettingsCategory::Privacy => SettingsCategory::Privacy,
                slint_ui::SettingsCategory::Plus => SettingsCategory::Plus,
                slint_ui::SettingsCategory::Bots => SettingsCategory::Bots,
                slint_ui::SettingsCategory::Voice => SettingsCategory::Voice,
                slint_ui::SettingsCategory::Appearance => SettingsCategory::Appearance,
                slint_ui::SettingsCategory::About => SettingsCategory::About,
            };
            let _ = t.send(Message::SettingsCategoryChanged(cat));
        });
    }
    on0!(on_settings_pick_avatar, Message::PickAvatarImage);
    on0!(on_settings_remove_avatar, Message::RemoveAvatarImage);
    on1!(
        on_settings_display_name_changed,
        |t: slint::SharedString| Message::SettingsDisplayNameChanged(t.to_string())
    );
    on1!(on_settings_status_changed, |t: slint::SharedString| {
        Message::SettingsStatusChanged(t.to_string())
    });
    on1!(on_settings_bio_changed, |t: slint::SharedString| {
        Message::SettingsBioChanged(t.to_string())
    });
    on1!(
        on_settings_avatar_color_selected,
        |c: slint::SharedString| Message::SettingsAvatarColorSelected(c.to_string())
    );
    on0!(on_settings_save_profile, Message::SaveProfile);
    on1!(
        on_settings_current_password_changed,
        |t: slint::SharedString| Message::SettingsCurrentPasswordChanged(t.to_string())
    );
    on1!(
        on_settings_new_password_changed,
        |t: slint::SharedString| Message::SettingsNewPasswordChanged(t.to_string())
    );
    on1!(
        on_settings_confirm_password_changed,
        |t: slint::SharedString| Message::SettingsConfirmPasswordChanged(t.to_string())
    );
    on0!(on_settings_change_password, Message::ChangePassword);
    on0!(on_settings_log_out, Message::LogOut);
    on0!(
        on_settings_toggle_store_history,
        Message::ToggleStoreHistoryGlobal
    );
    on0!(on_settings_toggle_hide_online, Message::ToggleHideOnline);
    on0!(on_settings_toggle_share_activity, Message::ToggleShareActivity);
    on0!(
        on_settings_toggle_e2ee_pad_messages,
        Message::ToggleE2eePadMessages
    );
    on0!(
        on_settings_toggle_friends_only_dms,
        Message::ToggleFriendsOnlyDms
    );
    on0!(on_settings_toggle_discoverable, Message::ToggleDiscoverable);
    on0!(
        on_settings_cycle_friend_request_privacy,
        Message::CycleFriendRequestPrivacy
    );
    on0!(on_settings_cycle_presence, Message::CyclePresenceStatus);
    on0!(on_settings_sign_out_others, Message::SignOutOtherSessions);
    on1!(
        on_settings_new_bot_name_changed,
        |t: slint::SharedString| Message::NewBotNameChanged(t.to_string())
    );
    on0!(on_settings_create_bot, Message::CreateBot);
    on0!(on_settings_refresh_bots, Message::RefreshMyBots);
    on1!(on_settings_copy_token, |t: slint::SharedString| {
        Message::CopyMessage(t.to_string())
    });
    on0!(on_settings_dismiss_token, Message::DismissBotToken);
    on1!(
        on_settings_regenerate_bot_token,
        |id: slint::SharedString| Message::RegenerateBotToken(id.to_string())
    );
    on1!(on_settings_delete_bot, |id: slint::SharedString| {
        Message::DeleteBot(id.to_string())
    });
    on1!(
        on_settings_bot_invite_username_changed,
        |t: slint::SharedString| Message::BotInviteUsernameChanged(t.to_string())
    );
    on0!(on_settings_invite_bot, Message::InviteBotToServer);
    on1!(
        on_settings_input_device_selected,
        |d: slint::SharedString| Message::SettingsInputDeviceSelected(d.to_string())
    );
    on1!(
        on_settings_output_device_selected,
        |d: slint::SharedString| Message::SettingsOutputDeviceSelected(d.to_string())
    );
    on1!(on_settings_noise_gate_changed, |v: f32| {
        Message::NoiseGateChanged(v)
    });
    on1!(on_settings_ui_scale_changed, |v: f32| {
        Message::UiScaleChanged(v)
    });
    on0!(on_settings_check_for_update, Message::CheckForUpdate);
    on0!(on_settings_restart_update, Message::RestartAndUpdate);
    on0!(on_settings_measure_ping, Message::MeasurePing);
    on0!(on_settings_plus_subscribe, Message::PlusSubscribe);
    on0!(on_settings_plus_manage_billing, Message::PlusManageBilling);
    on0!(on_settings_plus_refresh, Message::PlusRefreshStatus);
}

/// Wires the `ss_*` (server settings) Slint callbacks -- port of
/// src/view/server_settings.rs's button handlers.
fn wire_server_settings_callbacks(ui: &slint_ui::AppWindow, tx: &UnboundedSender<Message>) {
    macro_rules! on0 {
        ($setter:ident, $msg:expr) => {{
            let t = tx.clone();
            ui.$setter(move || {
                let _ = t.send($msg);
            });
        }};
    }
    macro_rules! on1 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a| {
                let _ = t.send($f(a));
            });
        }};
    }
    macro_rules! on2 {
        ($setter:ident, $f:expr) => {{
            let t = tx.clone();
            ui.$setter(move |a, b| {
                let _ = t.send($f(a, b));
            });
        }};
    }

    on0!(on_ss_back, Message::ToggleServerSettings);
    {
        let t = tx.clone();
        ui.on_ss_category_changed(move |cat| {
            let cat = match cat {
                slint_ui::ServerSettingsCategory::Overview => ServerSettingsCategory::Overview,
                slint_ui::ServerSettingsCategory::Channels => ServerSettingsCategory::Channels,
                slint_ui::ServerSettingsCategory::Members => ServerSettingsCategory::Members,
                slint_ui::ServerSettingsCategory::Roles => ServerSettingsCategory::Roles,
                slint_ui::ServerSettingsCategory::Invites => ServerSettingsCategory::Invites,
                slint_ui::ServerSettingsCategory::Danger => ServerSettingsCategory::Danger,
            };
            let _ = t.send(Message::ServerSettingsCategoryChanged(cat));
        });
    }
    on0!(on_ss_pick_icon, Message::PickServerIcon);
    on0!(on_ss_remove_icon, Message::RemoveServerIcon);
    on1!(
        on_ss_rename_server_input_changed,
        |t: slint::SharedString| Message::RenameServerInputChanged(t.to_string())
    );
    on0!(on_ss_rename_server, Message::RenameServer);
    on1!(on_ss_custom_slug_changed, |t: slint::SharedString| {
        Message::CustomSlugInputChanged(t.to_string())
    });
    on0!(on_ss_save_custom_slug, Message::SaveCustomSlug);
    on0!(on_ss_clear_custom_slug, Message::ClearCustomSlug);
    on1!(on_ss_new_channel_name_changed, |t: slint::SharedString| {
        Message::NewChannelNameChanged(t.to_string())
    });
    on0!(
        on_ss_toggle_new_channel_voice,
        Message::ToggleNewChannelIsVoice
    );
    on0!(on_ss_create_channel, Message::CreateChannel);
    on2!(
        on_ss_start_rename_channel,
        |id: slint::SharedString, name: slint::SharedString| {
            Message::StartRenameChannel(id.to_string(), name.to_string())
        }
    );
    on1!(
        on_ss_rename_channel_input_changed,
        |t: slint::SharedString| Message::RenameChannelInputChanged(t.to_string())
    );
    on0!(on_ss_save_rename_channel, Message::RenameChannel);
    on0!(on_ss_cancel_rename_channel, Message::CancelRenameChannel);
    on1!(on_ss_delete_channel, |id: slint::SharedString| {
        Message::DeleteChannel(id.to_string())
    });
    on1!(on_ss_move_channel_up, |id: slint::SharedString| {
        Message::MoveChannelUp(id.to_string())
    });
    on1!(on_ss_move_channel_down, |id: slint::SharedString| {
        Message::MoveChannelDown(id.to_string())
    });
    on1!(on_ss_edit_channel_perms, |id: slint::SharedString| {
        Message::EditChannelPerms(id.to_string())
    });
    on0!(on_ss_close_channel_perms, Message::CloseChannelPerms);
    on1!(on_ss_select_channel_perm_role, |id: slint::SharedString| {
        Message::SelectChannelPermRole(id.to_string())
    });
    {
        let t = tx.clone();
        ui.on_ss_cycle_channel_overwrite_perm(move |bit| {
            let _ = t.send(Message::CycleChannelOverwritePerm(bit as u32));
        });
    }
    on1!(on_ss_toggle_member_picker, |id: slint::SharedString| {
        Message::ToggleMemberRolePicker(id.to_string())
    });
    on2!(
        on_ss_toggle_member_role,
        |uid: slint::SharedString, rid: slint::SharedString| {
            Message::ToggleMemberRole(uid.to_string(), rid.to_string())
        }
    );
    on1!(on_ss_kick_member, |id: slint::SharedString| {
        Message::KickMember(id.to_string())
    });
    on2!(
        on_ss_mute_member,
        |id: slint::SharedString, hours: i32| {
            Message::MuteServerMember {
                user_id: id.to_string(),
                duration_hours: hours.max(1) as u32,
            }
        }
    );
    on1!(on_ss_unmute_member, |id: slint::SharedString| {
        Message::UnmuteServerMember(id.to_string())
    });
    on1!(on_ss_open_profile, |id: slint::SharedString| {
        Message::OpenProfile(id.to_string())
    });
    on1!(on_ss_new_role_name_changed, |t: slint::SharedString| {
        Message::NewRoleNameChanged(t.to_string())
    });
    on0!(on_ss_create_role, Message::CreateRole);
    on1!(on_ss_select_role_for_edit, |id: slint::SharedString| {
        Message::SelectRoleForEdit(id.to_string())
    });
    on0!(on_ss_close_role_editor, Message::CloseRoleEditor);
    on1!(on_ss_role_name_edit_changed, |t: slint::SharedString| {
        Message::RoleNameEditChanged(t.to_string())
    });
    on0!(on_ss_save_role_name, Message::SaveRoleName);
    on2!(
        on_ss_set_role_color,
        |id: slint::SharedString, hex: slint::SharedString| {
            Message::SetRoleColor(id.to_string(), hex.to_string())
        }
    );
    {
        let t = tx.clone();
        ui.on_ss_toggle_role_permission(move |id, bit| {
            let _ = t.send(Message::ToggleRolePermission(id.to_string(), bit as u32));
        });
    }
    on1!(
        on_ss_confirm_delete_role_click,
        |id: slint::SharedString| Message::ConfirmDeleteRole(id.to_string())
    );
    on0!(on_ss_cancel_delete_role, Message::CancelDeleteRole);
    on1!(on_ss_delete_role, |id: slint::SharedString| {
        Message::DeleteRole(id.to_string())
    });
    on1!(on_ss_copy_invite_code, |code: slint::SharedString| {
        Message::CopyInviteCode(code.to_string())
    });
    on1!(on_ss_copy_invite_link, |code: slint::SharedString| {
        Message::CopyInviteLink(code.to_string())
    });
    on0!(on_ss_regenerate_invite_code, Message::RegenerateInviteCode);
    on0!(on_ss_show_invite_qr, Message::SetInviteQrVisible(true));
    on0!(on_ss_hide_invite_qr, Message::SetInviteQrVisible(false));
    on0!(
        on_ss_toggle_confirm_delete_server,
        Message::ToggleConfirmDeleteServer
    );
    on0!(on_ss_delete_server, Message::DeleteServer);
}
