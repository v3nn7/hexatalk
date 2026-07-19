//! The `App` struct (all GUI/session/call/server state in one place, as
//! iced's Elm architecture expects) plus its lifecycle methods: `new()`,
//! `subscription()`, and the small helpers `update()` leans on (session
//! reset, avatar fetching, peerseal worker wiring, ...). `update()` itself
//! lives in its own file (src/update.rs) since it's the single largest
//! function in the app by a wide margin.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::{Duration, Instant};

use convex::{ConvexClient, FunctionResult, Value};
use maplit::btreemap;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::rt::{Job, Task, WindowAction, job};

use crate::crypto;
use crate::media::call;
use crate::media::notify::{BEEP_MESSAGE, notify_desktop, play_beep};
use crate::media::screenshare;
use crate::net::convex_parse::{expect_null, humanize_error, value_as_bool};
use crate::net::peer;
use crate::net::subscriptions::{
    DECRYPT_FAILED_PLACEHOLDER, admin_users_subscription, apply_decrypted_payload,
    blocked_subscription, call_subscription, channels_subscription, conversations_subscription,
    friends_subscription, members_subscription, messages_subscription, my_call_subscription,
    my_perms_subscription, outgoing_requests_subscription, peer_invite_subscription,
    peer_session_subscription, pins_subscription, requests_subscription, roles_subscription,
    room_voice_subscription, servers_subscription, social_stats_subscription,
    suggestions_subscription, tray_subscription, typing_ping_task, typing_subscription,
    voice_users_subscription,
};
use crate::state::history;
use crate::state::message::Message;
use crate::state::session_store::{
    connect_task, load_panel_prefs, load_session_token_from_disk, talkyss_data_dir,
};
use crate::state::settings_store::{PersistedSettings, load_settings, save_settings};
use crate::state::types::{
    AdminUserRow, AuthMode, BlockedUser, BotSummary, CallRole, ChannelSummary, ChatMessage,
    ConversationSummary, Friend, FriendSuggestion, FriendsFilter, IncomingRequest, MyCallInfo,
    OutgoingRequest, PendingAttachment, PeopleHit, ProfileView, ResizePanel, ServerMemberRow,
    ServerRoleRow, ServerSettingsCategory, ServerSummary, Session, SettingsCategory, SidebarTab,
    SocialStats, VoiceUserRow,
};
use crate::ui::mentions;
use crate::update_check::check_for_update_task;
use crate::{
    AVATAR_PALETTE, MAX_BACKGROUND_PEER_SESSIONS, PEER_CLEAR_HISTORY_CTRL, scroll_chat_to_bottom,
};

pub(crate) struct App {
    pub(crate) deployment_url: String,
    pub(crate) client: Option<ConvexClient>,
    pub(crate) connect_status: String,
    pub(crate) pending_restore_token: Option<String>,

    pub(crate) auth_mode: AuthMode,
    pub(crate) username_input: String,
    pub(crate) password_input: String,
    pub(crate) display_name_input: String,
    pub(crate) auth_error: Option<String>,
    pub(crate) auth_busy: bool,

    pub(crate) session: Option<Session>,
    /// peerseal long-term identity public key (base64), published to Convex.
    pub(crate) peerseal_public_key: Option<String>,
    /// AES key for local encrypted history (from peerseal private key).
    pub(crate) history_vault_key: Option<[u8; 32]>,
    /// One live peerseal session per currently-online friend, keyed by their
    /// user_id — background sessions run for every online friend (see
    /// `App::subscription`), not just whichever DM happens to be open.
    /// Commands into each friend's live peerseal DM worker.
    pub(crate) peer_cmd_txs: HashMap<String, tokio::sync::mpsc::UnboundedSender<peer::PeerCmd>>,
    pub(crate) peer_connected: HashMap<String, bool>,
    pub(crate) peer_status: HashMap<String, String>,
    pub(crate) peer_sas: HashMap<String, String>,
    pub(crate) peer_transport: HashMap<String, String>,
    pub(crate) peer_remote_fp: HashMap<String, String>,
    /// Local live messages from peerseal (not stored on Convex), per friend.
    pub(crate) peer_live_messages: HashMap<String, Vec<ChatMessage>>,
    pub(crate) peer_photo_seq: u64,
    /// Guest invite arrived before that friend's worker command channel was ready.
    pub(crate) pending_peer_invite: HashMap<String, String>,
    /// Host publish ack arrived before that friend's PeerCmdReady.
    pub(crate) pending_invite_published: HashMap<String, Result<(), String>>,

    pub(crate) friends: Vec<Friend>,
    pub(crate) friends_filter: FriendsFilter,
    pub(crate) people_hits: Vec<PeopleHit>,
    pub(crate) suggestions: Vec<FriendSuggestion>,
    pub(crate) social_stats: SocialStats,
    pub(crate) incoming_requests: Vec<IncomingRequest>,
    pub(crate) outgoing_requests: Vec<OutgoingRequest>,
    pub(crate) blocked: Vec<BlockedUser>,
    pub(crate) conversations: Vec<ConversationSummary>,
    pub(crate) admin_users: Vec<AdminUserRow>,
    pub(crate) admin_status: Option<String>,
    pub(crate) admin_search_input: String,
    /// 0 = All, 1 = Users, 2 = Staff, 3 = Banned (client-side list filter).
    pub(crate) admin_filter: i32,
    /// Platform counters for the dashboard header (on-demand adminStats).
    pub(crate) admin_stats: Option<crate::state::types::AdminStats>,
    /// user_id whose expanded detail drawer is open in the admin list.
    pub(crate) admin_detail_user_id: Option<String>,
    /// Loaded detail for `admin_detail_user_id` (on-demand adminUserDetail).
    pub(crate) admin_user_detail: Option<crate::state::types::AdminUserDetail>,
    pub(crate) sidebar_tab: SidebarTab,
    pub(crate) chat_filter_input: String,
    pub(crate) friends_filter_input: String,
    pub(crate) add_friend_input: String,
    pub(crate) add_friend_note: String,
    pub(crate) add_friend_status: Option<String>,
    pub(crate) friend_request_busy: bool,
    pub(crate) send_busy: bool,
    /// user_id awaiting a second click on "Block" before it actually fires.
    pub(crate) confirm_block_user_id: Option<String>,
    /// Ephemeral status toast shown near the top of the main UI.
    pub(crate) toast: Option<(String, Instant)>,

    pub(crate) new_group_open: bool,
    pub(crate) new_group_name_input: String,
    pub(crate) new_group_selected: BTreeSet<String>,
    pub(crate) group_create_status: Option<String>,

    pub(crate) servers: Vec<ServerSummary>,
    pub(crate) selected_server: Option<ServerSummary>,
    pub(crate) channels: Vec<ChannelSummary>,
    pub(crate) new_server_name_input: String,
    pub(crate) join_server_code_input: String,
    pub(crate) server_status: Option<String>,
    /// Mini menu under rail + for create / join server.
    pub(crate) server_add_menu_open: bool,
    pub(crate) custom_slug_input: String,
    pub(crate) server_icon_busy: bool,
    pub(crate) new_channel_open: bool,
    pub(crate) new_channel_name_input: String,
    pub(crate) new_channel_is_voice: bool,
    pub(crate) server_settings_open: bool,
    pub(crate) server_settings_category: ServerSettingsCategory,
    pub(crate) rename_server_input: String,
    pub(crate) confirm_delete_server: bool,
    /// Overview "about" editor buffer (seeded from the server on open).
    pub(crate) server_description_input: String,
    /// Member currently armed for the transfer-ownership confirm step.
    pub(crate) confirm_transfer_owner_id: Option<String>,
    /// Cached counts for the Overview stats card (on-demand serverStats).
    pub(crate) server_stats: Option<crate::state::types::ServerStats>,
    pub(crate) server_members: Vec<ServerMemberRow>,
    pub(crate) server_roles: Vec<ServerRoleRow>,
    pub(crate) my_server_permissions: u32,
    pub(crate) new_role_name_input: String,
    /// Role currently open in the Roles settings tab's editor panel.
    pub(crate) editing_role_id: Option<String>,
    pub(crate) role_name_edit_input: String,
    pub(crate) confirm_delete_role_id: Option<String>,
    /// user_id whose "add role" flyout is currently open (Members tab / profile).
    pub(crate) member_role_picker_open: Option<String>,
    pub(crate) voice_users: Vec<VoiceUserRow>,
    pub(crate) active_voice_channel: Option<String>,
    /// Status line while in a multi-party voice room.
    pub(crate) room_voice_status: Option<String>,
    /// Unsealed TGK1 conversation keys for groups/channels.
    pub(crate) group_key_store: Option<crypto::GroupKeyStore>,
    /// Effective: whether this open chat stores history for me (UI toggle).
    pub(crate) chat_store_enabled: bool,
    /// Whether Convex will actually store (all members allow).
    pub(crate) chat_store_allowed: bool,

