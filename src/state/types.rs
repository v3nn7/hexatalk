//! Plain data types shared across the app: session/user info, chat/server
//! domain rows as parsed from Convex, and small UI-state enums (which tab is
//! selected, which settings category, etc). No behavior beyond a couple of
//! tiny `Friend` display helpers.

/// A presence timestamp within this many ms of "now" counts as online.
///
/// Must stay aligned with Convex `friends.ONLINE_MS` (90s) and the server
/// heartbeat write throttle (`presence.HEARTBEAT_WRITE_INTERVAL_MS` = 30s).
/// A shorter client window (the old 15s) marked people offline between
/// heartbeats even though the backend still treated them as online — members
/// list / peer dots flickered "offline" constantly.
const ONLINE_THRESHOLD_MS: i64 = 90_000;

/// `last_seen_at` is ms since epoch (0 = never seen).
pub(crate) fn is_online(last_seen_at: i64) -> bool {
    if last_seen_at <= 0 {
        return false;
    }
    let now = chrono::Utc::now().timestamp_millis();
    (now - last_seen_at) < ONLINE_THRESHOLD_MS
}

// ---------- Domain types ----------

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) token: String,
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    /// user | moderator | admin
    pub(crate) platform_role: String,
    pub(crate) is_admin: bool,
    pub(crate) is_moderator: bool,
    pub(crate) avatar_color: String,
    pub(crate) status_message: String,
    pub(crate) bio: String,
    pub(crate) avatar_image_url: String,
    /// When false, Convex will not persist chats involving this user.
    pub(crate) store_chat_history: bool,
    pub(crate) hide_online_status: bool,
    pub(crate) friends_only_dms: bool,
    pub(crate) discoverable: bool,
    /// everyone | mutual_servers | nobody
    pub(crate) friend_request_privacy: String,
    /// online | idle | dnd | invisible
    pub(crate) presence_status: String,
    pub(crate) email: String,
    pub(crate) email_verified: bool,
    /// HexaTalk Plus (cosmetic). From auth:me / refreshed via plus:getMyStatus.
    pub(crate) plus_active: bool,
    /// ms epoch; 0 if inactive.
    pub(crate) plus_expires_at: i64,
    pub(crate) profile_banner_url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Friend {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) last_seen_at: i64,
    /// online | idle | dnd | offline
    pub(crate) presence: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) public_key: String,
    pub(crate) status_message: String,
    pub(crate) nickname: String,
    pub(crate) favorite: bool,
    pub(crate) private_note: String,
    pub(crate) friends_since: i64,
    pub(crate) mutual_servers: Vec<String>,
    pub(crate) is_staff: bool,
}

impl Friend {
    pub(crate) fn label(&self) -> &str {
        if self.nickname.is_empty() {
            &self.display_name
        } else {
            &self.nickname
        }
    }

