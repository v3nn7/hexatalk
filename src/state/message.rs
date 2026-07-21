//! The `Message` enum: every event the update loop can react to (button
//! clicks, text input changes, async task results, Convex subscription
//! pushes, call/tray/peer events, ...). One flat enum by design -- iced's
//! Elm architecture routes everything through a single `update(Message)`,
//! so splitting this further would just scatter match arms without any
//! real decoupling benefit.

use crate::net::api::ApiClient;

use crate::media::call;
use crate::media::screenshare;
use crate::net::peer;
use crate::state::types::{
    AdminUserRow, AttachmentPick, AuthMode, AvatarPick, BlockedUser, BotSummary, ChannelSummary,
    ChatMessage, ConversationSummary, Friend, FriendSuggestion, FriendsFilter, IncomingRequest,
    MyCallInfo, OutgoingRequest, PeopleHit, ProfileView, ResizePanel, ServerMemberRow,
    ServerRoleRow, ServerSettingsCategory, ServerSummary, Session, SettingsCategory, SidebarTab,
    SocialStats, VoiceUserRow,
};
use crate::tray;
use crate::update_check::UpdateOutcome;

// ---------- Messages ----------

#[derive(Clone)]
pub(crate) enum Message {
    Connected(ApiClient),
    ConnectFailed(String),

    SwitchAuthMode(AuthMode),
    UsernameInputChanged(String),
    PasswordInputChanged(String),
    DisplayNameInputChanged(String),
    EmailInputChanged(String),
    /// Confirm field on forgot-password (new password again).
    PasswordConfirmInputChanged(String),
    /// 6-digit code on forgot-password step 2.
    PasswordResetCodeInputChanged(String),
    SubmitAuth,
    AuthFinished(Result<Session, String>),
    RestoreFinished(Result<Session, String>),
    PublicKeyUploaded,
    /// Forgot-password: code email requested (always "ok" from server).
    PasswordResetCodeSent(Result<(), String>),
    /// Forgot-password: new password applied; client should return to login.
    PasswordResetFinished(Result<(), String>),

    // ---- Email verification gate ----
    EmailVerifyInputChanged(String),
    EmailVerifyCodeInputChanged(String),
    RequestEmailVerification,
    RequestEmailVerificationFinished(Result<(), String>),
    SubmitEmailVerificationCode,
    VerifyEmailCodeFinished(Result<(), String>),
    /// "Change email" link on the code step -- goes back to the email step
    /// without a round trip (no code has been confirmed yet either way).
    ChangeEmailVerifyAddress,

    /// peerseal live-channel events — background sessions run per online
    /// friend, so every message here is tagged with which friend (user_id)
    /// it's about, not just whichever DM happens to be open.
    PeerEvent(String, peer::PeerEvent),
    /// Worker ready — hold the command channel for sends.
    PeerCmdReady(String, tokio::sync::mpsc::UnboundedSender<peer::PeerCmd>),
    /// Guest: Convex delivered a peerseal invite for this friend's DM.
    PeerInviteUpdated(String, Option<String>),
    PeerInvitePublished(String, Result<(), String>),
    /// Fire-and-forget: makes sure an online friend has a DM conversation
    /// row to background-connect over. Result is ignored either way — the
    /// row (if newly created) flows back in via `conversations_subscription`.
    DirectConversationEnsured(Result<String, String>),

    CheckForUpdate,
    UpdateCheckFinished(UpdateOutcome),
    /// "Restart & install" button: swap in the staged update and relaunch.
    RestartAndUpdate,
    MeasurePing,
    PingMeasured(Option<u64>),
    WindowCloseRequested,
    TrayEvent(tray::TrayEvent),
    WindowFocusChanged(bool),

    FriendsUpdated(Vec<Friend>),
    RequestsUpdated(Vec<IncomingRequest>),
    OutgoingRequestsUpdated(Vec<OutgoingRequest>),
    SocialStatsUpdated(SocialStats),
    SuggestionsUpdated(Vec<FriendSuggestion>),
    PeopleSearchFinished(Result<Vec<PeopleHit>, String>),
    BlockedUpdated(Vec<BlockedUser>),
    SetFriendsFilter(FriendsFilter),
    ToggleFavorite(String),
    FavoriteToggled(Result<(String, bool), String>),
    RespondAllIncoming(bool),
    RespondAllFinished(Result<u32, String>),
    CyclePresenceStatus,
    ConversationsUpdated(Vec<ConversationSummary>),
    AdminUsersUpdated(Vec<AdminUserRow>),
    MessagesUpdated(Vec<ChatMessage>),
    /// Pinned messages of the open conversation (messages:listPinned watch).
    PinnedMessagesUpdated(Vec<ChatMessage>),
    /// Header "Pinned" button -- opens/closes the pinned-messages panel.
    TogglePinsPanel,
    PinMessage(String),
    UnpinMessage(String),
    PinToggled(Result<(), String>),