    /// Right-hand members drawer (server view).
    pub(crate) members_panel_open: bool,
    pub(crate) members_panel_width: f32,
    pub(crate) members_panel_target: f32,
    /// User-adjustable expanded width for the members drawer (drag-resize).
    pub(crate) members_panel_preferred_width: f32,
    /// User-adjustable width of the middle channel/sidebar list (drag-resize).
    pub(crate) channel_list_width: f32,
    /// Which panel a drag-resize is currently in progress for, if any.
    pub(crate) resizing_panel: Option<ResizePanel>,
    /// (cursor_x_at_drag_start, width_at_drag_start) — set on the first
    /// move sample after a drag starts, to avoid an initial jump.
    pub(crate) resize_drag_anchor: Option<(f32, f32)>,

    pub(crate) my_bots: Vec<BotSummary>,
    pub(crate) new_bot_name_input: String,
    pub(crate) bot_invite_username_input: String,
    pub(crate) bot_status: Option<String>,
    /// Shown once after create/regenerate — user should copy.
    pub(crate) bot_token_reveal: Option<String>,
    pub(crate) renaming_channel_id: Option<String>,
    pub(crate) rename_channel_input: String,

    pub(crate) active_conversation: Option<String>,
    pub(crate) active_conversation_kind: Option<String>,
    pub(crate) active_conversation_peer_id: Option<String>,
    pub(crate) active_peer_name: Option<String>,
    pub(crate) messages: Vec<ChatMessage>,
    /// Pinned messages of the open conversation (live `messages:listPinned`
    /// watch; decrypted on arrival like history rows). Backs the header panel.
    pub(crate) pinned_messages: Vec<ChatMessage>,
    /// Whether the chat header's pinned-messages panel is open.
    pub(crate) pins_panel_open: bool,
    pub(crate) message_input: String,
    /// @-autocomplete suggestions for the composer (display names matching
    /// the "@prefix" token being typed; includes "everyone" where the
    /// active conversation supports it). Empty = popup hidden.
    pub(crate) mention_suggestions: Vec<String>,
    pub(crate) pending_attachment: Option<PendingAttachment>,
    pub(crate) pending_reply: Option<(String, String, String)>,
    pub(crate) chat_error: Option<String>,
    pub(crate) editing_message_id: Option<String>,
    pub(crate) editing_message_encrypted: bool,
    pub(crate) hovered_message_id: Option<String>,
    /// URL of attachment shown in the fullscreen lightbox (if any).
    pub(crate) attachment_preview_url: Option<String>,
    /// Two-step confirm for "Clear chat" in the header.
    pub(crate) clear_chat_confirm: bool,
    pub(crate) clear_chat_busy: bool,

    pub(crate) seen_last_message_at: HashMap<String, i64>,
    /// `last_message_at` value for which a markRead mutation was already sent,
    /// per conversation. Prevents the markRead -> watch -> markRead loop.
    pub(crate) last_marked_read_at: HashMap<String, i64>,
    pub(crate) conversations_loaded: bool,
    pub(crate) requests_loaded: bool,

    pub(crate) my_call: Option<MyCallInfo>,
    pub(crate) call_role: Option<CallRole>,
    pub(crate) call_engine_key: Option<String>,
    pub(crate) call_muted: Arc<AtomicBool>,
    pub(crate) call_output_muted: Arc<AtomicBool>,
    pub(crate) call_status_text: Option<String>,
    pub(crate) settings_input_device: Option<String>,
    pub(crate) settings_output_device: Option<String>,
    pub(crate) noise_gate: Arc<AtomicU32>,
    /// Per-peer voice volume gains (peer user_id -> gain, 0.0..=2.0). The
    /// 1:1 call remote uses the special key "*". Applied live to decoded
    /// remote audio in call.rs / room_voice.rs.
    pub(crate) voice_gains: Arc<std::sync::Mutex<std::collections::HashMap<String, f32>>>,

    pub(crate) share_control_tx: Option<tokio::sync::mpsc::UnboundedSender<call::ShareCommand>>,
    pub(crate) share_control_slot:
        Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<call::ShareCommand>>>>,
    pub(crate) share_picker_open: bool,
    pub(crate) share_targets: Vec<screenshare::ShareTarget>,
    pub(crate) is_sharing: bool,
    /// Prefer including loopback system audio when sharing (if device exists).
    pub(crate) share_system_audio: bool,
    /// Viewer muted the remote share stream audio.
    pub(crate) remote_stream_muted: bool,
    /// Go-live quality line (fps / kbps).
    pub(crate) share_stats_line: String,
    pub(crate) command_palette_open: bool,
    pub(crate) command_palette_query: String,
    /// (conversation_id, display_line, message_id)
    pub(crate) command_palette_hits: Vec<(String, String, String)>,
    pub(crate) remote_share_frame: Option<Arc<[u8]>>,
    pub(crate) share_view_expanded: bool,

    pub(crate) viewing_profile: Option<ProfileView>,
    pub(crate) profile_error: Option<String>,

    pub(crate) settings_open: bool,
    pub(crate) settings_category: SettingsCategory,
    pub(crate) settings_display_name_input: String,
    pub(crate) settings_status_input: String,
    pub(crate) settings_bio_input: String,
    pub(crate) settings_avatar_color: String,
    pub(crate) settings_profile_status: Option<String>,
    pub(crate) settings_current_password_input: String,
    pub(crate) settings_new_password_input: String,
    pub(crate) settings_confirm_password_input: String,
    pub(crate) settings_password_status: Option<String>,
    pub(crate) settings_input_devices: Vec<String>,
    pub(crate) settings_output_devices: Vec<String>,

    pub(crate) avatar_image_cache: HashMap<String, Arc<[u8]>>,
    pub(crate) avatar_upload_busy: bool,

    pub(crate) typing_names: Vec<String>,
    pub(crate) typing_active: bool,
    pub(crate) last_typing_ping: Option<Instant>,

    /// Downloaded new build waiting to be swapped in on next real quit
    /// (see `stage_exe_swap` in src/update_check.rs).
    pub(crate) pending_update_path: Option<std::path::PathBuf>,
    pub(crate) update_check_status: Option<String>,
    pub(crate) ping_status: Option<String>,

    /// Whether the tray icon came up successfully — if not, there'd be no
    /// way to reopen or quit a window hidden-to-tray, so closing should
    /// just exit instead (see `Message::WindowCloseRequested`).
    pub(crate) tray_ready: bool,
    /// Whether the main window currently has OS focus — used to decide
    /// whether an incoming live peer message needs a desktop
    /// notification/sound (no point if you're already looking at it).
    pub(crate) window_focused: bool,
    /// Window-level side effect `update()` wants performed against the real
    /// `AppWindow` handle (which it doesn't have access to) -- consumed by
    /// the pump loop in `main.rs` right after the `update()` call. See
    /// `crate::rt::WindowAction`.
    pub(crate) pending_window_action: Option<WindowAction>,
}

