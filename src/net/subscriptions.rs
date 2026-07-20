//! Background jobs: one per Convex live query (friends, servers, channels,
//! members, conversations, calls, ...), plus the tray-icon bridge and a
//! couple of one-shot background `Task`s (`mark_read_task`,
//! `typing_ping_task`). Each Convex job follows the same shape: open a
//! `client.subscribe(...)` (via [`run_subscription`], which owns the
//! reconnect/backoff policy), loop over pushes, parse into a domain type,
//! forward as a `Message` into the update loop via `tx`.
//!
//! Ported from iced's `Subscription`-returning functions to plain
//! `crate::rt::Job`s (see src/rt.rs) driven by `App::subscription`'s
//! `SubscriptionRegistry::reconcile` call every update cycle -- same
//! dedup-by-id semantics `Subscription::run_with_id` had, just explicit.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::Duration;

use convex::{ConvexClient, FunctionResult, Value};
use futures::StreamExt;
use maplit::btreemap;
use tokio::sync::mpsc::UnboundedSender;

use crate::crypto;
use crate::media::call;
use crate::net::convex_parse::{
    expect_null, obj_array_ref, obj_bool, obj_f64, obj_ms, obj_object_ref, obj_opt_str, obj_str,
    obj_str_list, parse_object_array, value_as_bool,
};
use crate::net::peer;
use crate::net::rt::{AbortOnDrop, Job, Task, job};
use crate::state::message::Message;
use crate::state::types::{
    AdminUserRow, BlockedUser, CallRole, ChannelSummary, ChatMessage, ConversationSummary, Friend,
    FriendSuggestion, IncomingRequest, MemberRoleTag, MyCallInfo, OutgoingRequest, ServerMemberRow,
    ServerRoleRow, ServerSummary, Session, SocialStats, VoiceUserRow,
};
use crate::tray;

// ---------- Resilient subscription runner ----------

/// Runs one Convex live query *resiliently*: subscribe, forward each value
/// push to `on_result` (which returns `false` when the update loop is gone
/// and the job should stop), and when the stream ends or the subscribe
/// call itself fails (websocket drop, backend deploy, flaky network) wait
/// with exponential backoff and resubscribe instead of dying.
///
/// Before this helper, a failed `subscribe` returned immediately and
/// `SubscriptionRegistry::reconcile` respawned the job on the next update
/// cycle -- a tight reconnect hot-loop against an unreachable backend,
/// with no backoff at all.
///
/// Error pushes (`ErrorMessage`/`ConvexError`) are logged and skipped
/// rather than forwarded: every parser used to map them to an
/// empty/default snapshot, so a single transient server error briefly
/// blanked the friends/servers/messages lists, zeroed the caller's
/// permissions, or dropped the active-call card.
async fn run_subscription<F>(
    mut client: ConvexClient,
    name: &'static str,
    args: BTreeMap<String, Value>,
    mut on_result: F,
) where
    F: FnMut(FunctionResult) -> bool + Send,
{
    let mut failures: u32 = 0;
    loop {
        match client.subscribe(name, args.clone()).await {
            Ok(mut sub) => {
                failures = 0;
                while let Some(result) = sub.next().await {
                    match result {
                        FunctionResult::Value(_) => {
                            if !on_result(result) {
                                return;
                            }
                        }
                        FunctionResult::ErrorMessage(err) => {
                            eprintln!("[net] {name} pushed an error (state kept): {err}");
                        }
                        FunctionResult::ConvexError(err) => {
                            eprintln!("[net] {name} pushed an error (state kept): {err:?}");
                        }
                    }
                }
                // The stream ended (server closed it / websocket dropped).
                // Fall through to the backoff + resubscribe below; Convex
                // re-pushes the full current result on resubscribe, so the
                // UI state converges back without any gap handling.
            }
            Err(err) => {
                failures = failures.saturating_add(1);
                eprintln!("[net] subscribe {name} failed (failure #{failures}): {err}");
            }
        }
        backoff_sleep(failures).await;
    }
}