    /// Opens the reason picker under a message (message_id).
    ArmReportMessage(String),
    CancelReportMessage,
    /// message_id, message body (client-supplied — may be E2EE plaintext
    /// the server never sees otherwise), reason.
    SubmitMessageReport(String, String, String),
    MessageReportFinished(Result<(), String>),
    LoadAdminReports,
    AdminReportsUpdated(Result<Vec<crate::state::types::MessageReport>, String>),
    /// report_id, resolution ("actioned" | "dismissed").
    AdminResolveReport(String, String),
    AdminResolveReportFinished(Result<(), String>),

    SidebarTabChanged(SidebarTab),
    MessageHovered(Option<String>),
    AdminSearchInputChanged(String),
    AddFriendInputChanged(String),
    AddFriendNoteChanged(String),
    SendFriendRequest,
    FriendRequestFinished(Result<(), String>),
    /// Sends a request straight to this username, bypassing the Friends-tab
    /// search box entirely -- for "Add friend" on a profile card, where
    /// there's no visible search input to route through.
    SendFriendRequestToUser(String),
    ProfileFriendRequestFinished(Result<(), String>),
    RespondRequest(String, bool),
    RequestRespondFinished(Result<(), String>),
    CancelOutgoingRequest(String),
    CancelOutgoingFinished(Result<(), String>),
    RemoveFriend(String),
    RemoveFriendFinished(Result<(), String>),
    /// "Block" is a two-step confirm everywhere it appears: this arms it.
    ConfirmBlockUser(String),
    CancelBlockUser,
    BlockUser(String),
    BlockFinished(Result<(), String>),
    UnblockUser(String),
    UnblockFinished,
    OpenSupportDm(String),
    /// (title, peer_user_id, conversation_id)
    SupportDmOpened(Result<(String, String, String), String>),
    CycleFriendRequestPrivacy,

    /// user_id -- looked up in `self.friends` by the handler (Slint callbacks
    /// only ever hand back plain ids, not whole domain rows).
    OpenConversationWithFriend(String),
    /// conversation_id -- looked up in `self.conversations`.
    OpenConversationDirect(String),
    ConversationOpened(Result<(String, Option<String>, String), String>),
    MarkReadFinished,

    ToggleGroupPanel,
    GroupNameInputChanged(String),
    ToggleGroupMember(String),
    CreateGroup,
    GroupCreateFinished(Result<(String, String), String>),

    ServersUpdated(Vec<ServerSummary>),
    NewServerNameChanged(String),
    CreateServer,
    CreateServerFinished(Result<(), String>),
    JoinServerCodeChanged(String),
    JoinServer,
    JoinServerFinished(Result<(), String>),
    /// server_id -- looked up in `self.servers`.
    SelectServer(String),
    BackToServerList,
    /// Home / DMs rail button (clears selected server).
    GoHome,
    ToggleServerAddMenu,
    PickServerIcon,
    ServerIconPicked(AvatarPick),
    /// Ok = new public icon URL (may be empty if storage had no URL yet).
    ServerIconUploadFinished(Result<String, String>),
    RemoveServerIcon,
    ServerIconRemoveFinished(Result<(), String>),
    CustomSlugInputChanged(String),
    SaveCustomSlug,
    ClearCustomSlug,
    CustomSlugFinished(Result<String, String>),