impl App {
    pub(crate) fn new(deployment_url: String) -> (Self, Task<Message>) {
        let connect_url = deployment_url.clone();
        let task = connect_task(connect_url);
        let pending_restore_token = load_session_token_from_disk();
        let (channel_list_width, members_panel_preferred_width) = load_panel_prefs();
        let persisted_settings = load_settings();
        (
            Self {
                deployment_url,
                client: None,
                connect_status: "Connecting to Convex...".to_string(),
                pending_restore_token,
                auth_mode: AuthMode::Login,
                username_input: String::new(),
                password_input: String::new(),
                display_name_input: String::new(),
                auth_error: None,
                auth_busy: false,
                session: None,
                peerseal_public_key: None,
                history_vault_key: None,
                peer_cmd_txs: HashMap::new(),
                peer_connected: HashMap::new(),
                peer_status: HashMap::new(),
                peer_sas: HashMap::new(),
                peer_transport: HashMap::new(),
                peer_remote_fp: HashMap::new(),
                peer_live_messages: HashMap::new(),
                peer_photo_seq: 0,
                pending_peer_invite: HashMap::new(),
                pending_invite_published: HashMap::new(),
                friends: Vec::new(),
                friends_filter: FriendsFilter::All,
                people_hits: Vec::new(),
                suggestions: Vec::new(),
                social_stats: SocialStats::default(),
                incoming_requests: Vec::new(),
                outgoing_requests: Vec::new(),
                blocked: Vec::new(),
                conversations: Vec::new(),
                admin_users: Vec::new(),
                admin_status: None,
                admin_search_input: String::new(),
                admin_filter: 0,
                admin_stats: None,
                admin_detail_user_id: None,
                admin_user_detail: None,
                sidebar_tab: SidebarTab::Chats,
                chat_filter_input: String::new(),
                friends_filter_input: String::new(),
                add_friend_input: String::new(),
                add_friend_note: String::new(),
                add_friend_status: None,
                friend_request_busy: false,
                send_busy: false,
                confirm_block_user_id: None,
                toast: None,
                new_group_open: false,
                new_group_name_input: String::new(),
                new_group_selected: BTreeSet::new(),
                group_create_status: None,
                servers: Vec::new(),
                selected_server: None,
                channels: Vec::new(),
                new_server_name_input: String::new(),
                join_server_code_input: String::new(),
                server_status: None,
                server_add_menu_open: false,
                custom_slug_input: String::new(),
                server_icon_busy: false,
                new_channel_open: false,
                new_channel_name_input: String::new(),
                new_channel_is_voice: false,
                server_settings_open: false,
                server_settings_category: ServerSettingsCategory::Overview,
                rename_server_input: String::new(),
                confirm_delete_server: false,
                server_description_input: String::new(),
                confirm_transfer_owner_id: None,
                server_stats: None,
                server_members: Vec::new(),
                server_roles: Vec::new(),
                my_server_permissions: 0,
                new_role_name_input: String::new(),
                editing_role_id: None,
                role_name_edit_input: String::new(),
                confirm_delete_role_id: None,
                member_role_picker_open: None,
                voice_users: Vec::new(),
                active_voice_channel: None,
                room_voice_status: None,
                group_key_store: None,
                chat_store_enabled: true,
                chat_store_allowed: true,
                members_panel_open: true,
                members_panel_width: members_panel_preferred_width,
                members_panel_target: members_panel_preferred_width,
                members_panel_preferred_width,
                channel_list_width,
                resizing_panel: None,
                resize_drag_anchor: None,
                my_bots: Vec::new(),
                new_bot_name_input: String::new(),
                bot_invite_username_input: String::new(),
                bot_status: None,
                bot_token_reveal: None,
                renaming_channel_id: None,
                rename_channel_input: String::new(),
                active_conversation: None,
                active_conversation_kind: None,
                active_conversation_peer_id: None,
                active_peer_name: None,
                messages: Vec::new(),
                pinned_messages: Vec::new(),
                pins_panel_open: false,
                message_input: String::new(),
                mention_suggestions: Vec::new(),
                pending_attachment: None,
                pending_reply: None,
                chat_error: None,
                editing_message_id: None,
                editing_message_encrypted: false,
                hovered_message_id: None,
                attachment_preview_url: None,
                clear_chat_confirm: false,
                clear_chat_busy: false,
                seen_last_message_at: HashMap::new(),
                last_marked_read_at: HashMap::new(),
                conversations_loaded: false,
                requests_loaded: false,
                my_call: None,
                call_role: None,
                call_engine_key: None,
                call_muted: Arc::new(AtomicBool::new(false)),
                call_output_muted: Arc::new(AtomicBool::new(false)),
                call_status_text: None,
                settings_input_device: persisted_settings.input_device.clone(),
                settings_output_device: persisted_settings.output_device.clone(),
                noise_gate: Arc::new(AtomicU32::new(
                    persisted_settings
                        .noise_gate
                        .unwrap_or(call::DEFAULT_NOISE_GATE)
                        .to_bits(),
                )),
                voice_gains: Arc::new(std::sync::Mutex::new(
                    persisted_settings.voice_gains.clone(),
                )),
                share_control_tx: None,
                share_control_slot: Arc::new(std::sync::Mutex::new(None)),
                share_picker_open: false,
                share_targets: Vec::new(),
                is_sharing: false,
                share_system_audio: true,
                remote_stream_muted: false,
                share_stats_line: String::new(),
                command_palette_open: false,
                command_palette_query: String::new(),
                command_palette_hits: Vec::new(),
                remote_share_frame: None,
                share_view_expanded: false,
                settings_open: false,
                settings_category: SettingsCategory::Account,
                settings_display_name_input: String::new(),
                viewing_profile: None,
                profile_error: None,
                settings_status_input: String::new(),
                settings_bio_input: String::new(),
                settings_avatar_color: AVATAR_PALETTE[0].to_string(),
                settings_profile_status: None,
                settings_current_password_input: String::new(),
                settings_new_password_input: String::new(),
                settings_confirm_password_input: String::new(),
                settings_password_status: None,
                settings_input_devices: Vec::new(),
                settings_output_devices: Vec::new(),
                avatar_image_cache: HashMap::new(),
                avatar_upload_busy: false,
                typing_names: Vec::new(),
                typing_active: false,
                last_typing_ping: None,
                pending_update_path: None,
                update_check_status: None,
                ping_status: None,
                tray_ready: false,
                window_focused: true,
                pending_window_action: None,
            },
            Task::batch([task, check_for_update_task()]),
        )
    }

    pub(super) fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    /// Display names the current user can be pinged by (display name +
    /// username) -- the candidate set for "does this body mention me?".
    pub(crate) fn my_mention_names(&self) -> Vec<String> {
        self.session
            .as_ref()
            .map(|s| vec![s.display_name.clone(), s.username.clone()])
            .unwrap_or_default()
    }

    /// Recomputes the composer @-autocomplete popup contents from the
    /// current input + active conversation: server members in channels,
    /// friends in groups/DMs, plus an "everyone" entry only where the token
    /// actually pings the room (channels/groups, not 1:1 DMs).
    pub(crate) fn compute_mention_suggestions(&self) -> Vec<String> {
        let mut candidates: Vec<String> = Vec::new();
        match self.active_conversation_kind.as_deref() {
            Some("channel") | Some("voice") => {
                candidates.extend(self.server_members.iter().map(|m| m.display_name.clone()));
                candidates.push("everyone".to_string());
            }
            Some("group") => {
                candidates.extend(self.friends.iter().map(|f| f.label().to_string()));
                candidates.push("everyone".to_string());
            }
            _ => {
                // 1:1 DM: just the peer (fallback: all friends while the
                // conversation is still resolving).
                if let Some(peer_id) = &self.active_conversation_peer_id {
                    if let Some(f) = self.friends.iter().find(|f| &f.user_id == peer_id) {
                        candidates.push(f.label().to_string());
                    }
                }
                if candidates.is_empty() {
                    candidates.extend(self.friends.iter().map(|f| f.label().to_string()));
                }
            }
        }
        mentions::suggest(&self.message_input, &candidates, 8)
    }