    pub(crate) fn is_online_like(&self) -> bool {
        matches!(self.presence.as_str(), "online" | "idle" | "dnd") || is_online(self.last_seen_at)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockedUser {
    pub(crate) user_id: String,
    pub(crate) display_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IncomingRequest {
    pub(crate) request_id: String,
    pub(crate) from_user_id: String,
    pub(crate) from_username: String,
    pub(crate) from_display_name: String,
    pub(crate) from_avatar_color: String,
    pub(crate) from_avatar_image_url: String,
    pub(crate) note: String,
    pub(crate) sent_at: i64,
    pub(crate) from_status_message: String,
    pub(crate) mutual_servers: Vec<String>,
    pub(crate) presence: String,
    pub(crate) is_staff: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PeopleHit {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) status_message: String,
    pub(crate) presence: String,
    pub(crate) relation: String,
    pub(crate) incoming_request_id: String,
    pub(crate) mutual_servers: Vec<String>,
    pub(crate) is_staff: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FriendSuggestion {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) status_message: String,
    pub(crate) presence: String,
    pub(crate) mutual_servers: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SocialStats {
    pub(crate) friends_total: u32,
    pub(crate) friends_online: u32,
    pub(crate) incoming_pending: u32,
    pub(crate) outgoing_pending: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct OutgoingRequest {
    pub(crate) request_id: String,
    pub(crate) to_user_id: String,
    pub(crate) to_username: String,
    pub(crate) to_display_name: String,
    pub(crate) to_avatar_color: String,
    pub(crate) to_avatar_image_url: String,
    pub(crate) note: String,
    pub(crate) sent_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationSummary {
    pub(crate) conversation_id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) peer_user_id: Option<String>,
    pub(crate) last_message_at: i64,
    pub(crate) unread: bool,
    /// Unread messages that @-mention me (or @everyone) -- the red sidebar
    /// badge. 0 pre-deploy / for old messages without mention metadata.
    pub(crate) mention_count: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct MyCallInfo {
    pub(crate) call_id: String,
    pub(crate) is_caller: bool,
    pub(crate) status: String,
    pub(crate) peer_display_name: String,
    pub(crate) offer_sdp: String,
}

#[derive(Debug, Clone)]
pub(crate) enum CallRole {
    Caller {
        conversation_id: String,
        callee_id: String,
    },
    Callee {
        call_id: String,
        offer_sdp: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum AvatarPick {
    Cancelled,
    TooLarge,
    Ready(Vec<u8>, String),
}

#[derive(Debug, Clone)]
pub(crate) enum AttachmentPick {
    Cancelled,
    TooLarge,
    Ready(Vec<u8>, String),
}

#[derive(Clone)]
pub(crate) struct PendingAttachment {
    pub(crate) bytes: Vec<u8>,
    pub(crate) content_type: String,
}

impl std::fmt::Debug for PendingAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PendingAttachment({} bytes)", self.bytes.len())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    pub(crate) author_id: String,
    pub(crate) author_name: String,
    pub(crate) author_avatar_color: String,
    pub(crate) author_avatar_url: String,
    pub(crate) author_is_bot: bool,
    pub(crate) author_plus_active: bool,
    pub(crate) body: String,
    pub(crate) kind: String,
    pub(crate) attachment_url: String,
    /// When set, `attachment_url` points at ciphertext; decrypt with these
    /// before showing (keys travel inside the E2EE message envelope).
    pub(crate) attachment_key: Option<String>,
    pub(crate) attachment_nonce: Option<String>,
    pub(crate) reactions: Vec<(String, u32, bool)>,
    pub(crate) reply_to: Option<(String, String)>,
    pub(crate) encrypted: bool,
    pub(crate) sent_at: i64,
    pub(crate) deleted: bool,
    pub(crate) edited: bool,
    pub(crate) pinned: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileView {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) status_message: String,
    pub(crate) bio: String,
    pub(crate) last_seen_at: i64,
    pub(crate) presence: String,
    pub(crate) is_staff: bool,
    pub(crate) is_friend: bool,
    pub(crate) can_support_dm: bool,
    pub(crate) relation: String,
    pub(crate) request_id: String,
    pub(crate) mutual_servers: Vec<String>,
    pub(crate) favorite: bool,
    pub(crate) nickname: String,
    pub(crate) private_note: String,
    pub(crate) plus_active: bool,
    pub(crate) profile_banner_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FriendsFilter {
    All,
    Online,
    Favorites,
}

#[derive(Debug, Clone)]
pub(crate) struct AdminUserRow {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) role: String,
    pub(crate) banned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AuthMode {
    Login,
    Register,
    /// Logged-out password reset via email code.
    ForgotPassword,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SidebarTab {
    Chats,
    Friends,
    Requests,
    Servers,
    Admin,
}

/// Result of `servers:resolveCustomSlug` for a `vyrapp://join/<slug>` deep
/// link -- just enough to render the join-confirmation dialog and, if
/// accepted, drive it through the existing `joinByInviteCode` mutation.
#[derive(Debug, Clone)]
pub(crate) struct DeepLinkJoinInfo {
    pub(crate) server_id: String,
    pub(crate) name: String,
    pub(crate) icon_url: String,
    pub(crate) invites_paused: bool,
    /// Empty when `invites_paused` is true.
    pub(crate) invite_code: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerSummary {
    pub(crate) server_id: String,
    pub(crate) name: String,
    pub(crate) is_owner: bool,
    pub(crate) invite_code: String,
    pub(crate) icon_url: String,
    pub(crate) custom_slug: String,
    /// Owner-editable "about" blurb (may be empty).
    pub(crate) description: String,
    /// Server creation time, ms since epoch (from Convex `_creationTime`).
    pub(crate) created_at: i64,
    /// Conversation id a new member lands in first ("" = fall back to first
    /// text channel).
    pub(crate) welcome_channel_id: String,
    /// When true the public invite code is dormant (no new joins by code).
    pub(crate) invites_paused: bool,
}

/// Read-only per-server counters for the Overview stats card (on-demand
/// `servers:serverStats` query, not a live subscription).
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerStats {
    pub(crate) member_count: i64,
    pub(crate) text_channels: i64,
    pub(crate) voice_channels: i64,
    pub(crate) role_count: i64,
    pub(crate) message_count: i64,
    pub(crate) messages_capped: bool,
    pub(crate) created_at: i64,
    pub(crate) oldest_member_name: String,
    pub(crate) oldest_member_joined_at: i64,
}

/// Platform-wide counters for the admin dashboard header (on-demand
/// `admin:adminStats` query).
#[derive(Debug, Clone, Default)]
pub(crate) struct AdminStats {
    pub(crate) total_users: i64,
    pub(crate) online: i64,
    pub(crate) banned: i64,
    pub(crate) staff: i64,
    pub(crate) bots: i64,
    pub(crate) servers: i64,
}

/// One reported message, shown in the admin panel's Reports queue
/// (`reports:adminListReports`). Staff-only, log-and-review — never an
/// automatic ban.
#[derive(Debug, Clone)]
pub(crate) struct MessageReport {
    pub(crate) report_id: String,
    pub(crate) message_id: String,
    pub(crate) conversation_label: String,
    pub(crate) reporter_username: String,
    pub(crate) author_username: String,
    pub(crate) message_body: String,
    pub(crate) reason: String,
    pub(crate) status: String,
    pub(crate) created_at: i64,
}

/// Expanded detail for one user, shown in the admin per-user drawer
/// (on-demand `admin:adminUserDetail` query).
#[derive(Debug, Clone, Default)]
pub(crate) struct AdminUserDetail {
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) role: String,
    pub(crate) banned: bool,
    pub(crate) is_bot: bool,
    pub(crate) bio: String,
    pub(crate) status_message: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) created_at: i64,
    pub(crate) online: bool,
    pub(crate) last_seen_at: i64,
    pub(crate) server_names: Vec<String>,
    pub(crate) friend_count: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelSummary {
    pub(crate) conversation_id: String,
    pub(crate) name: String,
    pub(crate) channel_type: String, // "text" | "voice"
    /// Unread messages that @-mention me (or @everyone) -- red sidebar badge.
    pub(crate) mention_count: u32,
    pub(crate) category_id: String,
    pub(crate) position: i64,
    pub(crate) is_announcement: bool,
    pub(crate) is_system: bool,
    pub(crate) muted: bool,
    pub(crate) can_send: bool,
    pub(crate) permissions: u32,
}

/// One custom role explicitly assigned to a member (the implicit
/// @everyone role is never included — see convex/roles.ts).
#[derive(Debug, Clone)]
pub(crate) struct MemberRoleTag {
    pub(crate) role_id: String,
    pub(crate) name: String,
    pub(crate) color: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerMemberRow {
    pub(crate) user_id: String,
    pub(crate) display_name: String,
    pub(crate) username: String,
    pub(crate) avatar_color: String,
    pub(crate) avatar_image_url: String,
    pub(crate) is_owner: bool,
    pub(crate) is_bot: bool,
    /// user | moderator | admin (platform staff badge)
    pub(crate) platform_role: String,
    pub(crate) plus_active: bool,
    pub(crate) last_seen_at: i64,
    /// All custom roles this member currently holds (multi-role, Discord-style).
    pub(crate) roles: Vec<MemberRoleTag>,
}

#[derive(Debug, Clone)]
pub(crate) struct BotSummary {
    pub(crate) bot_id: String,
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) avatar_color: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ServerRoleRow {
    pub(crate) role_id: String,
    pub(crate) name: String,
    pub(crate) color: String,
    pub(crate) position: i64,
    pub(crate) permissions: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceUserRow {
    pub(crate) user_id: String,
    pub(crate) display_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerSettingsCategory {
    Overview,
    Channels,
    Members,
    Roles,
    Invites,
    Danger,
}

/// Which panel a drag-resize is currently adjusting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizePanel {
    ChannelList,
    Members,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsCategory {
    Account,
    Privacy,
    Plus,
    Bots,
    Voice,
    Appearance,
    About,
}

/// Permission bits — keep in sync with convex/roles.ts.
pub(crate) const PERM_VIEW_CHANNELS: u32 = 1 << 0;
pub(crate) const PERM_SEND_MESSAGES: u32 = 1 << 1;
pub(crate) const PERM_MANAGE_CHANNELS: u32 = 1 << 2;
pub(crate) const PERM_KICK_MEMBERS: u32 = 1 << 3;
pub(crate) const PERM_MANAGE_ROLES: u32 = 1 << 4;
pub(crate) const PERM_MANAGE_SERVER: u32 = 1 << 5;
pub(crate) const PERM_CONNECT_VOICE: u32 = 1 << 6;
pub(crate) const PERM_SPEAK: u32 = 1 << 7;
pub(crate) const PERM_ANNOUNCE: u32 = 1 << 8;