/// Exponential reconnect backoff: 1 s after a cleanly-ended stream, then
/// 2/4/8 s for consecutive subscribe failures, capped at 30 s. The job is
/// aborted (not blocked) if the registry cancels it -- `tokio::time::sleep`
/// is cancellation-safe.
async fn backoff_sleep(failures: u32) {
    let secs = match failures {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        _ => 30,
    };
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

// ---------- Subscriptions ----------

pub(crate) fn roles_subscription(
    client: ConvexClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("roles-subscription:{server_id}"), async move {
        run_subscription(
            client,
            "roles:listRoles",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "serverId".to_string() => Value::String(server_id),
            },
            move |result| {
                let roles = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ServerRoleRow {
                        role_id: obj_str(&obj, "roleId"),
                        name: obj_str(&obj, "name"),
                        color: obj_str(&obj, "color"),
                        position: obj_f64(&obj, "position") as i64,
                        permissions: obj_f64(&obj, "permissions") as u32,
                    })
                    .collect();
                tx.send(Message::ServerRolesUpdated(roles)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn my_perms_subscription(
    client: ConvexClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("my-perms-subscription:{server_id}"), async move {
        run_subscription(
            client,
            "roles:myPermissions",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "serverId".to_string() => Value::String(server_id),
            },
            move |result| {
                let perms = match result {
                    FunctionResult::Value(Value::Object(obj)) => {
                        obj_f64(&obj, "permissions") as u32
                    }
                    // Unexpected payload shape: keep the last known perms
                    // instead of flashing 0 (which hides channel UI).
                    _ => return true,
                };
                tx.send(Message::MyServerPermsUpdated(perms)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn room_voice_subscription(
    key: String,
    client: ConvexClient,
    token: String,
    user_id: String,
    conversation_id: String,
    input_device: Option<String>,
    output_device: Option<String>,
    muted: Arc<AtomicBool>,
    output_muted: Arc<AtomicBool>,
    noise_gate: Arc<AtomicU32>,
    gains: Arc<std::sync::Mutex<std::collections::HashMap<String, f32>>>,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("room-voice:{key}"), async move {
        let params = crate::media::room_voice::RoomVoiceParams {
            client,
            session_token: token,
            user_id,
            conversation_id,
            input_device,
            output_device,
            muted,
            output_muted,
            noise_gate,
            gains,
        };
        let (event_tx, mut event_rx) = futures::channel::mpsc::channel(16);
        // The guard aborts the engine task when this job ends or is aborted
        // by the registry -- previously the JoinHandle was dropped here, so
        // a respawned subscription left the old voice engine (mic capture!)
        // running orphaned.
        let _engine = AbortOnDrop(tokio::spawn(crate::media::room_voice::run_room_voice(
            params, event_tx,
        )));
        while let Some(event) = event_rx.next().await {
            if tx.send(Message::RoomVoiceEngineEvent(event)).is_err() {
                break;
            }
        }
    })
}

pub(crate) fn voice_users_subscription(
    client: ConvexClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("voice-users-subscription:{conversation_id}"),
        async move {
            run_subscription(
                client,
                "voice:listInChannel",
                btreemap! {
                    "sessionToken".to_string() => Value::String(token),
                    "conversationId".to_string() => Value::String(conversation_id),
                },
                move |result| {
                    let users = parse_object_array(result)
                        .into_iter()
                        .map(|obj| VoiceUserRow {
                            user_id: obj_str(&obj, "userId"),
                            display_name: obj_str(&obj, "displayName"),
                        })
                        .collect();
                    tx.send(Message::VoiceUsersUpdated(users)).is_ok()
                },
            )
            .await;
        },
    )
}

pub(crate) fn mark_read_task(
    client: &Option<ConvexClient>,
    session: &Option<Session>,
    conversation_id: String,
) -> Task<Message> {
    let (Some(client), Some(session)) = (client.clone(), session.clone()) else {
        return Task::none();
    };
    let mut client = client;
    Task::perform(
        async move {
            client
                .mutation(
                    "conversations:markRead",
                    btreemap! {
                        "sessionToken".to_string() => Value::String(session.token),
                        "conversationId".to_string() => Value::String(conversation_id),
                    },
                )
                .await
                .map_err(|err| err.to_string())
                .and_then(expect_null)
        },
        |_| Message::MarkReadFinished,
    )
}

pub(crate) fn typing_ping_task(
    client: &Option<ConvexClient>,
    session: &Option<Session>,
    conversation_id: String,
    typing: bool,
) -> Task<Message> {
    let (Some(client), Some(session)) = (client.clone(), session.clone()) else {
        return Task::none();
    };
    let mut client = client;
    Task::perform(
        async move {
            client
                .mutation(
                    "typing:setTyping",
                    btreemap! {
                        "sessionToken".to_string() => Value::String(session.token),
                        "conversationId".to_string() => Value::String(conversation_id),
                        "typing".to_string() => Value::Boolean(typing),
                    },
                )
                .await
                .map_err(|err| err.to_string())
                .and_then(expect_null)
        },
        |_| Message::TypingPingFinished,
    )
}

pub(crate) fn friends_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("friends-subscription", async move {
        run_subscription(
            client,
            "friends:listFriends",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let friends = parse_object_array(result)
                    .into_iter()
                    .map(|obj| Friend {
                        user_id: obj_str(&obj, "userId"),
                        username: obj_str(&obj, "username"),
                        display_name: obj_str(&obj, "displayName"),
                        last_seen_at: obj_ms(&obj, "lastSeenAt"),
                        presence: {
                            let p = obj_str(&obj, "presence");
                            if p.is_empty() { "offline".into() } else { p }
                        },
                        avatar_color: obj_str(&obj, "avatarColor"),
                        avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                        public_key: obj_str(&obj, "publicKey"),
                        status_message: obj_str(&obj, "statusMessage"),
                        nickname: obj_str(&obj, "nickname"),
                        favorite: obj.get("favorite").map(value_as_bool).unwrap_or(false),
                        private_note: obj_str(&obj, "privateNote"),
                        friends_since: obj_ms(&obj, "friendsSince"),
                        mutual_servers: obj_str_list(&obj, "mutualServers"),
                        is_staff: obj.get("isStaff").map(value_as_bool).unwrap_or(false),
                    })
                    .collect();
                tx.send(Message::FriendsUpdated(friends)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn servers_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("servers-subscription", async move {
        run_subscription(
            client,
            "servers:listMyServers",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let servers = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ServerSummary {
                        server_id: obj_str(&obj, "serverId"),
                        name: obj_str(&obj, "name"),
                        is_owner: obj_bool(&obj, "isOwner"),
                        invite_code: obj_str(&obj, "inviteCode"),
                        icon_url: obj_str(&obj, "iconUrl"),
                        custom_slug: obj_str(&obj, "customSlug"),
                        description: obj_str(&obj, "description"),
                        created_at: obj_ms(&obj, "createdAt"),
                        welcome_channel_id: obj_str(&obj, "welcomeChannelId"),
                        invites_paused: obj_bool(&obj, "invitesPaused"),
                    })
                    .collect();
                tx.send(Message::ServersUpdated(servers)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn channels_subscription(
    client: ConvexClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("channels-subscription:{server_id}"), async move {
        run_subscription(
            client,
            "servers:listChannels",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "serverId".to_string() => Value::String(server_id),
            },
            move |result| {
                let channels = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ChannelSummary {
                        conversation_id: obj_str(&obj, "conversationId"),
                        name: obj_str(&obj, "name"),
                        channel_type: {
                            let t = obj_str(&obj, "channelType");
                            if t.is_empty() { "text".into() } else { t }
                        },
                        // Absent pre-deploy -> 0 (badge hidden).
                        mention_count: obj_f64(&obj, "mentionCount") as u32,
                        category_id: obj_str(&obj, "categoryId"),
                        position: obj_f64(&obj, "position") as i64,
                        is_announcement: obj_bool(&obj, "isAnnouncement"),
                        is_system: obj_bool(&obj, "isSystem"),
                        muted: obj_bool(&obj, "muted"),
                        // Default true so older deployments still allow send.
                        can_send: obj
                            .get("canSend")
                            .map(|v| matches!(v, Value::Boolean(true)))
                            .unwrap_or(true),
                        permissions: obj_f64(&obj, "permissions") as u32,
                    })
                    .collect();
                tx.send(Message::ChannelsUpdated(channels)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn members_subscription(
    client: ConvexClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("members-subscription:{server_id}"), async move {
        run_subscription(
            client,
            "servers:listMembers",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "serverId".to_string() => Value::String(server_id),
            },
            move |result| {
                let members = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ServerMemberRow {
                        user_id: obj_str(&obj, "userId"),
                        display_name: obj_str(&obj, "displayName"),
                        username: obj_str(&obj, "username"),
                        avatar_color: obj_str(&obj, "avatarColor"),
                        avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                        is_owner: obj_bool(&obj, "isOwner"),
                        is_bot: obj_bool(&obj, "isBot"),
                        platform_role: {
                            let r = obj_str(&obj, "platformRole");
                            if r.is_empty() { "user".into() } else { r }
                        },
                        plus_active: obj_bool(&obj, "plusActive"),
                        last_seen_at: obj_ms(&obj, "lastSeenAt"),
                        // Borrowing accessor: no per-role BTreeMap clone.
                        roles: obj_array_ref(&obj, "roles")
                            .iter()
                            .filter_map(|r| match r {
                                Value::Object(r) => Some(MemberRoleTag {
                                    role_id: obj_str(r, "roleId"),
                                    name: obj_str(r, "name"),
                                    color: obj_str(r, "color"),
                                }),
                                _ => None,
                            })
                            .collect(),
                    })
                    .collect();
                tx.send(Message::MembersUpdated(members)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn tray_subscription(tx: UnboundedSender<Message>) -> Job {
    job("tray-subscription", async move {
        let (tray_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<tray::TrayEvent>();
        tray::spawn(tray_tx);
        while let Some(event) = rx.recv().await {
            if tx.send(Message::TrayEvent(event)).is_err() {
                break;
            }
        }
    })
}

pub(crate) fn requests_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("requests-subscription", async move {
        run_subscription(
            client,
            "friends:listIncomingRequests",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let requests = parse_object_array(result)
                    .into_iter()
                    .map(|obj| IncomingRequest {
                        request_id: obj_str(&obj, "requestId"),
                        from_user_id: obj_str(&obj, "fromUserId"),
                        from_username: obj_str(&obj, "fromUsername"),
                        from_display_name: obj_str(&obj, "fromDisplayName"),
                        from_avatar_color: obj_str(&obj, "fromAvatarColor"),
                        from_avatar_image_url: obj_str(&obj, "fromAvatarImageUrl"),
                        note: obj_str(&obj, "note"),
                        sent_at: obj_ms(&obj, "sentAt"),
                        from_status_message: obj_str(&obj, "fromStatusMessage"),
                        mutual_servers: obj_str_list(&obj, "mutualServers"),
                        presence: {
                            let p = obj_str(&obj, "presence");
                            if p.is_empty() { "offline".into() } else { p }
                        },
                        is_staff: obj.get("isStaff").map(value_as_bool).unwrap_or(false),
                    })
                    .collect();
                tx.send(Message::RequestsUpdated(requests)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn outgoing_requests_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("outgoing-requests-subscription", async move {
        run_subscription(
            client,
            "friends:listOutgoingRequests",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let requests = parse_object_array(result)
                    .into_iter()
                    .map(|obj| OutgoingRequest {
                        request_id: obj_str(&obj, "requestId"),
                        to_user_id: obj_str(&obj, "toUserId"),
                        to_username: obj_str(&obj, "toUsername"),
                        to_display_name: obj_str(&obj, "toDisplayName"),
                        to_avatar_color: obj_str(&obj, "toAvatarColor"),
                        to_avatar_image_url: obj_str(&obj, "toAvatarImageUrl"),
                        note: obj_str(&obj, "note"),
                        sent_at: obj_ms(&obj, "sentAt"),
                    })
                    .collect();
                tx.send(Message::OutgoingRequestsUpdated(requests)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn social_stats_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("social-stats-subscription", async move {
        run_subscription(
            client,
            "friends:socialStats",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                if let FunctionResult::Value(Value::Object(obj)) = result {
                    let stats = SocialStats {
                        friends_total: obj_f64(&obj, "friendsTotal") as u32,
                        friends_online: obj_f64(&obj, "friendsOnline") as u32,
                        incoming_pending: obj_f64(&obj, "incomingPending") as u32,
                        outgoing_pending: obj_f64(&obj, "outgoingPending") as u32,
                    };
                    return tx.send(Message::SocialStatsUpdated(stats)).is_ok();
                }
                true
            },
        )
        .await;
    })
}

pub(crate) fn suggestions_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("suggestions-subscription", async move {
        run_subscription(
            client,
            "friends:suggestPeople",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let list = parse_object_array(result)
                    .into_iter()
                    .map(|obj| FriendSuggestion {
                        user_id: obj_str(&obj, "userId"),
                        username: obj_str(&obj, "username"),
                        display_name: obj_str(&obj, "displayName"),
                        avatar_color: obj_str(&obj, "avatarColor"),
                        avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                        status_message: obj_str(&obj, "statusMessage"),
                        presence: {
                            let p = obj_str(&obj, "presence");
                            if p.is_empty() { "offline".into() } else { p }
                        },
                        mutual_servers: obj_str_list(&obj, "mutualServers"),
                    })
                    .collect();
                tx.send(Message::SuggestionsUpdated(list)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn blocked_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("blocked-subscription", async move {
        run_subscription(
            client,
            "friends:listBlocked",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let blocked = parse_object_array(result)
                    .into_iter()
                    .map(|obj| BlockedUser {
                        user_id: obj_str(&obj, "userId"),
                        display_name: obj_str(&obj, "displayName"),
                    })
                    .collect();
                tx.send(Message::BlockedUpdated(blocked)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn conversations_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("conversations-subscription", async move {
        run_subscription(
            client,
            "conversations:listMyConversations",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let conversations = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ConversationSummary {
                        conversation_id: obj_str(&obj, "conversationId"),
                        title: obj_str(&obj, "title"),
                        kind: obj_str(&obj, "kind"),
                        peer_user_id: obj_opt_str(&obj, "peerUserId"),
                        last_message_at: obj_ms(&obj, "lastMessageAt"),
                        unread: obj_bool(&obj, "unread"),
                        // Absent pre-deploy -> 0 (badge hidden).
                        mention_count: obj_f64(&obj, "mentionCount") as u32,
                    })
                    .collect();
                tx.send(Message::ConversationsUpdated(conversations)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn admin_users_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("admin-users-subscription", async move {
        run_subscription(
            client,
            "admin:listUsers",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                let users = parse_object_array(result)
                    .into_iter()
                    .map(|obj| AdminUserRow {
                        user_id: obj_str(&obj, "userId"),
                        username: obj_str(&obj, "username"),
                        display_name: obj_str(&obj, "displayName"),
                        role: obj_str(&obj, "role"),
                        banned: obj_bool(&obj, "banned"),
                    })
                    .collect();
                tx.send(Message::AdminUsersUpdated(users)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn my_call_subscription(
    client: ConvexClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("my-call-subscription", async move {
        run_subscription(
            client,
            "calls:myCall",
            btreemap! { "sessionToken".to_string() => Value::String(token) },
            move |result| {
                // Error pushes are filtered by run_subscription, so a
                // non-object value here really means "no active call"
                // (Value::Null) -- previously a transient error also mapped
                // to None and made the active call card vanish.
                let info = match result {
                    FunctionResult::Value(Value::Object(obj)) => Some(MyCallInfo {
                        call_id: obj_str(&obj, "callId"),
                        is_caller: obj_bool(&obj, "isCaller"),
                        status: obj_str(&obj, "status"),
                        peer_display_name: obj_str(&obj, "peerDisplayName"),
                        offer_sdp: obj_str(&obj, "offerSdp"),
                    }),
                    _ => None,
                };
                tx.send(Message::MyCallUpdated(info)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn call_subscription(
    key: String,
    client: ConvexClient,
    token: String,
    role: CallRole,
    input_device: Option<String>,
    output_device: Option<String>,
    muted: Arc<AtomicBool>,
    output_muted: Arc<AtomicBool>,
    noise_gate: Arc<AtomicU32>,
    share_control_slot: Arc<
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<call::ShareCommand>>>,
    >,
    gains: Arc<std::sync::Mutex<std::collections::HashMap<String, f32>>>,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("call-subscription:{key}"), async move {
        let (is_caller, call_id, conversation_id, callee_id, offer_sdp) = match role {
            CallRole::Caller {
                conversation_id,
                callee_id,
            } => (true, None, Some(conversation_id), Some(callee_id), None),
            CallRole::Callee { call_id, offer_sdp } => {
                (false, Some(call_id), None, None, Some(offer_sdp))
            }
        };

        // Runs once per call (dedup'd by id in `SubscriptionRegistry`), so
        // this is the one place that ever takes the receiver out of the slot.
        let share_rx = share_control_slot
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .unwrap_or_else(|| tokio::sync::mpsc::unbounded_channel().1);

        let params = call::CallParams {
            client,
            session_token: token,
            is_caller,
            call_id,
            conversation_id,
            callee_id,
            offer_sdp,
            input_device,
            output_device,
            muted,
            output_muted,
            noise_gate,
            gains,
            share_rx,
        };

        let (event_tx, mut event_rx) = futures::channel::mpsc::channel(16);
        // Aborts the call engine if this job ends or is aborted (see the
        // room_voice twin above for why the bare spawn handle was a leak).
        let _engine = AbortOnDrop(tokio::spawn(call::run_call(params, event_tx)));

        while let Some(event) = event_rx.next().await {
            if tx.send(Message::CallEngineEvent(event)).is_err() {
                break;
            }
        }
    })
}

pub(crate) const DECRYPT_FAILED_PLACEHOLDER: &str = "Unable to decrypt";

pub(crate) fn apply_decrypted_payload(msg: &mut ChatMessage, raw: &str) {
    if let Some(payload) = crypto::MessagePayload::decode(raw) {
        msg.body = payload.text;
        msg.attachment_key = payload.att_key;
        msg.attachment_nonce = payload.att_nonce;
    } else {
        // Older cache entries or plain text envelope.
        msg.body = raw.to_string();
        msg.attachment_key = None;
        msg.attachment_nonce = None;
    }
}

pub(crate) fn messages_subscription(
    client: ConvexClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("messages-subscription:{conversation_id}"),
        async move {
            run_subscription(
                client,
                "messages:list",
                btreemap! {
                    "sessionToken".to_string() => Value::String(token),
                    "conversationId".to_string() => Value::String(conversation_id),
                },
                move |result| {
                    // Ciphertext is left intact here; `decrypt_incoming_messages`
                    // runs on the update loop where the ratchet session lives.
                    let messages = parse_object_array(result)
                        .into_iter()
                        .map(|obj| {
                            let is_encrypted = obj_bool(&obj, "encrypted");
                            let raw_body = obj_str(&obj, "body");

                            // Encrypted reply snippets stay as raw ciphertext;
                            // `decrypt_incoming_messages` resolves them via the
                            // decrypt cache / ratchet on the update loop.
                            // Borrowing accessor: no BTreeMap clone.
                            let reply_to = obj_object_ref(&obj, "replyTo")
                                .map(|r| (obj_str(r, "authorName"), obj_str(r, "snippet")));

                            ChatMessage {
                                id: obj_str(&obj, "id"),
                                author_id: obj_str(&obj, "authorId"),
                                author_name: obj_str(&obj, "authorName"),
                                author_avatar_color: obj_str(&obj, "authorAvatarColor"),
                                author_avatar_url: obj_str(&obj, "authorAvatarImageUrl"),
                                author_is_bot: obj_bool(&obj, "authorIsBot"),
                                author_plus_active: obj_bool(&obj, "authorPlusActive"),
                                body: raw_body,
                                kind: obj_str(&obj, "kind"),
                                attachment_url: obj_str(&obj, "attachmentUrl"),
                                attachment_key: None,
                                attachment_nonce: None,
                                reactions: obj_array_ref(&obj, "reactions")
                                    .iter()
                                    .filter_map(|r| match r {
                                        Value::Object(r) => Some((
                                            obj_str(r, "emoji"),
                                            obj_f64(r, "count") as u32,
                                            obj_bool(r, "reactedByMe"),
                                        )),
                                        _ => None,
                                    })
                                    .collect(),
                                reply_to,
                                encrypted: is_encrypted,
                                sent_at: obj_ms(&obj, "sentAt"),
                                deleted: obj_bool(&obj, "deleted"),
                                edited: obj_bool(&obj, "edited"),
                                pinned: obj_bool(&obj, "pinned"),
                            }
                        })
                        .collect();
                    tx.send(Message::MessagesUpdated(messages)).is_ok()
                },
            )
            .await;
        },
    )
}

/// Watches `messages:listPinned` for the open conversation; rows arrive as
/// `ChatMessage`s (body = snippet; encrypted blobs stay ciphertext for the
/// update loop to decrypt, same convention as `messages_subscription`).
pub(crate) fn pins_subscription(
    client: ConvexClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("pins-subscription:{conversation_id}"), async move {
        run_subscription(
            client,
            "messages:listPinned",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "conversationId".to_string() => Value::String(conversation_id),
            },
            move |result| {
                let pinned = parse_object_array(result)
                    .into_iter()
                    .map(|obj| ChatMessage {
                        id: obj_str(&obj, "id"),
                        author_id: obj_str(&obj, "authorId"),
                        author_name: obj_str(&obj, "authorName"),
                        author_avatar_color: String::new(),
                        author_avatar_url: String::new(),
                        author_is_bot: false,
                        author_plus_active: false,
                        body: obj_str(&obj, "snippet"),
                        kind: "text".into(),
                        attachment_url: String::new(),
                        attachment_key: None,
                        attachment_nonce: None,
                        reactions: Vec::new(),
                        reply_to: None,
                        encrypted: obj_bool(&obj, "encrypted"),
                        sent_at: obj_ms(&obj, "sentAt"),
                        deleted: false,
                        edited: false,
                        pinned: true,
                    })
                    .collect();
                tx.send(Message::PinnedMessagesUpdated(pinned)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn typing_subscription(
    client: ConvexClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("typing-subscription:{conversation_id}"),
        async move {
            run_subscription(
                client,
                "typing:whoIsTyping",
                btreemap! {
                    "sessionToken".to_string() => Value::String(token),
                    "conversationId".to_string() => Value::String(conversation_id),
                },
                move |result| {
                    let names = match result {
                        FunctionResult::Value(Value::Array(items)) => items
                            .into_iter()
                            .filter_map(|item| match item {
                                Value::String(s) => Some(s),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    tx.send(Message::TypingUpdated(names)).is_ok()
                },
            )
            .await;
        },
    )
}

pub(crate) fn peer_session_subscription(
    local_user_id: String,
    peer_user_id: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("peerseal-session:{peer_user_id}"), async move {
        let (event_tx, mut event_rx) = futures::channel::mpsc::channel(32);
        let cmd_tx = peer::spawn_dm_session(
            local_user_id,
            peer_user_id.clone(),
            conversation_id,
            event_tx,
        );
        if tx
            .send(Message::PeerCmdReady(peer_user_id.clone(), cmd_tx))
            .is_err()
        {
            return;
        }
        // When this job is aborted by the registry, `event_rx` is dropped;
        // the peer worker notices the closed event channel and shuts itself
        // down (see peer.rs), so no orphaned session task survives.
        while let Some(ev) = event_rx.next().await {
            if tx
                .send(Message::PeerEvent(peer_user_id.clone(), ev))
                .is_err()
            {
                break;
            }
        }
    })
}

pub(crate) fn peer_invite_subscription(
    client: ConvexClient,
    token: String,
    peer_user_id: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("peerseal-invite:{peer_user_id}"), async move {
        // Last forwarded payload: dedups Convex re-pushes of the same
        // invite, while still delivering a *changed* payload (host
        // republish after a relay drop).
        let mut last_payload = String::new();
        run_subscription(
            client,
            "peer:getInvite",
            btreemap! {
                "sessionToken".to_string() => Value::String(token),
                "conversationId".to_string() => Value::String(conversation_id),
            },
            move |result| {
                let payload = match result {
                    FunctionResult::Value(Value::Object(obj)) => {
                        // Only the non-host should consume the payload.
                        if obj_bool(&obj, "isHost") {
                            None
                        } else {
                            let p = obj_str(&obj, "invitePayload");
                            if p.is_empty() { None } else { Some(p) }
                        }
                    }
                    _ => None,
                };
                match payload {
                    Some(p) if p != last_payload => {
                        last_payload = p.clone();
                        tx.send(Message::PeerInviteUpdated(peer_user_id.clone(), Some(p)))
                            .is_ok()
                    }
                    // No invite / unchanged payload: nothing to forward.
                    // `last_payload` is intentionally kept across clears so
                    // a later republish of a different string is delivered.
                    _ => true,
                }
            },
        )
        .await;
    })
}