    /// `(display_name, user_id)` candidates for the active conversation --
    /// the id-carrying counterpart of `compute_mention_suggestions`' name
    /// list, used at send time to resolve @names into mention metadata.
    pub(crate) fn mention_id_candidates(&self) -> Vec<(String, String)> {
        match self.active_conversation_kind.as_deref() {
            Some("channel") | Some("voice") => self
                .server_members
                .iter()
                .map(|m| (m.display_name.clone(), m.user_id.clone()))
                .collect(),
            Some("group") => self
                .friends
                .iter()
                .map(|f| (f.label().to_string(), f.user_id.clone()))
                .collect(),
            _ => {
                // 1:1 DM: just the peer (fallback: all friends while the
                // conversation is still resolving).
                if let Some(peer_id) = &self.active_conversation_peer_id {
                    if let Some(f) = self.friends.iter().find(|f| &f.user_id == peer_id) {
                        return vec![(f.label().to_string(), f.user_id.clone())];
                    }
                }
                self.friends
                    .iter()
                    .map(|f| (f.label().to_string(), f.user_id.clone()))
                    .collect()
            }
        }
    }

    /// Mention metadata for an outgoing message body: mentioned user ids +
    /// whether `@everyone` pings (channels/groups only -- same gate as the
    /// render-time highlight). Computed from the PLAINTEXT body, i.e. before
    /// any group/channel encryption happens.
    pub(crate) fn outgoing_mentions(&self, body: &str) -> (Vec<String>, bool) {
        let (mut ids, everyone) = mentions::resolve_mentions(body, &self.mention_id_candidates());
        // Your own messages never count as pinging you.
        if let Some(session) = &self.session {
            ids.retain(|id| id != &session.user_id);
        }
        let everyone_ok = matches!(
            self.active_conversation_kind.as_deref(),
            Some("channel") | Some("group")
        );
        (ids, everyone && everyone_ok)
    }

    /// Snapshot the user-facing audio settings to disk (settings.json).
    /// Called from `update()` whenever one of them changes; cheap enough
    /// (a few hundred bytes of JSON) that no debouncing is needed.
    pub(super) fn persist_settings(&self) {
        save_settings(&PersistedSettings {
            input_device: self.settings_input_device.clone(),
            output_device: self.settings_output_device.clone(),
            noise_gate: Some(f32::from_bits(
                self.noise_gate.load(std::sync::atomic::Ordering::Relaxed),
            )),
            voice_gains: self
                .voice_gains
                .lock()
                .map(|gains| gains.clone())
                .unwrap_or_default(),
        });
    }

