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
mod rt;
mod screenshare;
mod session_store;
mod subscriptions;
mod tray;
mod types;
mod update;
mod update_check;
mod viewmodel;
mod img_cache;
mod utils;

// Re-exported at crate root so every module can keep writing `use crate::*;`
// regardless of which file a given type/message-variant physically lives in.
pub(crate) use app::App;
pub(crate) use convex_parse::*;
pub(crate) use message::Message;
pub(crate) use notify::*;
pub(crate) use session_store::*;
pub(crate) use subscriptions::*;
pub(crate) use types::*;
pub(crate) use update_check::*;
pub(crate) use utils::*;

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rt::{SubscriptionRegistry, Task, WindowAction};
use slint::Model;
use slint::ComponentHandle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

// Generated Slint bindings, kept in their own module (not glob-exported at
// crate root) so `ui::AuthMode`/`ui::Screen` never collide with the
// business-logic types of the same name in `crate::types`.
mod ui {
    slint::include_modules!();
}

pub(crate) const ONLINE_THRESHOLD_MS: f64 = 15_000.0;

/// Set by `scroll_chat_to_bottom()`, consumed (and cleared) by the chat
/// screen's UI sync step, which pulses the message list's scroll-to-end.
pub(crate) static CHAT_SCROLL_PENDING: AtomicBool = AtomicBool::new(false);

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
const PEER_CLEAR_HISTORY_CTRL: &str = "\u{001e}TALKYSS_CLEAR_HISTORY\u{001e}";

/// Defensive cap on how many background peerseal sessions run at once (one
/// per online friend) — bounds concurrent Noise/relay connections for
/// accounts with very large friends lists.
const MAX_BACKGROUND_PEER_SESSIONS: usize = 25;

// ---------- Entry point ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        eprintln!("Missing CONVEX_URL in .env.local. Run `npx convex dev` and rebuild.");
        std::process::exit(1);
    }

    // UI fonts are embedded at Slint compile time (see the `import "*.ttf"`
    // lines at the top of ui/main.slint) -- no runtime registration needed.
    let ui = ui::AppWindow::new()?;

    // Force the initial window size explicitly instead of relying purely on
    // `preferred-width`/`preferred-height` in main.slint. Those are only a
    // hint winit applies while creating the platform window, and in a
    // release build (much faster to reach the first frame than an
    // unoptimized debug build) content has been observed rendering at the
    // wrong size -- consistent with the first paint racing ahead of that
    // initial resize. Setting the size explicitly here is a direct call
    // into the window adapter rather than a hint, so it doesn't depend on
    // that timing.
    ui.window().set_size(slint::WindowSize::Logical(slint::LogicalSize::new(1180.0, 760.0)));

    // Slint owns the main thread's event loop. A dedicated background
    // thread owns a tokio runtime and drives `App::update`/background jobs
    // exactly like iced's Elm-architecture runtime used to -- see
    // `run_pump` below and `src/rt.rs` for the `Task`/`Job` shim.
    let (tx, rx) = unbounded_channel::<Message>();
    wire_callbacks(&ui, tx.clone());

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        let tokio_rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        tokio_rt.block_on(run_pump(deployment_url, rx, tx, ui_weak));
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
    ui_weak: slint::Weak<ui::AppWindow>,
) {
    let (mut app, boot_task) = App::new(deployment_url);
    let mut registry = SubscriptionRegistry::new();

    boot_task.spawn(&tx);
    registry.reconcile(app.subscription(tx.clone()));
    sync_ui(&app, &ui_weak);

    while let Some(message) = rx.recv().await {
        let task = app.update(message);
        task.spawn(&tx);
        registry.reconcile(app.subscription(tx.clone()));
        apply_window_action(&mut app, &ui_weak);
        sync_ui(&app, &ui_weak);
    }
}