    /// `vyrapp://join/<slug>` arrived, either as this process's own argv
    /// (cold start) or forwarded over the single-instance loopback socket
    /// from a second launch (see `main.rs`'s deep-link listener).
    DeepLinkReceived(String),
    DeepLinkResolved(Result<Option<crate::state::types::DeepLinkJoinInfo>, String>),
    ConfirmJoinDeepLink,
    JoinDeepLinkFinished(Result<(), String>),
    DismissJoinDialog,
    CopyInviteCode(String),
    /// Same as `CopyInviteCode`, but copies the shareable `hexatalk://invite/<code>`
    /// link instead of the bare code.
    CopyInviteLink(String),
    ChannelsUpdated(Vec<ChannelSummary>),
    /// conversation_id -- looked up in `self.channels`.
    OpenChannel(String),
    ToggleNewChannelInput,
    NewChannelNameChanged(String),
    CreateChannel,
    CreateChannelFinished(Result<(), String>),
    ToggleServerSettings,
    RenameServerInputChanged(String),
    RenameServer,
    RenameServerFinished(Result<(), String>),
    RegenerateInviteCode,
    RegenerateInviteCodeFinished(Result<(), String>),
    ToggleConfirmDeleteServer,
    DeleteServer,
    DeleteServerFinished(Result<(), String>),
    ServerSettingsCategoryChanged(ServerSettingsCategory),
    // ---- Server description ----
    ServerDescriptionInputChanged(String),
    SaveServerDescription,
    SaveServerDescriptionFinished(Result<(), String>),
    // ---- Transfer ownership (Danger Zone) ----
    /// user_id of the member the owner is about to hand the server to (arms
    /// the confirm step); passing "" cancels.
    ConfirmTransferOwnership(String),
    TransferOwnership(String),
    TransferOwnershipFinished(Result<(), String>),
    // ---- Defaults: welcome channel + invite pause ----
    SetWelcomeChannel(String),
    SetWelcomeChannelFinished(Result<(), String>),
    ToggleInvitesPaused,
    SetInvitesPausedFinished(Result<(), String>),
    // ---- Server stats (on-demand) ----
    LoadServerStats,
    ServerStatsUpdated(Option<crate::state::types::ServerStats>),
    MembersUpdated(Vec<ServerMemberRow>),
    KickMember(String),
    KickMemberFinished(Result<(), String>),
    StartRenameChannel(String, String),
    RenameChannelInputChanged(String),
    RenameChannel,
    RenameChannelFinished(Result<(), String>),
    CancelRenameChannel,
    DeleteChannel(String),
    DeleteChannelFinished(Result<(), String>),
    MoveChannelUp(String),
    MoveChannelDown(String),
    MoveChannelFinished(Result<(), String>),
    EditChannelPerms(String),
    CloseChannelPerms,
    ChannelOverwritesLoaded(Result<(String, Vec<(String, u32, u32)>), String>),
    SelectChannelPermRole(String),
    CycleChannelOverwritePerm(u32),
    ChannelOverwriteSaved(Result<(), String>),

    MessageInputChanged(String),
    /// @-autocomplete suggestion picked from the composer popup.
    MentionSuggestionPicked(String),
    PickAttachmentImage,
    AttachmentFilePicked(AttachmentPick),
    RemovePendingAttachment,
    /// Open a chat attachment at full size (lightbox).
    OpenAttachmentPreview(String),
    CloseAttachmentPreview,
    SendMessage,
    MessageSentFinished(Result<(), String>),
    EditMessage(String, String, bool),
    CancelEdit,
    EditFinished(Result<(), String>),
    DeleteMessage(String),
    DeleteFinished(Result<(), String>),
    PurgeMessage(String),
    PurgeFinished(Result<(), String>),
    CopyMessage(String),
    ToggleReaction(String, String),
    ReactionToggled(Result<(), String>),
    ReplyToMessage(String, String, String),
    CancelReply,

    /// Ask before wiping local vault + Convex + peer copy of this chat.
    ToggleClearChatConfirm,
    ConfirmClearChat,
    ClearChatFinished(Result<String, String>),

    ToggleStoreHistoryGlobal,
    StoreHistoryGlobalFinished(Result<bool, String>),
    ToggleStoreHistoryThisChat,
    StoreHistoryChatFinished(Result<bool, String>),
    ConversationStorePrefLoaded(bool, bool),
    ToggleHideOnline,
    ToggleFriendsOnlyDms,
    ToggleDiscoverable,
    PrivacyFlagFinished(Result<(), String>),
    SignOutOtherSessions,
    SignOutOthersFinished(Result<u32, String>),

    NewChannelIsVoice(bool),
    /// Flips `new_channel_is_voice` in place -- the Slint toggle button has
    /// no access to the current value to negate itself the way the iced
    /// view used to when constructing `NewChannelIsVoice(!self.x)`.
    ToggleNewChannelIsVoice,
    JoinVoiceChannel,
    LeaveVoiceChannel,
    VoiceUsersUpdated(Vec<VoiceUserRow>),
    /// Per-peer voice volume slider moved: (peer user_id, "*" for the 1:1
    /// call remote, gain 0.0..=5.0).
    VoiceVolumeChanged(String, f32),
    /// Ok(Some(channel_id)) = joined; Ok(None) = left.
    VoiceActionFinished(Result<Option<String>, String>),
    RoomVoiceEngineEvent(crate::media::room_voice::RoomVoiceEvent),
    /// Group/channel key ready (or failed) for the open conversation.
    GroupKeyReady(Result<(), String>),
    /// Unsealed key stored: (conversation_id, epoch, key bytes).
    GroupKeyLoaded(String, u32, [u8; 32]),

    ServerRolesUpdated(Vec<ServerRoleRow>),
    NewRoleNameChanged(String),
    CreateRole,
    CreateRoleFinished(Result<(), String>),
    ToggleMemberRole(String, String),
    ToggleRoleFinished(Result<(), String>),
    ToggleMemberRolePicker(String),
    MyServerPermsUpdated(u32),
    SelectRoleForEdit(String),
    CloseRoleEditor,
    RoleNameEditChanged(String),
    SaveRoleName,
    SetRoleColor(String, String),
    ToggleRolePermission(String, u32),
    RoleMutationFinished(Result<(), String>),
    ConfirmDeleteRole(String),
    CancelDeleteRole,
    DeleteRole(String),