    pub(super) fn load_conversation_store_pref(&self) -> Task<Message> {
        let (Some(client), Some(session), Some(conversation_id)) = (
            self.client.clone(),
            self.session.clone(),
            self.active_conversation.clone(),
        ) else {
            return Task::none();
        };
        let mut client = client;
        Task::perform(
            async move {
                let result = client
                    .query(
                        "prefs:getConversationStore",
                        btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                            "conversationId".to_string() => Value::String(conversation_id),
                        },
                    )
                    .await
                    .map_err(|err| err.to_string())?;
                match result {
                    FunctionResult::Value(Value::Object(obj)) => {
                        let store = obj.get("store").map(value_as_bool).unwrap_or(true);
                        let allows = obj
                            .get("conversationAllowsStorage")
                            .map(value_as_bool)
                            .unwrap_or(true);
                        Ok((store, allows))
                    }
                    FunctionResult::ErrorMessage(e) => Err(e),
                    _ => Err("Unexpected response".into()),
                }
            },
            |r| match r {
                Ok((s, a)) => Message::ConversationStorePrefLoaded(s, a),
                Err(_) => Message::ConversationStorePrefLoaded(true, true),
            },
        )
    }

    /// Drop in-memory chat + any leftover local vault/cache/ratchet files for
    /// this conversation (or the whole pair). Convex is cleared separately.
    pub(super) fn wipe_local_chat_history(&mut self) {
        let data_dir = talkyss_data_dir();
        if let Some(session) = self.session.as_ref() {
            if let Some(conv_id) = self.active_conversation.as_deref() {
                history::wipe_chat(&session.user_id, conv_id);
            }
            // Chat "logs" left under %APPDATA%/Talkyss after DMs:
            // decrypt_cache_*, ratchet_v3_*, legacy ratchet_*.
            if let Some(peer_id) = self.active_conversation_peer_id.as_deref() {
                crypto::DecryptCache::clear(&data_dir, &session.user_id, peer_id);
                crypto::RatchetSession::clear(&data_dir, &session.user_id, peer_id);
            }
        }
        // Drop vault media handles for this chat from the image cache.
        self.avatar_image_cache
            .retain(|url, _| !history::is_media_url_tag(url));
        self.messages.clear();
        self.peer_live_messages.clear();
        self.pending_attachment = None;
        self.pending_reply = None;
        self.editing_message_id = None;
        self.message_input.clear();
        self.hovered_message_id = None;
        self.clear_chat_confirm = false;
    }

    pub(super) fn reset_session(&mut self) {
        self.session = None;
        self.friends.clear();
        self.incoming_requests.clear();
        self.outgoing_requests.clear();
        self.blocked.clear();
        self.conversations.clear();
        self.admin_users.clear();
        self.admin_status = None;
        self.admin_search_input.clear();
        self.admin_filter = 0;
        self.admin_stats = None;
        self.admin_detail_user_id = None;
        self.admin_user_detail = None;
        self.sidebar_tab = SidebarTab::Chats;
        self.chat_filter_input.clear();
        self.friends_filter_input.clear();
        self.friend_request_busy = false;
        self.send_busy = false;
        self.toast = None;
        self.new_group_open = false;
        self.new_group_name_input.clear();
        self.new_group_selected.clear();
        self.group_create_status = None;
        self.active_conversation = None;
        self.active_conversation_kind = None;
        self.active_conversation_peer_id = None;
        self.active_peer_name = None;
        self.stop_all_peer_sessions();
        self.active_voice_channel = None;
        self.room_voice_status = None;
        self.voice_users.clear();
        self.group_key_store = None;
        self.peerseal_public_key = None;
        self.history_vault_key = None;
        self.messages.clear();
        self.pinned_messages.clear();
        self.pins_panel_open = false;
        self.message_input.clear();
        self.add_friend_input.clear();
        self.add_friend_note.clear();
        self.add_friend_status = None;
        self.chat_error = None;
        self.editing_message_id = None;
        self.hovered_message_id = None;
        self.pending_attachment = None;
        self.pending_reply = None;
        self.clear_chat_confirm = false;
        self.clear_chat_busy = false;
        self.seen_last_message_at.clear();
        self.last_marked_read_at.clear();
        self.conversations_loaded = false;
        self.requests_loaded = false;
        self.my_call = None;
        self.call_role = None;
        self.call_engine_key = None;
        self.call_muted = Arc::new(AtomicBool::new(false));
        self.call_output_muted = Arc::new(AtomicBool::new(false));
        self.call_status_text = None;
        self.clear_share_ui();
        self.settings_open = false;
        self.settings_category = SettingsCategory::Account;
        self.settings_profile_status = None;
        self.settings_password_status = None;
        self.settings_current_password_input.clear();
        self.settings_new_password_input.clear();
        self.settings_confirm_password_input.clear();
        self.viewing_profile = None;
        self.profile_error = None;
        self.username_input.clear();
        self.password_input.clear();
        self.display_name_input.clear();
        self.auth_error = None;
        self.auth_busy = false;
        self.typing_names.clear();
        self.typing_active = false;
        self.last_typing_ping = None;
        self.servers.clear();
        self.selected_server = None;
        self.channels.clear();
        self.new_server_name_input.clear();
        self.join_server_code_input.clear();
        self.server_status = None;
        self.new_channel_open = false;
        self.new_channel_name_input.clear();
        self.server_settings_open = false;
        self.server_settings_category = ServerSettingsCategory::Overview;
        self.confirm_delete_server = false;
        self.server_description_input.clear();
        self.confirm_transfer_owner_id = None;
        self.server_stats = None;
        self.server_members.clear();
        self.renaming_channel_id = None;
        self.rename_channel_input.clear();
    }

    pub(super) fn fetch_missing_avatars(
        &self,
        urls: impl IntoIterator<Item = String>,
    ) -> Task<Message> {
        self.fetch_missing_images(urls.into_iter().map(|url| (url, None, None)))
    }

    /// Fetch images; when key/nonce are set the bytes are E2EE attachment
    /// ciphertext and get decrypted before landing in the image cache.
    pub(super) fn fetch_missing_images(
        &self,
        jobs: impl IntoIterator<Item = (String, Option<String>, Option<String>)>,
    ) -> Task<Message> {
        let mut tasks = Vec::new();
        for (url, att_key, att_nonce) in jobs {
            if url.is_empty() || self.avatar_image_cache.contains_key(&url) {
                continue;
            }
            let url_for_result = url.clone();
            tasks.push(Task::perform(
                async move {
                    let result = reqwest::get(&url).await;
                    match result {
                        Ok(response) => {
                            let bytes = response
                                .bytes()
                                .await
                                .map(|b| b.to_vec())
                                .map_err(|err| err.to_string())?;
                            if let (Some(key), Some(nonce)) = (att_key, att_nonce) {
                                crypto::decrypt_attachment(&key, &nonce, &bytes)
                                    .ok_or_else(|| "Failed to decrypt attachment".to_string())
                            } else {
                                Ok(bytes)
                            }
                        }
                        Err(err) => Err(err.to_string()),
                    }
                },
                move |result| Message::AvatarImageLoaded(url_for_result.clone(), result),
            ));
        }
        Task::batch(tasks)
    }

    /// If we were signalling "typing" in the current conversation, tell the
    /// server we stopped, and reset the debounce state.
    pub(super) fn stop_typing_task(&mut self) -> Task<Message> {
        if !self.typing_active {
            return Task::none();
        }
        self.typing_active = false;
        self.last_typing_ping = None;
        let Some(conversation_id) = self.active_conversation.clone() else {
            return Task::none();
        };
        typing_ping_task(&self.client, &self.session, conversation_id, false)
    }

    /// Prepares fresh screen-share state for a call that's about to start.
    /// The receiver half is stashed behind a mutex so `call_subscription`'s
    /// long-lived async block -- which only actually runs once per call,
    /// thanks to `Subscription::run_with_id` dedup -- can pull it out even
    /// though `subscription(&self)` only gets a shared reference.
    pub(super) fn reset_share_state(&mut self) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.share_control_tx = Some(tx);
        self.share_control_slot = Arc::new(std::sync::Mutex::new(Some(rx)));
        self.share_picker_open = false;
        self.share_targets.clear();
        self.is_sharing = false;
        self.remote_share_frame = None;
        self.share_view_expanded = false;
    }

    /// Clears screen-share UI state when a call ends. Doesn't touch
    /// `share_control_tx`/`share_control_slot` -- those get replaced
    /// wholesale by `reset_share_state` the next time a call starts.
    pub(super) fn clear_share_ui(&mut self) {
        self.share_picker_open = false;
        self.is_sharing = false;
        self.remote_share_frame = None;
        self.share_view_expanded = false;
    }

    /// Loads peerseal identity and publishes its public key for fingerprint UI.
    pub(super) fn ensure_identity_key(&mut self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        let public_key = match peer::load_peerseal_identity(&session.user_id) {
            Ok((id, b64)) => {
                self.peerseal_public_key = Some(b64.clone());
                self.history_vault_key =
                    Some(history::vault_key_from_identity_private(&id.private));
                b64
            }
            Err(err) => {
                self.chat_error = Some(format!("peerseal identity: {err}"));
                return Task::none();
            }
        };

        let mut client = client;
        Task::perform(
            async move {
                client
                    .mutation(
                        "profile:setPublicKey",
                        btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                            "publicKey".to_string() => Value::String(public_key),
                        },
                    )
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(expect_null)
            },
            |_result| Message::PublicKeyUploaded,
        )
    }

    /// Tears down every background peer session (logout only — sessions
    /// otherwise run for as long as a friend stays online, independent of
    /// which DM is open; see `App::subscription` and the reaping logic in
    /// the `Message::FriendsUpdated` handler for the per-friend case).
    fn stop_all_peer_sessions(&mut self) {
        for tx in self.peer_cmd_txs.values() {
            let _ = tx.send(peer::PeerCmd::Shutdown);
        }
        self.peer_cmd_txs.clear();
        self.peer_connected.clear();
        self.peer_status.clear();
        self.peer_sas.clear();
        self.peer_transport.clear();
        self.peer_remote_fp.clear();
        self.peer_live_messages.clear();
        self.pending_peer_invite.clear();
        self.pending_invite_published.clear();
    }

    /// Shuts down and forgets one friend's background peer session (they
    /// went offline, or were unfriended/blocked).
    pub(super) fn stop_peer_session_for(&mut self, peer_id: &str) {
        if let Some(tx) = self.peer_cmd_txs.remove(peer_id) {
            let _ = tx.send(peer::PeerCmd::Shutdown);
        }
        self.peer_connected.remove(peer_id);
        self.peer_status.remove(peer_id);
        self.peer_sas.remove(peer_id);
        self.peer_transport.remove(peer_id);
        self.peer_remote_fp.remove(peer_id);
        self.peer_live_messages.remove(peer_id);
        self.pending_peer_invite.remove(peer_id);
        self.pending_invite_published.remove(peer_id);
    }

    fn persist_peer_message(&self, _msg: &ChatMessage, _photo_bytes: Option<&[u8]>) {
        // No local vault — durable history is written to Convex by the sender.
    }

    /// `peer_id` is the friend this event is about — since sessions now run
    /// in the background for every online friend simultaneously, events can
    /// arrive for any of them, not just whichever DM is currently open.
    pub(super) fn handle_peer_event(
        &mut self,
        peer_id: String,
        ev: peer::PeerEvent,
    ) -> Task<Message> {
        let is_viewing = self.active_conversation_peer_id.as_deref() == Some(peer_id.as_str());
        match ev {
            peer::PeerEvent::Status(s) => {
                self.peer_status.insert(peer_id, s);
                Task::none()
            }
            peer::PeerEvent::HostInvite {
                payload,
                expires_at_ms,
            } => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conversation_id) = self
                    .conversations
                    .iter()
                    .find(|c| {
                        c.kind == "direct" && c.peer_user_id.as_deref() == Some(peer_id.as_str())
                    })
                    .map(|c| c.conversation_id.clone())
                else {
                    // Silently dropping this would strand the host worker in
                    // `cmd_rx.recv()` forever, waiting for InvitePublished /
                    // InvitePublishFailed that never comes. Report the failure
                    // through the normal path so it can retry instead.
                    return Task::done(Message::PeerInvitePublished(
                        peer_id,
                        Err("no direct conversation yet".to_string()),
                    ));
                };
                self.peer_status
                    .insert(peer_id.clone(), "Publishing invite…".into());
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "peer:publishInvite",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                    "invitePayload".to_string() => Value::String(payload),
                                    "expiresAt".to_string() => Value::Float64(expires_at_ms as f64),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    move |result| Message::PeerInvitePublished(peer_id.clone(), result),
                )
            }
            peer::PeerEvent::Connected {
                sas_emojis,
                transport,
                remote_fingerprint,
            } => {
                self.peer_connected.insert(peer_id.clone(), true);
                self.peer_sas.insert(peer_id.clone(), sas_emojis);
                self.peer_transport.insert(peer_id.clone(), transport);
                self.peer_remote_fp
                    .insert(peer_id.clone(), remote_fingerprint);
                self.peer_status
                    .insert(peer_id, "Secure channel ready".into());
                if is_viewing {
                    self.chat_error = None;
                }
                Task::none()
            }
            peer::PeerEvent::Text(text) => {
                // Remote peer requested a mutual history wipe.
                if text == PEER_CLEAR_HISTORY_CTRL {
                    if is_viewing {
                        self.wipe_local_chat_history();
                        self.show_toast("Peer cleared this chat");
                    }
                    return Task::none();
                }
                let author = self
                    .friends
                    .iter()
                    .find(|f| f.user_id == peer_id)
                    .map(|f| f.label().to_string())
                    .unwrap_or_else(|| "Peer".into());
                let live_count = self
                    .peer_live_messages
                    .get(&peer_id)
                    .map(Vec::len)
                    .unwrap_or(0);
                let msg = ChatMessage {
                    id: format!(
                        "peer-in-{}-{}",
                        chrono::Local::now().timestamp_millis(),
                        live_count
                    ),
                    author_id: peer_id.clone(),
                    author_name: author,
                    author_avatar_color: String::new(),
                    author_avatar_url: String::new(),
                    author_is_bot: false,
                    body: text,
                    kind: "text".into(),
                    attachment_url: String::new(),
                    attachment_key: None,
                    attachment_nonce: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    encrypted: true,
                    sent_at: chrono::Local::now().timestamp_millis(),
                    deleted: false,
                    edited: false,
                    pinned: false,
                };
                if !(self.window_focused && is_viewing) {
                    play_beep(BEEP_MESSAGE);
                    if mentions::mentions_any(&msg.body, &self.my_mention_names()) {
                        notify_desktop(
                            &format!("{} mentioned you", msg.author_name),
                            &mentions::snippet(&msg.body, 140),
                        );
                    } else {
                        notify_desktop("Talkyss", &format!("New message · {}", msg.author_name));
                    }
                }
                self.persist_peer_message(&msg, None);
                self.peer_live_messages
                    .entry(peer_id)
                    .or_default()
                    .push(msg);
                if is_viewing {
                    scroll_chat_to_bottom()
                } else {
                    Task::none()
                }
            }
            peer::PeerEvent::Photo {
                bytes,
                content_type: _,
            } => {
                let msg_id = format!(
                    "peer-photo-{}-{}",
                    chrono::Local::now().timestamp_millis(),
                    self.peer_photo_seq
                );
                self.peer_photo_seq += 1;
                let url = history::media_url_tag(&msg_id);
                self.avatar_image_cache
                    .insert(url.clone(), Arc::from(bytes.clone()));
                let author = self
                    .friends
                    .iter()
                    .find(|f| f.user_id == peer_id)
                    .map(|f| f.label().to_string())
                    .unwrap_or_else(|| "Peer".into());
                let msg = ChatMessage {
                    id: msg_id,
                    author_id: peer_id.clone(),
                    author_name: author,
                    author_avatar_color: String::new(),
                    author_avatar_url: String::new(),
                    author_is_bot: false,
                    body: String::new(),
                    kind: "text".into(),
                    attachment_url: url,
                    attachment_key: None,
                    attachment_nonce: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    encrypted: true,
                    sent_at: chrono::Local::now().timestamp_millis(),
                    deleted: false,
                    edited: false,
                    pinned: false,
                };
                if !(self.window_focused && is_viewing) {
                    play_beep(BEEP_MESSAGE);
                    notify_desktop("Talkyss", &format!("New message · {}", msg.author_name));
                }
                self.persist_peer_message(&msg, Some(&bytes));
                self.peer_live_messages
                    .entry(peer_id)
                    .or_default()
                    .push(msg);
                if is_viewing {
                    scroll_chat_to_bottom()
                } else {
                    Task::none()
                }
            }
            peer::PeerEvent::Error(err) => {
                self.peer_connected.insert(peer_id.clone(), false);
                self.peer_status.insert(peer_id, err.clone());
                if is_viewing {
                    self.chat_error = Some(err);
                }
                Task::none()
            }
            peer::PeerEvent::Disconnected => {
                self.peer_connected.insert(peer_id.clone(), false);
                self.peer_status
                    .entry(peer_id.clone())
                    .or_insert_with(|| "Secure channel closed".into());
                self.peer_cmd_txs.remove(&peer_id);
                Task::none()
            }
        }
    }

    pub(super) fn push_local_peer_message(
        &mut self,
        session: &Session,
        peer_id: &str,
        body: String,
        photo: Option<Vec<u8>>,
        content_type: String,
    ) {
        let live_count = self
            .peer_live_messages
            .get(peer_id)
            .map(Vec::len)
            .unwrap_or(0);
        let msg_id = format!(
            "peer-out-{}-{}",
            chrono::Local::now().timestamp_millis(),
            live_count
        );
        let mut attachment_url = String::new();
        let photo_for_vault = photo.clone();
        if let Some(bytes) = photo {
            let url = history::media_url_tag(&msg_id);
            self.avatar_image_cache
                .insert(url.clone(), Arc::from(bytes));
            attachment_url = url;
            let _ = content_type;
        }
        let msg = ChatMessage {
            id: msg_id,
            author_id: session.user_id.clone(),
            author_name: session.display_name.clone(),
            author_avatar_color: session.avatar_color.clone(),
            author_avatar_url: session.avatar_image_url.clone(),
            author_is_bot: false,
            body,
            kind: "text".into(),
            attachment_url,
            attachment_key: None,
            attachment_nonce: None,
            reactions: Vec::new(),
            reply_to: None,
            encrypted: true,
            sent_at: chrono::Local::now().timestamp_millis(),
            deleted: false,
            edited: false,
            pinned: false,
        };
        self.persist_peer_message(&msg, photo_for_vault.as_deref());
        self.peer_live_messages
            .entry(peer_id.to_string())
            .or_default()
            .push(msg);
    }

    /// Decrypt group/channel TGK1 bodies (and legacy TKR3 DM blobs if any).
    /// Live DMs use peerseal and arrive via `peer_live_messages`.
    pub(super) fn decrypt_incoming_messages(
        &mut self,
        messages: Vec<ChatMessage>,
    ) -> Vec<ChatMessage> {
        let Some(conversation_id) = self.active_conversation.clone() else {
            return messages;
        };
        let kind = self.active_conversation_kind.as_deref().unwrap_or("");
        let is_groupish = kind == "group" || kind == "channel" || kind == "voice";
        if !is_groupish {
            return messages;
        }

        let key_info = self
            .group_key_store
            .as_ref()
            .and_then(|s| s.get(&conversation_id));

        messages
            .into_iter()
            .map(|mut msg| {
                // Decrypt reply snippets even when the parent body is plain.
                if let Some((author, snippet)) = msg.reply_to.take() {
                    if crypto::looks_like_group_blob(&snippet) {
                        if let Some((_epoch, key)) = key_info {
                            if let Some(plain) =
                                crypto::decrypt_group_message(&key, &conversation_id, &snippet)
                            {
                                let text = crypto::MessagePayload::decode(&plain)
                                    .map(|p| p.text)
                                    .unwrap_or(plain);
                                let short = if text.chars().count() > 80 {
                                    format!("{}…", text.chars().take(80).collect::<String>())
                                } else {
                                    text
                                };
                                msg.reply_to = Some((author, short));
                            } else {
                                msg.reply_to =
                                    Some((author, DECRYPT_FAILED_PLACEHOLDER.to_string()));
                            }
                        } else {
                            msg.reply_to = Some((author, DECRYPT_FAILED_PLACEHOLDER.to_string()));
                        }
                    } else {
                        msg.reply_to = Some((author, snippet));
                    }
                }

                if !msg.encrypted || msg.deleted || msg.kind == "call" {
                    return msg;
                }
                if crypto::looks_like_group_blob(&msg.body) {
                    if let Some((_epoch, key)) = key_info {
                        if let Some(plain) =
                            crypto::decrypt_group_message(&key, &conversation_id, &msg.body)
                        {
                            apply_decrypted_payload(&mut msg, &plain);
                            return msg;
                        }
                    }
                    msg.body = DECRYPT_FAILED_PLACEHOLDER.to_string();
                    msg.attachment_key = None;
                    msg.attachment_nonce = None;
                }
                msg
            })
            .collect()
    }

    /// Load or bootstrap the shared group key for the open group/channel.
    pub(super) fn ensure_group_key(&mut self) -> Task<Message> {
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        let Some(conversation_id) = self.active_conversation.clone() else {
            return Task::none();
        };
        let kind = self.active_conversation_kind.as_deref().unwrap_or("");
        if kind != "group" && kind != "channel" && kind != "voice" {
            return Task::none();
        }

        if self.group_key_store.is_none() {
            self.group_key_store = Some(crypto::GroupKeyStore::load(
                &talkyss_data_dir(),
                &session.user_id,
            ));
        }
        if self
            .group_key_store
            .as_ref()
            .and_then(|s| s.get(&conversation_id))
            .is_some()
        {
            return Task::none();
        }

        let identity = match peer::load_peerseal_identity(&session.user_id) {
            Ok((id, _)) => crypto::IdentityKeyPair::from_bytes(id.private),
            Err(err) => {
                return Task::done(Message::GroupKeyReady(Err(format!(
                    "Identity for group crypto: {err}"
                ))));
            }
        };

        let conv_for_store = conversation_id.clone();
        Task::perform(
            async move {
                ensure_group_key_async(client, session.token, conversation_id, identity)
                    .await
                    .map(|(epoch, key)| (conv_for_store, epoch, key))
            },
            |result| match result {
                Ok((cid, epoch, key)) => Message::GroupKeyLoaded(cid, epoch, key),
                Err(e) => Message::GroupKeyReady(Err(e)),
            },
        )
    }
}