fn apply_window_action(app: &mut App, ui_weak: &slint::Weak<ui::AppWindow>) {
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
/// domain structs, never `slint::Image`/generated `ui::*Row` values) is
/// allowed to cross the thread boundary here -- Slint's own types aren't
/// guaranteed `Send`, so the `ui::*Row` conversion (see `src/viewmodel.rs`)
/// happens inside `apply()`, which only ever runs on the Slint UI thread.
///
/// `image_cache` carries the raw avatar/attachment bytes (`Arc<[u8]>`, so
/// `Send`) fetched on the pump thread. `slint::Image` itself can't cross the
/// boundary, so decoding happens on the UI thread (see `src/img_cache.rs`).
struct UiSnapshot {
    screen: ui::Screen,
    auth_mode: ui::AuthMode,
    username_input: String,
    password_input: String,
    display_name_input: String,
    auth_error: String,
    auth_busy: bool,
    connect_status: String,
    app_version_line: String,
    image_cache: std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    chat: Option<ChatRaw>,
    profile: Option<ProfileRaw>,
    settings: Option<SettingsRaw>,
    server_settings: Option<ServerSettingsRaw>,
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
    member_role_picker_open: Option<String>,
    new_role_name_input: String,
    editing_role_id: Option<String>,
    role_name_edit_input: String,
    confirm_delete_role_id: Option<String>,
    confirm_delete_server: bool,
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
    settings_profile_status: Option<String>,
    settings_current_password_input: String,
    settings_new_password_input: String,
    settings_confirm_password_input: String,
    settings_password_status: Option<String>,
    settings_input_devices: Vec<String>,
    settings_output_devices: Vec<String>,
    settings_input_device: Option<String>,
    settings_output_device: Option<String>,
    avatar_upload_busy: bool,
    my_bots: Vec<BotSummary>,
    new_bot_name_input: String,
    bot_invite_username_input: String,
    bot_status: Option<String>,
    bot_token_reveal: Option<String>,
    noise_gate: f32,
    update_check_status: Option<String>,
    ping_status: Option<String>,
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
/// thread. See `UiSnapshot` docs for why this can't hold `ui::*` types.
struct ChatRaw {
    session: Session,
    my_avatar_url: String,
    peer_avatar_url: String,
    pending_attachment_preview: Option<std::sync::Arc<[u8]>>,
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
    active_conversation: Option<String>,
    active_conversation_kind: Option<String>,
    active_conversation_peer_id: Option<String>,
    active_peer_name: Option<String>,
    peer_connected: std::collections::HashMap<String, bool>,
    peer_status: std::collections::HashMap<String, String>,
    peer_sas: std::collections::HashMap<String, String>,
    peer_transport: std::collections::HashMap<String, String>,
    peer_remote_fp: std::collections::HashMap<String, String>,
    chat_store_enabled: bool,
    chat_store_allowed: bool,
    clear_chat_busy: bool,
    clear_chat_confirm: bool,
    active_voice_channel: Option<String>,
    room_voice_status: Option<String>,
    voice_users: Vec<VoiceUserRow>,
    messages: Vec<ChatMessage>,
    peer_live_messages: std::collections::HashMap<String, Vec<ChatMessage>>,
    message_input: String,
    has_pending_attachment: bool,
    pending_reply: Option<(String, String, String)>,
    chat_error: Option<String>,
    editing_message_id: Option<String>,
    my_call: Option<MyCallInfo>,
    call_muted: bool,
    call_output_muted: bool,
    call_status_text: Option<String>,
    is_sharing: bool,
    share_picker_open: bool,
    share_targets: Vec<screenshare::ShareTarget>,
    has_remote_share_frame: bool,
    share_view_expanded: bool,
    server_members: Vec<ServerMemberRow>,
    members_panel_width: f32,
    channel_list_width: f32,
    typing_names: Vec<String>,
}

impl UiSnapshot {
    fn from_app(app: &App) -> Self {
        let screen = if app.session.is_none() {
            ui::Screen::Auth
        } else if app.viewing_profile.is_some() || app.profile_error.is_some() {
            ui::Screen::Profile
        } else if app.settings_open {
            ui::Screen::Settings
        } else if app.server_settings_open && app.selected_server.is_some() {
            ui::Screen::ServerSettings
        } else {
            ui::Screen::Chat
        };
        let server_settings = app.selected_server.as_ref().map(|server| ServerSettingsRaw {
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
            member_role_picker_open: app.member_role_picker_open.clone(),
            new_role_name_input: app.new_role_name_input.clone(),
            editing_role_id: app.editing_role_id.clone(),
            role_name_edit_input: app.role_name_edit_input.clone(),
            confirm_delete_role_id: app.confirm_delete_role_id.clone(),
            confirm_delete_server: app.confirm_delete_server,
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
            update_check_status: app.update_check_status.clone(),
            ping_status: app.ping_status.clone(),
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
                app.server_members.iter().find(|m| m.user_id == p.user_id).cloned()
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
            my_server_permissions: app.my_server_permissions,
            admin_search_input: app.admin_search_input.clone(),
            admin_status: app.admin_status.clone(),
            admin_users: app.admin_users.clone(),
            active_conversation: app.active_conversation.clone(),
            active_conversation_kind: app.active_conversation_kind.clone(),
            active_conversation_peer_id: app.active_conversation_peer_id.clone(),
            active_peer_name: app.active_peer_name.clone(),
            peer_connected: app.peer_connected.clone(),
            peer_status: app.peer_status.clone(),
            peer_sas: app.peer_sas.clone(),
            peer_transport: app.peer_transport.clone(),
            peer_remote_fp: app.peer_remote_fp.clone(),
            chat_store_enabled: app.chat_store_enabled,
            chat_store_allowed: app.chat_store_allowed,
            clear_chat_busy: app.clear_chat_busy,
            clear_chat_confirm: app.clear_chat_confirm,
            active_voice_channel: app.active_voice_channel.clone(),
            room_voice_status: app.room_voice_status.clone(),
            voice_users: app.voice_users.clone(),
            messages: app.messages.clone(),
            peer_live_messages: app.peer_live_messages.clone(),
            message_input: app.message_input.clone(),
            has_pending_attachment: app.pending_attachment.is_some(),
            pending_reply: app.pending_reply.clone(),
            chat_error: app.chat_error.clone(),
            editing_message_id: app.editing_message_id.clone(),
            my_call: app.my_call.clone(),
            call_muted: app.call_muted.load(Ordering::Relaxed),
            call_output_muted: app.call_output_muted.load(Ordering::Relaxed),
            call_status_text: app.call_status_text.clone(),
            is_sharing: app.is_sharing,
            share_picker_open: app.share_picker_open,
            share_targets: app.share_targets.clone(),
            has_remote_share_frame: app.remote_share_frame.is_some(),
            share_view_expanded: app.share_view_expanded,
            server_members: app.server_members.clone(),
            members_panel_width: app.members_panel_width,
            channel_list_width: app.channel_list_width,
            typing_names: app.typing_names.clone(),
        });
        Self {
            screen,
            auth_mode: match app.auth_mode {
                crate::types::AuthMode::Login => ui::AuthMode::Login,
                crate::types::AuthMode::Register => ui::AuthMode::Register,
            },
            username_input: app.username_input.clone(),
            image_cache: app.avatar_image_cache.clone(),
            password_input: app.password_input.clone(),
            display_name_input: app.display_name_input.clone(),
            auth_error: app.auth_error.clone().unwrap_or_default(),
            auth_busy: app.auth_busy,
            connect_status: app.connect_status.clone(),
            app_version_line: format!("v{CURRENT_APP_VERSION} · E2EE · P2P CALLS"),
            chat,
            profile,
            settings,
            server_settings,
        }
    }

    fn apply(&self, ui: &ui::AppWindow) {
        ui.set_current_screen(self.screen);
        ui.set_auth_mode(self.auth_mode);
        ui.set_username_input(self.username_input.clone().into());
        ui.set_password_input(self.password_input.clone().into());
        ui.set_display_name_input(self.display_name_input.clone().into());
        ui.set_auth_error(self.auth_error.clone().into());
        ui.set_auth_busy(self.auth_busy);
        ui.set_connect_status(self.connect_status.clone().into());
        ui.set_app_version_line(self.app_version_line.clone().into());
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

const ROLE_PERM_LABELS: [(u32, &str); 8] = [
    (PERM_VIEW_CHANNELS, "View channels"),
    (PERM_SEND_MESSAGES, "Send messages"),
    (PERM_MANAGE_CHANNELS, "Manage channels"),
    (PERM_KICK_MEMBERS, "Kick members"),
    (PERM_MANAGE_ROLES, "Manage roles"),
    (PERM_MANAGE_SERVER, "Manage server"),
    (PERM_CONNECT_VOICE, "Connect to voice"),
    (PERM_SPEAK, "Speak"),
];

fn apply_server_settings(
    s: &ServerSettingsRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &ui::AppWindow,
) {
    let server = &s.server;
    ui.set_ss_category(match s.category {
        ServerSettingsCategory::Overview => ui::ServerSettingsCategory::Overview,
        ServerSettingsCategory::Channels => ui::ServerSettingsCategory::Channels,
        ServerSettingsCategory::Members => ui::ServerSettingsCategory::Members,
        ServerSettingsCategory::Roles => ui::ServerSettingsCategory::Roles,
        ServerSettingsCategory::Invites => ui::ServerSettingsCategory::Invites,
        ServerSettingsCategory::Danger => ui::ServerSettingsCategory::Danger,
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
    ui.set_ss_channels(
        s.channels
            .iter()
            .map(|c| ui::SSChannelRow {
                conversation_id: c.conversation_id.clone().into(),
                name: c.name.clone().into(),
                is_voice: c.channel_type == "voice",
                is_renaming: s.renaming_channel_id.as_deref() == Some(c.conversation_id.as_str()),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );

    ui.set_ss_can_manage_roles(can_manage_roles);
    ui.set_ss_can_kick(can_kick);
    let assignable_roles: Vec<&ServerRoleRow> = s.server_roles.iter().filter(|r| r.position != 0).collect();
    ui.set_ss_members(
        s.server_members
            .iter()
            .map(|m| {
                let role_label = if m.is_owner {
                    "Owner".to_string()
                } else if m.roles.is_empty() {
                    "Member".to_string()
                } else {
                    m.roles.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
                };
                ui::SSMemberRow {
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
                        .map(|r| ui::SSAssignableRole {
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
            .map(|r| ui::SSRoleRow {
                role_id: r.role_id.clone().into(),
                name: r.name.clone().into(),
                color: viewmodel::hex_color(&r.color),
                is_editing: s.editing_role_id.as_deref() == Some(r.role_id.as_str()),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    if let Some(editing) = s.editing_role_id.as_deref().and_then(|id| s.server_roles.iter().find(|r| r.role_id == id)) {
        ui.set_ss_role_edit_color(editing.color.clone().into());
        ui.set_ss_editing_role_is_default(editing.position == 0);
        ui.set_ss_role_permissions(
            ROLE_PERM_LABELS
                .iter()
                .map(|(bit, label)| ui::SSPermRow {
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
    ui.set_ss_confirm_delete_server(s.confirm_delete_server);
}

fn apply_settings(
    s: &SettingsRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &ui::AppWindow,
) {
    let session = &s.session;
    ui.set_settings_category(match s.category {
        SettingsCategory::Account => ui::SettingsCategory::Account,
        SettingsCategory::Privacy => ui::SettingsCategory::Privacy,
        SettingsCategory::Bots => ui::SettingsCategory::Bots,
        SettingsCategory::Voice => ui::SettingsCategory::Voice,
        SettingsCategory::About => ui::SettingsCategory::About,
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
    ui.set_settings_profile_status(s.settings_profile_status.clone().unwrap_or_default().into());
    ui.set_settings_current_password_input(s.settings_current_password_input.clone().into());
    ui.set_settings_new_password_input(s.settings_new_password_input.clone().into());
    ui.set_settings_confirm_password_input(s.settings_confirm_password_input.clone().into());
    ui.set_settings_password_status(s.settings_password_status.clone().unwrap_or_default().into());
    let (badge_text, badge_bg, badge_fg) = if session.platform_role == "owner" {
        viewmodel::badge_for_platform_role("owner")
    } else if session.is_admin {
        viewmodel::badge_for_platform_role("admin")
    } else if session.is_moderator {
        viewmodel::badge_for_platform_role("moderator")
    } else {
        viewmodel::badge_for_platform_role("user")
    };
    ui.set_settings_my_badge_text(badge_text);
    ui.set_settings_my_badge_bg(badge_bg);
    ui.set_settings_my_badge_fg(badge_fg);
    ui.set_settings_store_chat_history(session.store_chat_history);
    ui.set_settings_hide_online_status(session.hide_online_status);
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
            .map(|b| ui::BotRow {
                bot_id: b.bot_id.clone().into(),
                display_name: b.display_name.clone().into(),
                username: b.username.clone().into(),
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    ui.set_settings_bot_invite_username_input(s.bot_invite_username_input.clone().into());
    ui.set_settings_bot_status(s.bot_status.clone().unwrap_or_default().into());
    let mut input_devices = vec![ui::DeviceRow {
        name: "System default".into(),
        selected: s.settings_input_device.is_none(),
    }];
    input_devices.extend(s.settings_input_devices.iter().map(|d| ui::DeviceRow {
        name: d.clone().into(),
        selected: s.settings_input_device.as_deref() == Some(d.as_str()),
    }));
    ui.set_settings_input_devices(input_devices.as_slice().into());
    let mut output_devices = vec![ui::DeviceRow {
        name: "System default".into(),
        selected: s.settings_output_device.is_none(),
    }];
    output_devices.extend(s.settings_output_devices.iter().map(|d| ui::DeviceRow {
        name: d.clone().into(),
        selected: s.settings_output_device.as_deref() == Some(d.as_str()),
    }));
    ui.set_settings_output_devices(output_devices.as_slice().into());
    ui.set_settings_noise_gate(s.noise_gate);
    ui.set_settings_noise_gate_label(if s.noise_gate <= 0.0005 {
        "Off".to_string()
    } else {
        format!("{:.3}", s.noise_gate)
    }.into());
    ui.set_settings_version_line(format!("Talkyss v{CURRENT_APP_VERSION}").into());
    ui.set_settings_vault_hint(history::vault_root_display(&session.user_id).into());
    ui.set_settings_update_check_status(s.update_check_status.clone().unwrap_or_default().into());
    ui.set_settings_ping_status(s.ping_status.clone().unwrap_or_default().into());
}

fn apply_profile(
    p: &ProfileRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &ui::AppWindow,
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
    ui.set_profile_online(is_online(profile.last_seen_at));
    ui.set_profile_status_message(profile.status_message.clone().into());
    ui.set_profile_bio(profile.bio.clone().into());
    ui.set_profile_is_staff(profile.is_staff);
    let viewing_self = p.my_user_id.as_deref() == Some(profile.user_id.as_str());
    ui.set_profile_show_support_dm(profile.can_support_dm && !viewing_self);
    ui.set_profile_is_friend(profile.is_friend);
    ui.set_profile_favorite(profile.favorite);
    ui.set_profile_relation(profile.relation.clone().into());
    ui.set_profile_request_id(profile.request_id.clone().into());
    ui.set_profile_friend_request_busy(p.friend_request_busy);
    ui.set_profile_can_moderate(profile.relation != "self" && !viewing_self);
    ui.set_profile_is_blocked(p.blocked.iter().any(|b| b.user_id == profile.user_id));
    ui.set_profile_confirm_block(p.confirm_block_user_id.as_deref() == Some(profile.user_id.as_str()));
    ui.set_profile_mutual_servers_line(if profile.mutual_servers.is_empty() {
        String::new()
    } else {
        format!("Servers in common: {}", profile.mutual_servers.join(", "))
    }.into());
    if let (Some(server_name), Some(member)) = (&p.selected_server_name, &p.member) {
        ui.set_profile_has_role_info(true);
        ui.set_profile_role_section_title(format!("Role in {server_name}").into());
        ui.set_profile_role_is_owner(member.is_owner);
        ui.set_profile_role_badges(
            member
                .roles
                .iter()
                .map(|r| ui::RoleTagRow {
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
    static CONVO_ROWS_CACHE: std::cell::RefCell<Vec<ui::ConversationRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static SERVER_ROWS_CACHE: std::cell::RefCell<Vec<ui::ServerRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static TEXT_CHANNEL_ROWS_CACHE: std::cell::RefCell<Vec<ui::ChannelRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static VOICE_CHANNEL_ROWS_CACHE: std::cell::RefCell<Vec<ui::ChannelRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn rows_eq<T>(a: &[T], b: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| eq(x, y))
}

fn convo_row_eq(a: &ui::ConversationRow, b: &ui::ConversationRow) -> bool {
    a.id == b.id && a.title == b.title && a.unread == b.unread && a.active == b.active
}

fn server_row_eq(a: &ui::ServerRow, b: &ui::ServerRow) -> bool {
    a.server_id == b.server_id
        && a.name == b.name
        && a.initial == b.initial
        && a.icon_url == b.icon_url
        && a.active == b.active
}

fn channel_row_eq(a: &ui::ChannelRow, b: &ui::ChannelRow) -> bool {
    a.conversation_id == b.conversation_id
        && a.label == b.label
        && a.is_voice == b.is_voice
        && a.active == b.active
}

/// Pushes `rows` to `set` only when they differ from the cached copy.
fn set_rows_if_changed<T: Clone>(
    cache: &'static std::thread::LocalKey<std::cell::RefCell<Vec<T>>>,
    rows: Vec<T>,
    eq: impl Fn(&T, &T) -> bool,
    set: impl FnOnce(Vec<T>),
) {
    let changed = cache.with(|c| {
        let mut c = c.borrow_mut();
        if rows_eq(&c, &rows, &eq) {
            false
        } else {
            *c = rows.clone();
            true
        }
    });
    if changed {
        set(rows);
    }
}

fn apply_chat(
    c: &ChatRaw,
    cache: &std::collections::HashMap<String, std::sync::Arc<[u8]>>,
    ui: &ui::AppWindow,
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
        && !matches!(c.sidebar_tab, SidebarTab::Admin | SidebarTab::Friends | SidebarTab::Requests)
    {
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
        SidebarTab::Chats => ui::SidebarTab::Chats,
        SidebarTab::Friends => ui::SidebarTab::Friends,
        SidebarTab::Requests => ui::SidebarTab::Requests,
        SidebarTab::Servers => ui::SidebarTab::Servers,
        SidebarTab::Admin => ui::SidebarTab::Admin,
    };

    // ---- Rail ----
    ui.set_chat_home_active(home_active);
    ui.set_chat_unread_count(unread_count);
    set_rows_if_changed(
        &SERVER_ROWS_CACHE,
        viewmodel::server_rows(&c.servers, c.selected_server.as_ref().map(|s| s.server_id.as_str())),
        server_row_eq,
        |rows| ui.set_chat_servers(rows.as_slice().into()),
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
    ui.set_chat_server_status(c.server_status.clone().unwrap_or_default().into());
    ui.set_chat_new_group_open(c.new_group_open);
    ui.set_chat_new_group_name(c.new_group_name_input.clone().into());
    ui.set_chat_group_candidates(
        viewmodel::group_candidate_rows(&c.friends, &c.new_group_selected)
            .as_slice()
            .into(),
    );
    ui.set_chat_group_create_status(c.group_create_status.clone().unwrap_or_default().into());
    set_rows_if_changed(
        &CONVO_ROWS_CACHE,
        viewmodel::conversation_rows(&c.conversations, c.active_conversation.as_deref()),
        convo_row_eq,
        |rows| ui.set_chat_conversations(rows.as_slice().into()),
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
    ui.set_chat_friends(viewmodel::friend_rows(&filtered_friends).as_slice().into());
    ui.set_chat_blocked(viewmodel::blocked_rows(&c.blocked).as_slice().into());
    ui.set_chat_confirm_block_user_id(c.confirm_block_user_id.clone().unwrap_or_default().into());
    ui.set_chat_friend_request_busy(c.friend_request_busy);
    ui.set_chat_incoming_requests(viewmodel::incoming_request_rows(&c.incoming_requests).as_slice().into());
    ui.set_chat_outgoing_requests(viewmodel::outgoing_request_rows(&c.outgoing_requests).as_slice().into());
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
    set_rows_if_changed(
        &TEXT_CHANNEL_ROWS_CACHE,
        viewmodel::channel_rows(&c.channels, c.active_conversation.as_deref(), false),
        channel_row_eq,
        |rows| ui.set_chat_text_channels(rows.as_slice().into()),
    );
    set_rows_if_changed(
        &VOICE_CHANNEL_ROWS_CACHE,
        viewmodel::channel_rows(&c.channels, c.active_conversation.as_deref(), true),
        channel_row_eq,
        |rows| ui.set_chat_voice_channels(rows.as_slice().into()),
    );
    ui.set_chat_admin_search(c.admin_search_input.clone().into());
    ui.set_chat_admin_status(c.admin_status.clone().unwrap_or_default().into());
    ui.set_chat_admin_users(
        viewmodel::admin_user_rows(&c.admin_users, &c.admin_search_input, &session.username)
            .as_slice()
            .into(),
    );
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
    } else {
        viewmodel::badge_for_platform_role("user")
    };
    ui.set_chat_my_badge_text(badge_text);
    ui.set_chat_my_badge_bg(badge_bg);
    ui.set_chat_my_badge_fg(badge_fg);

    // ---- Chat area ----
    let has_conversation = c.active_conversation.is_some();
    ui.set_chat_has_conversation(has_conversation);
    let peer_friend = c
        .active_conversation_peer_id
        .as_ref()
        .and_then(|id| c.friends.iter().find(|f| &f.user_id == id));
    let is_channel_icon = peer_friend.is_none();
    ui.set_chat_is_channel_icon(is_channel_icon);
    ui.set_chat_peer_title(c.active_peer_name.clone().unwrap_or_else(|| "Chat".into()).into());
    if let Some(friend) = peer_friend {
        ui.set_chat_peer_initial(viewmodel::initial(friend.label()));
        ui.set_chat_peer_avatar_color(viewmodel::hex_color(&friend.avatar_color));
        ui.set_chat_peer_online(is_online(friend.last_seen_at));
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
            let fp = cur_peer_id.and_then(|id| c.peer_remote_fp.get(id)).map(String::as_str).unwrap_or("…");
            let tr = cur_peer_id.and_then(|id| c.peer_transport.get(id)).map(String::as_str).unwrap_or("?");
            format!("peerseal · {tr} · {fp}")
        } else {
            cur_peer_id
                .and_then(|id| c.peer_status.get(id))
                .cloned()
                .unwrap_or_else(|| "Connecting secure channel…".to_string())
        };
        ui.set_chat_connection_label(label.into());
        ui.set_chat_sas_label(
            cur_peer_id.and_then(|id| c.peer_sas.get(id)).cloned().unwrap_or_default().into(),
        );
    } else {
        ui.set_chat_connection_label("".into());
        ui.set_chat_sas_label("".into());
    }
    ui.set_chat_show_call_button(is_direct && c.my_call.is_none());
    let is_server_channel = matches!(c.active_conversation_kind.as_deref(), Some("channel") | Some("voice"));
    ui.set_chat_is_server_channel(is_server_channel);
    ui.set_chat_store_enabled(c.chat_store_enabled);
    ui.set_chat_store_allowed(c.chat_store_allowed);
    ui.set_chat_clear_chat_busy(c.clear_chat_busy);
    ui.set_chat_clear_chat_confirm(c.clear_chat_confirm);
    let can_voice = matches!(c.active_conversation_kind.as_deref(), Some("voice") | Some("group"));
    ui.set_chat_can_voice(can_voice);
    let in_voice = c.active_voice_channel.as_deref() == c.active_conversation.as_deref() && can_voice;
    ui.set_chat_in_voice(in_voice);
    ui.set_chat_room_voice_status(c.room_voice_status.clone().unwrap_or_default().into());
    ui.set_chat_voice_users_label(
        c.voice_users.iter().map(|u| u.display_name.as_str()).collect::<Vec<_>>().join(", ").into(),
    );
    ui.set_chat_messages(
        viewmodel::chat_message_rows(
            &c.messages,
            c.active_conversation_peer_id.as_ref().and_then(|id| c.peer_live_messages.get(id)).map(Vec::as_slice),
            &session.user_id,
            session.is_admin,
        )
        .as_slice()
        .into(),
    );
    ui.set_chat_quick_emojis(QUICK_REACT_EMOJIS.iter().map(|e| slint::SharedString::from(*e)).collect::<Vec<_>>().as_slice().into());
    let is_editing = c.editing_message_id.is_some();
    ui.set_chat_is_editing(is_editing);
    let mut placeholder = if is_editing { "Edit message..." } else { "Type a message..." }.to_string();
    if is_direct && !peer_connected_now {
        placeholder = "Waiting for secure channel…".to_string();
    }
    ui.set_chat_input_placeholder(placeholder.into());
    ui.set_chat_send_label(if is_editing { "Save" } else { "Send" }.into());
    let crypto_ready = !is_direct || peer_connected_now;
    ui.set_chat_crypto_ready(crypto_ready);
    ui.set_chat_message_input(c.message_input.clone().into());
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
    let online_members: Vec<ServerMemberRow> =
        c.server_members.iter().filter(|m| !m.is_bot && is_online(m.last_seen_at)).cloned().collect();
    let offline_members: Vec<ServerMemberRow> =
        c.server_members.iter().filter(|m| !m.is_bot && !is_online(m.last_seen_at)).cloned().collect();
    let bot_members: Vec<ServerMemberRow> = c.server_members.iter().filter(|m| m.is_bot).cloned().collect();
    ui.set_chat_members_online(online_members.len() as i32);
    ui.set_chat_members_online_list(viewmodel::member_rows(&online_members).as_slice().into());
    ui.set_chat_members_offline_list(viewmodel::member_rows(&offline_members).as_slice().into());
    ui.set_chat_members_bot_list(viewmodel::member_rows(&bot_members).as_slice().into());

    // ---- Call banner ----
    if let Some(call) = &c.my_call {
        let is_ringing = call.status == "ringing";
        ui.set_chat_call_visible(true);
        ui.set_chat_call_ringing(is_ringing);
        ui.set_chat_call_incoming(is_ringing && !call.is_caller);
        ui.set_chat_call_active(call.status == "active");
        let label = match call.status.as_str() {
            "ringing" if !call.is_caller => format!("Incoming call from {}", call.peer_display_name),
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
        ui.set_chat_share_targets(viewmodel::share_target_rows(&c.share_targets).as_slice().into());
        ui.set_chat_has_remote_frame(c.has_remote_share_frame);
        ui.set_chat_remote_frame_title(format!("{}'s screen", call.peer_display_name).into());
        ui.set_chat_share_expanded(c.share_view_expanded);
    } else {
        ui.set_chat_call_visible(false);
        ui.set_chat_has_remote_frame(false);
    }

    // Patch avatar/attachment images now that we're on the UI thread and
    // `slint::Image` is safe to construct. Rows whose image hasn't been
    // fetched yet keep their colored-initial fallback until it arrives
    // (the next resync, triggered by `AvatarImageLoaded`, fills them in).
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_servers,
        cache,
        |r: &ui::ServerRow| r.icon_url.to_string(),
        |r, img| r.icon = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_people_hits,
        cache,
        |r: &ui::PeopleHitRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_suggestions,
        cache,
        |r: &ui::SuggestionRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_friends,
        cache,
        |r: &ui::FriendRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_incoming_requests,
        cache,
        |r: &ui::IncomingRequestRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_outgoing_requests,
        cache,
        |r: &ui::OutgoingRequestRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_members_online_list,
        cache,
        |r: &ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_members_offline_list,
        cache,
        |r: &ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    fill_model_photos(
        ui,
        ui::AppWindow::get_chat_members_bot_list,
        cache,
        |r: &ui::MemberRow| r.photo_url.to_string(),
        |r, img| r.photo = img,
    );
    {
        let model = ui.get_chat_messages();
        let n = model.row_count();
        for i in 0..n {
            if let Some(mut row) = model.row_data(i) {
                if let Some(img) = img_cache::image_for(cache, &row.author_photo_url) {
                    row.author_photo = img;
                }
                if let Some(img) = img_cache::image_for(cache, &row.attachment_url) {
                    row.attachment = img;
                }
                model.set_row_data(i, row);
            }
        }
    }
}

fn sync_ui(app: &App, ui_weak: &slint::Weak<ui::AppWindow>) {
    let snapshot = UiSnapshot::from_app(app);
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            snapshot.apply(&ui);
        }
    });
}

/// Patch `photo`/`attachment` on already-set Slint list models. The rows were
/// built on the pump thread with empty images (because `slint::Image` can't
/// cross threads); here -- on the UI thread -- we decode the bytes for each
/// row's URL from the snapshot's `image_cache` and assign the result. Rows
/// whose image hasn't been fetched yet keep their colored-initial fallback.
fn fill_model_photos<T: Clone + 'static>(
    ui: &ui::AppWindow,
    get: impl Fn(&ui::AppWindow) -> slint::ModelRc<T>,
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
fn wire_callbacks(ui: &ui::AppWindow, tx: UnboundedSender<Message>) {
    let t = tx.clone();
    ui.on_auth_switch_mode(move |mode| {
        let mode = match mode {
            ui::AuthMode::Login => crate::types::AuthMode::Login,
            ui::AuthMode::Register => crate::types::AuthMode::Register,
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
    ui.on_auth_submit(move || {
        let _ = t.send(Message::SubmitAuth);
    });

    let t = tx.clone();
    ui.on_escape_pressed(move || {
        let _ = t.send(Message::EscapePressed);
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
fn wire_chat_callbacks(ui: &ui::AppWindow, tx: &UnboundedSender<Message>) {
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
    on1!(on_chat_select_server, |id: slint::SharedString| Message::SelectServer(id.to_string()));
    on0!(on_chat_toggle_add_menu, Message::ToggleServerAddMenu);
    on0!(on_chat_open_friends, Message::SidebarTabChanged(SidebarTab::Friends));
    on0!(on_chat_open_requests, Message::SidebarTabChanged(SidebarTab::Requests));
    on0!(on_chat_open_admin, Message::SidebarTabChanged(SidebarTab::Admin));

    // ---- Sidebar: add-server / join-server ----
    on1!(on_chat_new_server_name_changed, |t: slint::SharedString| Message::NewServerNameChanged(t.to_string()));
    on0!(on_chat_create_server, Message::CreateServer);
    on1!(on_chat_join_server_code_changed, |t: slint::SharedString| Message::JoinServerCodeChanged(t.to_string()));
    on0!(on_chat_join_server, Message::JoinServer);

    // ---- Sidebar: Chats tab ----
    on0!(on_chat_toggle_group_panel, Message::ToggleGroupPanel);
    on1!(on_chat_group_name_changed, |t: slint::SharedString| Message::GroupNameInputChanged(t.to_string()));
    on1!(on_chat_toggle_group_member, |id: slint::SharedString| Message::ToggleGroupMember(id.to_string()));
    on0!(on_chat_create_group, Message::CreateGroup);
    on1!(on_chat_open_conversation, |id: slint::SharedString| Message::OpenConversationDirect(id.to_string()));

    // ---- Sidebar: Friends tab ----
    on1!(on_chat_add_friend_input_changed, |t: slint::SharedString| Message::AddFriendInputChanged(t.to_string()));
    on1!(on_chat_add_friend_note_changed, |t: slint::SharedString| Message::AddFriendNoteChanged(t.to_string()));
    on0!(on_chat_send_friend_request, Message::SendFriendRequest);
    on1!(on_chat_set_friends_filter, |i: i32| Message::SetFriendsFilter(match i {
        1 => FriendsFilter::Online,
        2 => FriendsFilter::Favorites,
        _ => FriendsFilter::All,
    }));
    on1!(on_chat_friends_filter_input_changed, |t: slint::SharedString| Message::FriendsFilterChanged(t.to_string()));
    on1!(on_chat_open_profile, |id: slint::SharedString| Message::OpenProfile(id.to_string()));
    on1!(on_chat_send_friend_request_to, |u: slint::SharedString| Message::SendFriendRequestToUser(u.to_string()));
    on1!(on_chat_message_friend, |id: slint::SharedString| Message::OpenConversationWithFriend(id.to_string()));
    on1!(on_chat_toggle_favorite, |id: slint::SharedString| Message::ToggleFavorite(id.to_string()));
    on1!(on_chat_remove_friend, |id: slint::SharedString| Message::RemoveFriend(id.to_string()));
    on1!(on_chat_confirm_block, |id: slint::SharedString| Message::ConfirmBlockUser(id.to_string()));
    on0!(on_chat_cancel_block, Message::CancelBlockUser);
    on1!(on_chat_block_user, |id: slint::SharedString| Message::BlockUser(id.to_string()));
    on1!(on_chat_unblock_user, |id: slint::SharedString| Message::UnblockUser(id.to_string()));

    // ---- Sidebar: Requests tab ----
    {
        let t = tx.clone();
        ui.on_chat_respond_request(move |request_id, _from_user_id, accept| {
            let _ = t.send(Message::RespondRequest(request_id.to_string(), accept));
        });
    }
    on0!(on_chat_accept_all, Message::RespondAllIncoming(true));
    on0!(on_chat_decline_all, Message::RespondAllIncoming(false));
    on1!(on_chat_cancel_outgoing, |id: slint::SharedString| Message::CancelOutgoingRequest(id.to_string()));

    // ---- Sidebar: Servers tab ----
    on0!(on_chat_toggle_server_settings, Message::ToggleServerSettings);
    on1!(on_chat_copy_invite_link, |code: slint::SharedString| Message::CopyInviteLink(code.to_string()));
    on0!(on_chat_toggle_new_channel, Message::ToggleNewChannelInput);
    on1!(on_chat_new_channel_name_changed, |t: slint::SharedString| Message::NewChannelNameChanged(t.to_string()));
    on0!(on_chat_create_channel, Message::CreateChannel);
    on0!(on_chat_toggle_new_channel_voice, Message::ToggleNewChannelIsVoice);
    on1!(on_chat_open_channel, |id: slint::SharedString| Message::OpenChannel(id.to_string()));

    // ---- Sidebar: Admin tab ----
    on1!(on_chat_admin_search_changed, |t: slint::SharedString| Message::AdminSearchInputChanged(t.to_string()));
    on2!(on_chat_admin_set_role, |id: slint::SharedString, role: slint::SharedString| {
        Message::AdminSetPlatformRole(id.to_string(), role.to_string())
    });
    on2!(on_chat_admin_set_banned, |id: slint::SharedString, banned: bool| {
        Message::AdminSetBanned(id.to_string(), banned)
    });

    // ---- Sidebar: account footer + resize ----
    on0!(on_chat_open_settings, Message::OpenSettings);
    on0!(on_chat_sidebar_resize_started, Message::PanelResizeStarted(ResizePanel::ChannelList));
    on1!(on_chat_sidebar_resize_moved, |x: f32| Message::PanelResizeMoved(x));
    on0!(on_chat_sidebar_resize_ended, Message::PanelResizeEnded);

    // ---- Chat area ----
    on0!(on_chat_start_call, Message::StartCall);
    on0!(on_chat_toggle_store, Message::ToggleStoreHistoryThisChat);
    on0!(on_chat_toggle_clear_confirm, Message::ToggleClearChatConfirm);
    on0!(on_chat_confirm_clear, Message::ConfirmClearChat);
    on0!(on_chat_join_voice, Message::JoinVoiceChannel);
    on0!(on_chat_leave_voice, Message::LeaveVoiceChannel);
    on1!(on_chat_message_input_edited, |t: slint::SharedString| Message::MessageInputChanged(t.to_string()));
    on0!(on_chat_send, Message::SendMessage);
    on0!(on_chat_pick_attachment, Message::PickAttachmentImage);
    on0!(on_chat_remove_attachment, Message::RemovePendingAttachment);
    on0!(on_chat_cancel_edit, Message::CancelEdit);
    on0!(on_chat_cancel_reply, Message::CancelReply);
    on2!(on_chat_react, |id: slint::SharedString, emoji: slint::SharedString| {
        Message::ToggleReaction(id.to_string(), emoji.to_string())
    });
    on3!(on_chat_reply, |id: slint::SharedString, author: slint::SharedString, snippet: slint::SharedString| {
        Message::ReplyToMessage(id.to_string(), author.to_string(), snippet.to_string())
    });
    on1!(on_chat_copy, |t: slint::SharedString| Message::CopyMessage(t.to_string()));
    on3!(on_chat_edit, |id: slint::SharedString, body: slint::SharedString, enc: bool| {
        Message::EditMessage(id.to_string(), body.to_string(), enc)
    });
    on1!(on_chat_delete, |id: slint::SharedString| Message::DeleteMessage(id.to_string()));
    on1!(on_chat_purge, |id: slint::SharedString| Message::PurgeMessage(id.to_string()));
    on1!(on_chat_open_attachment, |url: slint::SharedString| Message::OpenAttachmentPreview(url.to_string()));

    // ---- Members drawer ----
    on0!(on_chat_toggle_members, Message::ToggleMembersPanel);
    on0!(on_chat_members_resize_started, Message::PanelResizeStarted(ResizePanel::Members));
    on1!(on_chat_members_resize_moved, |x: f32| Message::PanelResizeMoved(x));
    on0!(on_chat_members_resize_ended, Message::PanelResizeEnded);

    // ---- Call banner ----
    on0!(on_chat_call_accept, Message::AcceptCall);
    on0!(on_chat_call_decline, Message::DeclineCall);
    on0!(on_chat_call_hang_up, Message::HangUp);
    on0!(on_chat_call_toggle_mute, Message::ToggleMute);
    on0!(on_chat_call_toggle_mute_all, Message::ToggleMuteAll);
    on0!(on_chat_toggle_share_picker, Message::ToggleSharePicker);
    on0!(on_chat_stop_share, Message::StopShare);
    on1!(on_chat_start_share, |id: slint::SharedString| Message::StartShare(id.to_string()));
    on0!(on_chat_toggle_share_size, Message::ToggleShareViewSize);
}

/// Wires the `profile_*` Slint callbacks -- port of src/view/profile.rs's
/// button handlers.
fn wire_profile_callbacks(ui: &ui::AppWindow, tx: &UnboundedSender<Message>) {
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
    on1!(on_profile_support_dm, |id: slint::SharedString| Message::OpenSupportDm(id.to_string()));
    on1!(on_profile_message_friend, |id: slint::SharedString| Message::OpenConversationWithFriend(id.to_string()));
    on1!(on_profile_toggle_favorite, |id: slint::SharedString| Message::ToggleFavorite(id.to_string()));
    on1!(on_profile_respond_request, |id: slint::SharedString| Message::RespondRequest(id.to_string(), true));
    on1!(on_profile_send_friend_request, |u: slint::SharedString| Message::SendFriendRequestToUser(u.to_string()));
    on1!(on_profile_unblock, |id: slint::SharedString| Message::UnblockUser(id.to_string()));
    on1!(on_profile_confirm_block_click, |id: slint::SharedString| Message::ConfirmBlockUser(id.to_string()));
    on1!(on_profile_block, |id: slint::SharedString| Message::BlockUser(id.to_string()));
    on0!(on_profile_cancel_block, Message::CancelBlockUser);
}

/// Wires the `settings_*` Slint callbacks -- port of src/view/settings.rs's
/// button handlers.
fn wire_settings_callbacks(ui: &ui::AppWindow, tx: &UnboundedSender<Message>) {
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
                ui::SettingsCategory::Account => SettingsCategory::Account,
                ui::SettingsCategory::Privacy => SettingsCategory::Privacy,
                ui::SettingsCategory::Bots => SettingsCategory::Bots,
                ui::SettingsCategory::Voice => SettingsCategory::Voice,
                ui::SettingsCategory::About => SettingsCategory::About,
            };
            let _ = t.send(Message::SettingsCategoryChanged(cat));
        });
    }
    on0!(on_settings_pick_avatar, Message::PickAvatarImage);
    on0!(on_settings_remove_avatar, Message::RemoveAvatarImage);
    on1!(on_settings_display_name_changed, |t: slint::SharedString| Message::SettingsDisplayNameChanged(t.to_string()));
    on1!(on_settings_status_changed, |t: slint::SharedString| Message::SettingsStatusChanged(t.to_string()));
    on1!(on_settings_bio_changed, |t: slint::SharedString| Message::SettingsBioChanged(t.to_string()));
    on1!(on_settings_avatar_color_selected, |c: slint::SharedString| Message::SettingsAvatarColorSelected(c.to_string()));
    on0!(on_settings_save_profile, Message::SaveProfile);
    on1!(on_settings_current_password_changed, |t: slint::SharedString| Message::SettingsCurrentPasswordChanged(t.to_string()));
    on1!(on_settings_new_password_changed, |t: slint::SharedString| Message::SettingsNewPasswordChanged(t.to_string()));
    on1!(on_settings_confirm_password_changed, |t: slint::SharedString| Message::SettingsConfirmPasswordChanged(t.to_string()));
    on0!(on_settings_change_password, Message::ChangePassword);
    on0!(on_settings_log_out, Message::LogOut);
    on0!(on_settings_toggle_store_history, Message::ToggleStoreHistoryGlobal);
    on0!(on_settings_toggle_hide_online, Message::ToggleHideOnline);
    on0!(on_settings_toggle_friends_only_dms, Message::ToggleFriendsOnlyDms);
    on0!(on_settings_toggle_discoverable, Message::ToggleDiscoverable);
    on0!(on_settings_cycle_friend_request_privacy, Message::CycleFriendRequestPrivacy);
    on0!(on_settings_cycle_presence, Message::CyclePresenceStatus);
    on0!(on_settings_sign_out_others, Message::SignOutOtherSessions);
    on1!(on_settings_new_bot_name_changed, |t: slint::SharedString| Message::NewBotNameChanged(t.to_string()));
    on0!(on_settings_create_bot, Message::CreateBot);
    on0!(on_settings_refresh_bots, Message::RefreshMyBots);
    on1!(on_settings_copy_token, |t: slint::SharedString| Message::CopyMessage(t.to_string()));
    on0!(on_settings_dismiss_token, Message::DismissBotToken);
    on1!(on_settings_regenerate_bot_token, |id: slint::SharedString| Message::RegenerateBotToken(id.to_string()));
    on1!(on_settings_delete_bot, |id: slint::SharedString| Message::DeleteBot(id.to_string()));
    on1!(on_settings_bot_invite_username_changed, |t: slint::SharedString| Message::BotInviteUsernameChanged(t.to_string()));
    on0!(on_settings_invite_bot, Message::InviteBotToServer);
    on1!(on_settings_input_device_selected, |d: slint::SharedString| Message::SettingsInputDeviceSelected(d.to_string()));
    on1!(on_settings_output_device_selected, |d: slint::SharedString| Message::SettingsOutputDeviceSelected(d.to_string()));
    on1!(on_settings_noise_gate_changed, |v: f32| Message::NoiseGateChanged(v));
    on0!(on_settings_check_for_update, Message::CheckForUpdate);
    on0!(on_settings_measure_ping, Message::MeasurePing);
}

/// Wires the `ss_*` (server settings) Slint callbacks -- port of
/// src/view/server_settings.rs's button handlers.
fn wire_server_settings_callbacks(ui: &ui::AppWindow, tx: &UnboundedSender<Message>) {
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
                ui::ServerSettingsCategory::Overview => ServerSettingsCategory::Overview,
                ui::ServerSettingsCategory::Channels => ServerSettingsCategory::Channels,
                ui::ServerSettingsCategory::Members => ServerSettingsCategory::Members,
                ui::ServerSettingsCategory::Roles => ServerSettingsCategory::Roles,
                ui::ServerSettingsCategory::Invites => ServerSettingsCategory::Invites,
                ui::ServerSettingsCategory::Danger => ServerSettingsCategory::Danger,
            };
            let _ = t.send(Message::ServerSettingsCategoryChanged(cat));
        });
    }
    on0!(on_ss_pick_icon, Message::PickServerIcon);
    on0!(on_ss_remove_icon, Message::RemoveServerIcon);
    on1!(on_ss_rename_server_input_changed, |t: slint::SharedString| Message::RenameServerInputChanged(t.to_string()));
    on0!(on_ss_rename_server, Message::RenameServer);
    on1!(on_ss_custom_slug_changed, |t: slint::SharedString| Message::CustomSlugInputChanged(t.to_string()));
    on0!(on_ss_save_custom_slug, Message::SaveCustomSlug);
    on0!(on_ss_clear_custom_slug, Message::ClearCustomSlug);
    on1!(on_ss_new_channel_name_changed, |t: slint::SharedString| Message::NewChannelNameChanged(t.to_string()));
    on0!(on_ss_toggle_new_channel_voice, Message::ToggleNewChannelIsVoice);
    on0!(on_ss_create_channel, Message::CreateChannel);
    on2!(on_ss_start_rename_channel, |id: slint::SharedString, name: slint::SharedString| {
        Message::StartRenameChannel(id.to_string(), name.to_string())
    });
    on1!(on_ss_rename_channel_input_changed, |t: slint::SharedString| Message::RenameChannelInputChanged(t.to_string()));
    on0!(on_ss_save_rename_channel, Message::RenameChannel);
    on0!(on_ss_cancel_rename_channel, Message::CancelRenameChannel);
    on1!(on_ss_delete_channel, |id: slint::SharedString| Message::DeleteChannel(id.to_string()));
    on1!(on_ss_toggle_member_picker, |id: slint::SharedString| Message::ToggleMemberRolePicker(id.to_string()));
    on2!(on_ss_toggle_member_role, |uid: slint::SharedString, rid: slint::SharedString| {
        Message::ToggleMemberRole(uid.to_string(), rid.to_string())
    });
    on1!(on_ss_kick_member, |id: slint::SharedString| Message::KickMember(id.to_string()));
    on1!(on_ss_open_profile, |id: slint::SharedString| Message::OpenProfile(id.to_string()));
    on1!(on_ss_new_role_name_changed, |t: slint::SharedString| Message::NewRoleNameChanged(t.to_string()));
    on0!(on_ss_create_role, Message::CreateRole);
    on1!(on_ss_select_role_for_edit, |id: slint::SharedString| Message::SelectRoleForEdit(id.to_string()));
    on0!(on_ss_close_role_editor, Message::CloseRoleEditor);
    on1!(on_ss_role_name_edit_changed, |t: slint::SharedString| Message::RoleNameEditChanged(t.to_string()));
    on0!(on_ss_save_role_name, Message::SaveRoleName);
    on2!(on_ss_set_role_color, |id: slint::SharedString, hex: slint::SharedString| {
        Message::SetRoleColor(id.to_string(), hex.to_string())
    });
    {
        let t = tx.clone();
        ui.on_ss_toggle_role_permission(move |id, bit| {
            let _ = t.send(Message::ToggleRolePermission(id.to_string(), bit as u32));
        });
    }
    on1!(on_ss_confirm_delete_role_click, |id: slint::SharedString| Message::ConfirmDeleteRole(id.to_string()));
    on0!(on_ss_cancel_delete_role, Message::CancelDeleteRole);
    on1!(on_ss_delete_role, |id: slint::SharedString| Message::DeleteRole(id.to_string()));
    on1!(on_ss_copy_invite_code, |code: slint::SharedString| Message::CopyInviteCode(code.to_string()));
    on1!(on_ss_copy_invite_link, |code: slint::SharedString| Message::CopyInviteLink(code.to_string()));
    on0!(on_ss_regenerate_invite_code, Message::RegenerateInviteCode);
    on0!(on_ss_toggle_confirm_delete_server, Message::ToggleConfirmDeleteServer);
    on0!(on_ss_delete_server, Message::DeleteServer);
}
