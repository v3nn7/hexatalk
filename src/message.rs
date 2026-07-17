//! The `Message` enum: every event the update loop can react to (button
//! clicks, text input changes, async task results, Convex subscription
//! pushes, call/tray/peer events, ...). One flat enum by design -- iced's
//! Elm architecture routes everything through a single `update(Message)`,
//! so splitting this further would just scatter match arms without any
//! real decoupling benefit.

use convex::ConvexClient;

use crate::*;

// ---------- Messages ----------

#[derive(Clone)]
pub(crate) enum Message {
    Connected(ConvexClient),
    ConnectFailed(String),

    SwitchAuthMode(AuthMode),
    UsernameInputChanged(String),
    PasswordInputChanged(String),
    DisplayNameInputChanged(String),
    SubmitAuth,
    AuthFinished(Result<Session, String>),
    RestoreFinished(Result<Session, String>),
    PublicKeyUploaded,

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
    MeasurePing,
    PingMeasured(Option<u64>),
    WindowCloseRequested(iced::window::Id),
    TrayEvent(tray::TrayEvent),
    WindowFocusChanged(iced::window::Event),

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

    OpenConversationWithFriend(Friend),
    OpenConversationDirect(ConversationSummary),
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
    SelectServer(ServerSummary),
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
    CopyInviteCode(String),
    /// Same as `CopyInviteCode`, but copies the shareable `talkyss://invite/<code>`
    /// link instead of the bare code.
    CopyInviteLink(String),
    ChannelsUpdated(Vec<ChannelSummary>),
    OpenChannel(ChannelSummary),
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

    MessageInputChanged(String),
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
    JoinVoiceChannel,
    LeaveVoiceChannel,
    VoiceUsersUpdated(Vec<VoiceUserRow>),
    /// Ok(Some(channel_id)) = joined; Ok(None) = left.
    VoiceActionFinished(Result<Option<String>, String>),
    RoomVoiceEngineEvent(crate::room_voice::RoomVoiceEvent),
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

    Tick,
    HeartbeatFinished,

    MyCallUpdated(Option<MyCallInfo>),
    StartCall,
    AcceptCall,
    DeclineCall,
    HangUp,
    ToggleMute,
    ToggleMuteAll,
    CallActionFinished(Result<(), String>),
    CallEngineEvent(call::CallEvent),

    ToggleSharePicker,
    ShareTargetsLoaded(Vec<screenshare::ShareTarget>),
    StartShare(screenshare::ShareTarget),
    StopShare,
    ToggleShareViewSize,
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

    AvatarImageLoaded(String, Result<Vec<u8>, String>),
    PickAvatarImage,
    AvatarFilePicked(AvatarPick),
    AvatarUploadFinished(Result<String, String>),
    RemoveAvatarImage,
    AvatarRemoveFinished(Result<(), String>),

    TypingUpdated(Vec<String>),
    TypingPingFinished,

    RingTick,

    LogOut,
    LoggedOut,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", std::any::type_name::<Self>())
    }
}