/// Fetch my sealed package or bootstrap a new group key for all members.
async fn ensure_group_key_async(
    mut client: ConvexClient,
    token: String,
    conversation_id: String,
    identity: crypto::IdentityKeyPair,
) -> Result<(u32, [u8; 32]), String> {
    use convex::{FunctionResult, Value};
    use maplit::btreemap;

    // 1) Existing package for me?
    let pkg = client
        .query(
            "groupKeys:myPackage",
            btreemap! {
                "sessionToken".to_string() => Value::String(token.clone()),
                "conversationId".to_string() => Value::String(conversation_id.clone()),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    if let FunctionResult::Value(Value::Object(obj)) = &pkg {
        let epoch = match obj.get("epoch") {
            Some(Value::Float64(n)) => *n as u32,
            Some(Value::Int64(n)) => *n as u32,
            _ => 0,
        };
        let sealed = match obj.get("sealedKey") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        let eph = match obj.get("ephPublicKey") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        if epoch > 0 && !sealed.is_empty() && !eph.is_empty() {
            let key = crypto::unseal_group_key(&identity, eph, sealed)
                .ok_or_else(|| "Could not unseal group key (wrong identity?)".to_string())?;
            return Ok((epoch, key));
        }
    }

    // 2) Bootstrap: list member public keys, generate key, seal for each.
    let members_result = client
        .query(
            "groupKeys:listMemberPublicKeys",
            btreemap! {
                "sessionToken".to_string() => Value::String(token.clone()),
                "conversationId".to_string() => Value::String(conversation_id.clone()),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    let root = match members_result {
        FunctionResult::Value(Value::Object(root)) => root,
        FunctionResult::ErrorMessage(err) => return Err(humanize_error(&err)),
        FunctionResult::ConvexError(err) => return Err(humanize_error(&format!("{err:?}"))),
        _ => return Err("Unexpected server response while listing member keys".to_string()),
    };
    let existing_epoch = match root.get("epoch") {
        Some(Value::Float64(n)) => *n as u32,
        Some(Value::Int64(n)) => *n as u32,
        _ => 0,
    };
    // Race: someone else just published — re-fetch package.
    if existing_epoch > 0 {
        let pkg = client
            .query(
                "groupKeys:myPackage",
                btreemap! {
                    "sessionToken".to_string() => Value::String(token.clone()),
                    "conversationId".to_string() => Value::String(conversation_id.clone()),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        if let FunctionResult::Value(Value::Object(obj)) = pkg {
            let epoch = match obj.get("epoch") {
                Some(Value::Float64(n)) => *n as u32,
                Some(Value::Int64(n)) => *n as u32,
                _ => 0,
            };
            let sealed = match obj.get("sealedKey") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let eph = match obj.get("ephPublicKey") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            if epoch > 0 && !sealed.is_empty() {
                let key = crypto::unseal_group_key(&identity, &eph, &sealed)
                    .ok_or_else(|| "Could not unseal group key".to_string())?;
                return Ok((epoch, key));
            }
        }
        return Err(
            "Group key exists but this device has no package yet — ask a member to re-open the chat"
                .into(),
        );
    }

    let members = match root.get("members") {
        Some(Value::Array(a)) => a,
        _ => return Err("No members for group key".into()),
    };

    let group_key = crypto::generate_group_key();
    let mut packages = Vec::new();
    for m in members {
        let Value::Object(obj) = m else { continue };
        let user_id = match obj.get("userId") {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let public_key = match obj.get("publicKey") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        if public_key.len() != 44 {
            continue;
        }
        if let Some((eph, sealed)) = crypto::seal_group_key_for(&public_key, &group_key) {
            packages.push(btreemap! {
                "userId".to_string() => Value::String(user_id),
                "sealedKey".to_string() => Value::String(sealed),
                "ephPublicKey".to_string() => Value::String(eph),
            });
        }
    }
    if packages.is_empty() {
        return Err(
            "No members have a public key yet — everyone must open the app once to publish keys"
                .into(),
        );
    }

    let pkg_values: Vec<Value> = packages.into_iter().map(Value::Object).collect();
    let result = client
        .mutation(
            "groupKeys:publishPackages",
            btreemap! {
                "sessionToken".to_string() => Value::String(token.clone()),
                "conversationId".to_string() => Value::String(conversation_id.clone()),
                "epoch".to_string() => Value::Float64(1.0),
                "packages".to_string() => Value::Array(pkg_values),
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    match result {
        FunctionResult::Value(Value::Object(obj)) => {
            let created = matches!(obj.get("created"), Some(Value::Boolean(true)));
            let epoch = match obj.get("epoch") {
                Some(Value::Float64(n)) => *n as u32,
                Some(Value::Int64(n)) => *n as u32,
                _ => 1,
            };
            if created {
                return Ok((epoch, group_key));
            }
            // Lost race — fetch package sealed by the winner.
            let pkg = client
                .query(
                    "groupKeys:myPackage",
                    btreemap! {
                        "sessionToken".to_string() => Value::String(token),
                        "conversationId".to_string() => Value::String(conversation_id),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            if let FunctionResult::Value(Value::Object(obj)) = pkg {
                let sealed = match obj.get("sealedKey") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let eph = match obj.get("ephPublicKey") {
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let key = crypto::unseal_group_key(&identity, &eph, &sealed)
                    .ok_or_else(|| "Could not unseal group key after race".to_string())?;
                return Ok((epoch, key));
            }
            Err("Group key bootstrap race failed".into())
        }
        FunctionResult::ErrorMessage(msg) => Err(msg),
        _ => Err("Group key publish failed".into()),
    }
}

impl App {
    /// The full set of desired background jobs for the current state --
    /// former `Subscription`s, now plain `Job`s the pump loop in `main.rs`
    /// reconciles against a `rt::SubscriptionRegistry` after every
    /// `update()` call (same dedup-by-id semantics `Subscription::run_with_id`
    /// had). `tx` is where every job sends its resulting `Message`s.
    pub(crate) fn subscription(&self, tx: UnboundedSender<Message>) -> Vec<Job> {
        // Always on, even before login: the tray icon needs to exist so the
        // close button can minimize to it instead of quitting outright. The
        // periodic update re-check also runs pre-login -- the app can update
        // itself even from the auth screen.
        let mut subs = vec![tray_subscription(tx.clone()), update_check_job(tx.clone())];

        let (Some(client), Some(session)) = (self.client.clone(), self.session.clone()) else {
            return subs;
        };

        subs.extend([
            friends_subscription(client.clone(), session.token.clone(), tx.clone()),
            requests_subscription(client.clone(), session.token.clone(), tx.clone()),
            outgoing_requests_subscription(client.clone(), session.token.clone(), tx.clone()),
            social_stats_subscription(client.clone(), session.token.clone(), tx.clone()),
            suggestions_subscription(client.clone(), session.token.clone(), tx.clone()),
            blocked_subscription(client.clone(), session.token.clone(), tx.clone()),
            conversations_subscription(client.clone(), session.token.clone(), tx.clone()),
            servers_subscription(client.clone(), session.token.clone(), tx.clone()),
            my_call_subscription(client.clone(), session.token.clone(), tx.clone()),
            tick_job(tx.clone()),
        ]);

        if session.is_admin || session.is_moderator {
            subs.push(admin_users_subscription(
                client.clone(),
                session.token.clone(),
                tx.clone(),
            ));
        }

        if let Some(server) = &self.selected_server {
            subs.push(channels_subscription(
                client.clone(),
                session.token.clone(),
                server.server_id.clone(),
                tx.clone(),
            ));
            // Always keep member roster live while a server is open.
            subs.push(members_subscription(
                client.clone(),
                session.token.clone(),
                server.server_id.clone(),
                tx.clone(),
            ));
            subs.push(roles_subscription(
                client.clone(),
                session.token.clone(),
                server.server_id.clone(),
                tx.clone(),
            ));
            if self.server_settings_open {
                subs.push(my_perms_subscription(
                    client.clone(),
                    session.token.clone(),
                    server.server_id.clone(),
                    tx.clone(),
                ));
            }
        }

        // Voice roster for the open room (server VC or group) and any room
        // we're still connected to after navigating away.
        if let Some(conversation_id) = self.active_voice_channel.as_ref().or_else(|| {
            matches!(
                self.active_conversation_kind.as_deref(),
                Some("voice") | Some("group")
            )
            .then(|| self.active_conversation.as_ref())
            .flatten()
        }) {
            subs.push(voice_users_subscription(
                client.clone(),
                session.token.clone(),
                conversation_id.clone(),
                tx.clone(),
            ));
        }

        // Multi-party WebRTC mesh while joined to a voice room.
        if let Some(room_id) = &self.active_voice_channel {
            subs.push(room_voice_subscription(
                room_id.clone(),
                client.clone(),
                session.token.clone(),
                session.user_id.clone(),
                room_id.clone(),
                self.settings_input_device.clone(),
                self.settings_output_device.clone(),
                Arc::clone(&self.call_muted),
                Arc::clone(&self.call_output_muted),
                Arc::clone(&self.noise_gate),
                self.voice_gains.clone(),
                tx.clone(),
            ));
        }

        // Smooth members drawer open/close.
        if (self.members_panel_width - self.members_panel_target).abs() > 0.6 {
            subs.push(animate_members_panel_job(tx.clone()));
        }

        if let Some(conversation_id) = &self.active_conversation {
            subs.push(messages_subscription(
                client.clone(),
                session.token.clone(),
                conversation_id.clone(),
                tx.clone(),
            ));
            subs.push(pins_subscription(
                client.clone(),
                session.token.clone(),
                conversation_id.clone(),
                tx.clone(),
            ));
            subs.push(typing_subscription(
                client.clone(),
                session.token.clone(),
                conversation_id.clone(),
                tx.clone(),
            ));
        }

        // peerseal: one live DM worker per online friend, running in the
        // background regardless of which screen is open — not just for
        // whichever DM happens to be `active_conversation` (that used to
        // require both sides to have the same chat open simultaneously to
        // ever connect at all). Capped defensively so a huge friends list
        // can't open unbounded concurrent Noise/relay sessions.
        let mut scheduled_peers: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for friend in self
            .friends
            .iter()
            .filter(|f| f.is_online_like())
            .take(MAX_BACKGROUND_PEER_SESSIONS)
        {
            let Some(conversation_id) = self.conversations.iter().find_map(|c| {
                (c.kind == "direct" && c.peer_user_id.as_deref() == Some(friend.user_id.as_str()))
                    .then(|| c.conversation_id.clone())
            }) else {
                continue;
            };
            scheduled_peers.insert(friend.user_id.clone());
            subs.push(peer_session_subscription(
                session.user_id.clone(),
                friend.user_id.clone(),
                conversation_id.clone(),
                tx.clone(),
            ));
            // Guest always watches Convex for host invites.
            if !peer::is_peerseal_host(&session.user_id, &friend.user_id) {
                subs.push(peer_invite_subscription(
                    client.clone(),
                    session.token.clone(),
                    friend.user_id.clone(),
                    conversation_id,
                    tx.clone(),
                ));
            }
        }
        // The friends loop above misses direct conversations whose peer
        // isn't (or isn't yet) a mutual friend — support DMs in particular
        // (`openSupportDm` deliberately bypasses the friends system), but
        // also any residual DM left over from an unfriended contact. Those
        // must still be able to connect while actively open, or the header
        // is stuck showing "Connecting secure channel…" forever since no
        // worker was ever started for that peer at all.
        if self.active_conversation_kind.as_deref() == Some("direct") {
            if let (Some(peer_id), Some(conversation_id)) =
                (&self.active_conversation_peer_id, &self.active_conversation)
            {
                if !scheduled_peers.contains(peer_id) {
                    subs.push(peer_session_subscription(
                        session.user_id.clone(),
                        peer_id.clone(),
                        conversation_id.clone(),
                        tx.clone(),
                    ));
                    if !peer::is_peerseal_host(&session.user_id, peer_id) {
                        subs.push(peer_invite_subscription(
                            client.clone(),
                            session.token.clone(),
                            peer_id.clone(),
                            conversation_id.clone(),
                            tx.clone(),
                        ));
                    }
                }
            }
        }

        // Ring while an incoming call is waiting to be answered.
        if self
            .my_call
            .as_ref()
            .map(|c| c.status == "ringing" && !c.is_caller)
            .unwrap_or(false)
        {
            subs.push(ring_tick_job(tx.clone()));
        }

        if let (Some(role), Some(key)) = (&self.call_role, &self.call_engine_key) {
            subs.push(call_subscription(
                key.clone(),
                client.clone(),
                session.token.clone(),
                role.clone(),
                self.settings_input_device.clone(),
                self.settings_output_device.clone(),
                Arc::clone(&self.call_muted),
                Arc::clone(&self.call_output_muted),
                Arc::clone(&self.noise_gate),
                Arc::clone(&self.share_control_slot),
                self.voice_gains.clone(),
                tx.clone(),
            ));
        }

        subs
    }
}

/// Periodic 5s tick -- ping measurement / housekeeping (`Message::Tick`).
fn tick_job(tx: UnboundedSender<Message>) -> Job {
    job("tick", async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if tx.send(Message::Tick).is_err() {
                break;
            }
        }
    })
}

/// Re-checks for updates every 30 minutes in the background. The boot-time
/// check already runs at startup, so the immediate first interval tick is
/// consumed up front; `Message::CheckForUpdate` itself skips re-downloading
/// once an update is staged.
fn update_check_job(tx: UnboundedSender<Message>) -> Job {
    job("update-check", async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
        interval.tick().await; // swallow the immediate first tick
        loop {
            interval.tick().await;
            if tx.send(Message::CheckForUpdate).is_err() {
                break;
            }
        }
    })
}

/// 2s ring cadence while an incoming call is waiting to be answered.
fn ring_tick_job(tx: UnboundedSender<Message>) -> Job {
    job("ring-tick", async move {
        let mut interval = tokio::time::interval(Duration::from_millis(2000));
        loop {
            interval.tick().await;
            if tx.send(Message::RingTick).is_err() {
                break;
            }
        }
    })
}

/// ~60fps drive for the members-drawer open/close animation, only running
/// while `members_panel_width` hasn't settled on `members_panel_target`.
fn animate_members_panel_job(tx: UnboundedSender<Message>) -> Job {
    job("animate-members-panel", async move {
        let mut interval = tokio::time::interval(Duration::from_millis(16));
        loop {
            interval.tick().await;
            if tx.send(Message::AnimateMembersPanel).is_err() {
                break;
            }
        }
    })
}