    ToggleMembersPanel,
    AnimateMembersPanel,
    PanelResizeStarted(ResizePanel),
    PanelResizeMoved(f32),
    PanelResizeEnded,

    CreateBot,
    BotCreateFinished(Result<(String, String), String>), // name, token
    RefreshMyBots,
    MyBotsUpdated(Vec<BotSummary>),
    NewBotNameChanged(String),
    BotInviteUsernameChanged(String),
    InviteBotToServer,
    InviteBotFinished(Result<(), String>),
    RegenerateBotToken(String),
    BotTokenFinished(Result<String, String>),
    DeleteBot(String),
    DeleteBotFinished(Result<(), String>),
    DismissBotToken,

    ChatFilterChanged(String),
    FriendsFilterChanged(String),
    RetryConnect,
    ClearToast,

    AdminSetRole(String, bool),
    AdminSetPlatformRole(String, String),
    AdminSetRoleFinished(Result<(), String>),
    AdminSetBanned(String, bool),
    AdminSetBannedFinished(Result<(), String>),
    // ---- Admin panel: stats, filter, per-user detail, force-logout ----
    LoadAdminStats,
    AdminStatsUpdated(Result<crate::state::types::AdminStats, String>),
    /// 0 = All, 1 = Users, 2 = Staff, 3 = Banned (client-side filter).
    SetAdminFilter(i32),
    /// Toggle the expanded detail drawer for a user (empty/"same id" closes).
    ToggleAdminUserDetail(String),
    AdminUserDetailUpdated(Option<crate::state::types::AdminUserDetail>),
    AdminRevokeSessions(String),
    AdminRevokeSessionsFinished(Result<(), String>),

    Tick,
    HeartbeatFinished,

    MyCallUpdated(Option<MyCallInfo>),
    StartCall,
    AcceptCall,
    DeclineCall,
    HangUp,
    ToggleMute,
    ToggleMuteAll,
    /// Deafen = speaker/output mute only (mic is untouched); the sidebar
    /// user-panel headphones button drives this.
    ToggleDeafen,
    CallActionFinished(Result<(), String>),
    CallEngineEvent(call::CallEvent),

    ToggleSharePicker,
    ShareTargetsLoaded(Vec<screenshare::ShareTarget>),
    /// Encoded via `viewmodel::encode_share_target`/`decode_share_target`
    /// since Slint callbacks only ever hand back plain strings.
    StartShare(String),
    StopShare,
    ToggleShareViewSize,
    /// Mute remote share stream audio (viewer → peer signal).
    ToggleStreamMute,
    /// Include system audio (loopback) in outbound share.
    ToggleShareSystemAudio,
    /// Mute notifications for the active channel/conversation.
    ToggleChannelMute,
    ChannelMuteFinished(Result<bool, String>),
    OpenCommandPalette,
    CloseCommandPalette,
    CommandPaletteQueryChanged(String),
    CommandPaletteSearchFinished(Result<Vec<(String, String, String)>, String>),
    CommandPalettePick(usize),
    EscapePressed,

    OpenProfile(String),
    ProfileLoaded(Result<ProfileView, String>),
    CloseProfile,

    OpenSettings,
    CloseSettings,
    SettingsCategoryChanged(SettingsCategory),
    SettingsDisplayNameChanged(String),
    SettingsStatusChanged(String),
    SettingsBioChanged(String),
    SettingsAvatarColorSelected(String),
    SaveProfile,
    ProfileSaveFinished(Result<(), String>),
    SettingsCurrentPasswordChanged(String),
    SettingsNewPasswordChanged(String),
    SettingsConfirmPasswordChanged(String),
    ChangePassword,
    PasswordChangeFinished(Result<(), String>),
    SettingsInputDeviceSelected(String),
    SettingsOutputDeviceSelected(String),
    NoiseGateChanged(f32),

    /// HexaTalk Plus — Stripe checkout / portal / refresh status.
    PlusSubscribe,
    PlusManageBilling,
    PlusRefreshStatus,
    PlusCheckoutUrl(Result<String, String>),
    PlusStatusRefreshed(Result<(bool, i64), String>),

    AvatarImageLoaded(String, Result<Vec<u8>, String>),
    PickAvatarImage,
    AvatarFilePicked(AvatarPick),
    AvatarUploadFinished(Result<String, String>),
    RemoveAvatarImage,
    AvatarRemoveFinished(Result<(), String>),

    TypingUpdated(Vec<String>),
    TypingPingFinished,

    LogOut,
    LoggedOut,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", std::any::type_name::<Self>())
    }
}
