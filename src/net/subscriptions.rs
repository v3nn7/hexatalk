//! Background jobs: one per server-backed live view (friends, servers,
//! channels, members, conversations, calls, ...), plus the tray-icon bridge
//! and a couple of one-shot background `Task`s (`mark_read_task`,
//! `typing_ping_task`).
//!
//! Post-migration model (see MIGRATION_API.md, section "Subskrypcje"): each
//! job (1) ensures the shared WebSocket is up via `client.ensure_ws()`,
//! (2) does an initial fetch through `client.query(<same Convex path>)` --
//! the dispatch layer translates that path into the matching REST GET -- and
//! (3) loops on a `tokio::select!` over the shared
//! `broadcast::Receiver<WsEvent>` (filtered per subscription: conversation
//! events match on `event.channel == conversationId`, personal server-scope
//! events fail open) plus a 30 s tick as a reconnect/missed-event fallback.
//! A matching event triggers a refetch -> parse -> emit cycle, so the UI
//! converges exactly like it did under Convex live queries.
//!
//! `typing:whoIsTyping` and `calls:myCall` have no REST GET: `typing` keeps a
//! local map fed by `typing` events with a 5 s expiry, and `my_call`
//! reconstructs the active call from `call.*` events.
//!
//! Ported from iced's `Subscription`-returning functions to plain
//! `crate::rt::Job`s (see src/rt.rs) driven by `App::subscription`'s
//! `SubscriptionRegistry::reconcile` call every update cycle -- same
//! dedup-by-id semantics `Subscription::run_with_id` had, just explicit.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::{Duration, Instant};

use futures::StreamExt;
use maplit::btreemap;
use tokio::sync::mpsc::UnboundedSender;

use crate::crypto;
use crate::media::call;
use crate::net::api::{ApiClient, FunctionResult, Value, WsEvent};
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

// ---------- Polling/event subscription runner ----------

/// Fallback refetch cadence for every subscription (WS events drive the
/// fast path; the tick covers reconnects and anything the server missed).
const DEFAULT_TICK: Duration = Duration::from_secs(30);
/// Peer invites have no WS event at all, so polling is the only channel --
/// keep it tighter than the default to not stall Peerseal session setup.
const PEER_INVITE_TICK: Duration = Duration::from_secs(10);
/// Typing indicators expire server-side after ~5 s; mirror that locally.
const TYPING_TTL: Duration = Duration::from_secs(5);
/// How often the typing map is swept for expired entries.
const TYPING_SWEEP: Duration = Duration::from_secs(1);
/// Trailing debounce for WS-event-triggered refetches: a burst of matching
/// events (message.new + reaction.add + member.* in the same second) sets a
/// dirty flag and the refetch only runs once the burst goes quiet for this
/// long. Without it every event fired a full REST refetch per subscription.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(400);
/// Upper bound on how long a dirty flag may sit unrefreshed while events
/// keep arriving (a busy channel would otherwise starve the refetch).
const EVENT_MAX_WAIT: Duration = Duration::from_secs(1);

/// Pending debounce state: `(first_event_at, last_event_at)` while a matching
/// WS event has been seen but the refetch has not run yet.
type PendingRefresh = Option<(Instant, Instant)>;

/// Resolves once the debounce window for `pending` closes: 400 ms after the
/// last event, capped at 1 s from the first. Pends forever when clean, so
/// the `select!` branch simply never wins until an event marks us dirty.
async fn debounce_due(pending: PendingRefresh) {
    match pending {
        Some((first, last)) => {
            let due = (last + EVENT_DEBOUNCE).min(first + EVENT_MAX_WAIT);
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Forwards one fetched `FunctionResult` into `on_result` (which returns
/// `false` when the update loop is gone and the job should stop). Error
/// results are logged and skipped rather than forwarded: every parser maps
/// them to an empty/default snapshot, so a single transient server error
/// would briefly blank the friends/servers/messages lists, zero the caller's
/// permissions, or drop the active-call card.
fn deliver<F>(name: &'static str, result: FunctionResult, on_result: &mut F) -> bool
where
    F: FnMut(FunctionResult) -> bool + Send,
{
    match result {
        FunctionResult::Value(_) => on_result(result),
        FunctionResult::ErrorMessage(err) => {
            eprintln!("[net] {name} returned an error (state kept): {err}");
            true
        }
        FunctionResult::ConvexError(err) => {
            eprintln!("[net] {name} returned an error (state kept): {err:?}");
            true
        }
    }
}

/// Runs one REST-backed "live query": initial fetch + refetch on matching
/// WS events (coalesced through the trailing `EVENT_DEBOUNCE` window, capped
/// by `EVENT_MAX_WAIT`) + refetch every `tick`. Fetch failures are logged
/// and skipped (state kept) -- the next event or tick retries. A lagged
/// broadcast receiver forces an immediate refetch; a closed one (WS task
/// restarted on reconnect) is re-attached via `client.subscribe_events()`.
async fn run_query_subscription<F, M>(
    client: ApiClient,
    name: &'static str,
    args: BTreeMap<String, Value>,
    tick: Duration,
    matches: M,
    mut on_result: F,
) where
    F: FnMut(FunctionResult) -> bool + Send,
    M: Fn(&WsEvent) -> bool + Send,
{
    client.ensure_ws();
    let mut rx = client.subscribe_events();
    // The first interval tick completes immediately, so the loop below also
    // performs the initial fetch.
    let mut ticker = tokio::time::interval(tick);
    // Dirty flag for the trailing debounce: matching events only arm it,
    // the refetch fires once the burst goes quiet.
    let mut pending: PendingRefresh = None;
    loop {
        let refresh = tokio::select! {
            _ = ticker.tick() => {
                pending = None;
                true
            }
            event = rx.recv() => match event {
                Ok(event) => {
                    if matches(&event) {
                        let now = Instant::now();
                        match pending {
                            Some((_, ref mut last)) => *last = now,
                            None => pending = Some((now, now)),
                        }
                    }
                    false
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Events were lost, so coalescing is unsafe -- refetch
                    // immediately instead of waiting out the quiet window.
                    eprintln!("[net] {name}: dropped {n} WS events; refreshing");
                    pending = None;
                    true
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // The WS task was restarted (reconnect); re-attach to the
                    // new broadcast channel and force a refresh.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    client.ensure_ws();
                    rx = client.subscribe_events();
                    pending = None;
                    true
                }
            },
            _ = debounce_due(pending) => {
                pending = None;
                true
            }
        };
        if !refresh {
            continue;
        }
        match client.query(name, args.clone()).await {
            Ok(result) => {
                if !deliver(name, result, &mut on_result) {
                    return;
                }
            }
            Err(err) => eprintln!("[net] {name} fetch failed (state kept): {err}"),
        }
    }
}

/// Server-scope personal events (`member.*`, `channel.*`, `server.updated`)
/// carry the *user* id in `channel`; the server id lives in the payload.
/// When the payload names a server we filter on it, otherwise we fail open
/// (a refetch is cheap and the UI just repaints the same data).
fn payload_matches_server(event: &WsEvent, server_id: &str) -> bool {
    match event
        .payload
        .get("server_id")
        .or_else(|| event.payload.get("serverId"))
        .and_then(|v| v.as_str())
    {
        Some(id) => id == server_id,
        None => true,
    }
}

fn is_server_scope_event(kind: &str) -> bool {
    kind.starts_with("channel.") || kind.starts_with("member.") || kind == "server.updated"
}

/// First string value found under any of `keys` (snake_case first, camelCase
/// fallback) -- WS payload shapes are server-owned, so read defensively.
fn json_str(payload: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| payload.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

/// Extracts (user id, display name) from a `typing` event payload, accepting
/// both a flat `{user_id, display_name}` and a nested `{user: {...}}` shape.
fn typing_event_user(payload: &serde_json::Value) -> Option<(String, String)> {
    let user = payload.get("user").unwrap_or(payload);
    let id = ["user_id", "userId", "id"]
        .iter()
        .find_map(|k| user.get(*k).and_then(|v| v.as_str()))?;
    let name = ["display_name", "displayName", "username", "name"]
        .iter()
        .find_map(|k| user.get(*k).and_then(|v| v.as_str()))
        .unwrap_or(id);
    Some((id.to_string(), name.to_string()))
}

/// Sorted, deduped display names of everyone currently typing.
fn typing_names(typers: &HashMap<String, (String, Instant)>) -> Vec<String> {
    let mut names: Vec<String> = typers.values().map(|(name, _)| name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

// ---------- Subscriptions ----------

pub(crate) fn roles_subscription(
    client: ApiClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("roles-subscription:{server_id}"), async move {
        let _ = token; // auth rides in the Bearer header now
        let watch = server_id.clone();
        run_query_subscription(
            client,
            "roles:listRoles",
            btreemap! {
                "serverId".to_string() => Value::String(server_id),
            },
            DEFAULT_TICK,
            move |event: &WsEvent| {
                is_server_scope_event(&event.kind) && payload_matches_server(event, &watch)
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
    client: ApiClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("my-perms-subscription:{server_id}"), async move {
        let _ = token;
        let watch = server_id.clone();
        run_query_subscription(
            client,
            "roles:myPermissions",
            btreemap! {
                "serverId".to_string() => Value::String(server_id),
            },
            DEFAULT_TICK,
            move |event: &WsEvent| {
                is_server_scope_event(&event.kind) && payload_matches_server(event, &watch)
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
    client: ApiClient,
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
    client: ApiClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("voice-users-subscription:{conversation_id}"),
        async move {
            let _ = token;
            let conv = conversation_id.clone();
            run_query_subscription(
                client,
                "voice:listInChannel",
                btreemap! {
                    "conversationId".to_string() => Value::String(conversation_id),
                },
                DEFAULT_TICK,
                move |event: &WsEvent| {
                    (event.kind == "voice.join" || event.kind == "voice.leave")
                        && event.channel == conv
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
    client: &Option<ApiClient>,
    session: &Option<Session>,
    conversation_id: String,
) -> Task<Message> {
    let _ = session; // token travels in the Bearer header via `client`
    let Some(client) = client.clone() else {
        return Task::none();
    };
    Task::perform(
        async move {
            client
                .mutation(
                    "conversations:markRead",
                    btreemap! {
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
    client: &Option<ApiClient>,
    session: &Option<Session>,
    conversation_id: String,
    typing: bool,
) -> Task<Message> {
    let _ = session;
    let Some(client) = client.clone() else {
        return Task::none();
    };
    Task::perform(
        async move {
            client
                .mutation(
                    "typing:setTyping",
                    btreemap! {
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("friends-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:listFriends",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("servers-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "servers:listMyServers",
            BTreeMap::new(),
            DEFAULT_TICK,
            |event: &WsEvent| {
                event.kind == "server.updated"
                    || matches!(event.kind.as_str(), "member.join" | "member.leave" | "member.kick")
            },
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
    client: ApiClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("channels-subscription:{server_id}"), async move {
        let _ = token;
        let watch = server_id.clone();
        run_query_subscription(
            client,
            "servers:listChannels",
            btreemap! {
                "serverId".to_string() => Value::String(server_id),
            },
            DEFAULT_TICK,
            move |event: &WsEvent| {
                is_server_scope_event(&event.kind) && payload_matches_server(event, &watch)
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
    client: ApiClient,
    token: String,
    server_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("members-subscription:{server_id}"), async move {
        let _ = token;
        let watch = server_id.clone();
        run_query_subscription(
            client,
            "servers:listMembers",
            btreemap! {
                "serverId".to_string() => Value::String(server_id),
            },
            DEFAULT_TICK,
            move |event: &WsEvent| {
                // Server-scope changes + presence (arrives on the personal
                // channel = my userId, so it's not server-scoped).
                (is_server_scope_event(&event.kind) && payload_matches_server(event, &watch))
                    || event.kind == "presence"
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("requests-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:listIncomingRequests",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("outgoing-requests-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:listOutgoingRequests",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("social-stats-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:socialStats",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("suggestions-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:suggestPeople",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("blocked-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "friends:listBlocked",
            BTreeMap::new(),
            DEFAULT_TICK,
            // friend_request.* dla zmian relacji + presence dla aktywności
            // (oba eventy przychodzą na kanale = moje userId).
            |event: &WsEvent| {
                event.kind.starts_with("friend_request.") || event.kind == "presence"
            },
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
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("conversations-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "conversations:listMyConversations",
            BTreeMap::new(),
            DEFAULT_TICK,
            |event: &WsEvent| {
                event.kind == "message.new"
                    || event.kind.starts_with("member.")
                    || event.kind.starts_with("channel.")
            },
            move |result| {
                let conversations = parse_object_array(result)
                    .into_iter()
                    .map(|obj| {
                        let title = obj_str(&obj, "title");
                        let is_support = obj_bool(&obj, "isSupport")
                            || title.eq_ignore_ascii_case("Support")
                            || title.eq_ignore_ascii_case("HexaTalk Support");
                        ConversationSummary {
                            conversation_id: obj_str(&obj, "conversationId"),
                            title,
                            kind: obj_str(&obj, "kind"),
                            peer_user_id: obj_opt_str(&obj, "peerUserId"),
                            last_message_at: obj_ms(&obj, "lastMessageAt"),
                            unread: obj_bool(&obj, "unread"),
                            // Absent pre-deploy -> 0 (badge hidden).
                            mention_count: obj_f64(&obj, "mentionCount") as u32,
                            is_support,
                        }
                    })
                    .collect();
                tx.send(Message::ConversationsUpdated(conversations)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn admin_users_subscription(
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("admin-users-subscription", async move {
        let _ = token;
        run_query_subscription(
            client,
            "admin:listUsers",
            BTreeMap::new(),
            DEFAULT_TICK,
            |_: &WsEvent| false, // no WS events for the admin panel
            move |result| {
                let users = parse_object_array(result)
                    .into_iter()
                    .map(|obj| AdminUserRow {
                        user_id: obj_str(&obj, "userId"),
                        username: obj_str(&obj, "username"),
                        display_name: obj_str(&obj, "displayName"),
                        role: obj_str(&obj, "role"),
                        banned: obj_bool(&obj, "banned"),
                        ban_expires_at: {
                            let a = obj_ms(&obj, "banExpiresAt");
                            if a > 0 {
                                a
                            } else {
                                obj_ms(&obj, "bannedUntil")
                            }
                        },
                        muted: obj_bool(&obj, "muted"),
                        mute_expires_at: {
                            let a = obj_ms(&obj, "muteExpiresAt");
                            if a > 0 {
                                a
                            } else {
                                obj_ms(&obj, "mutedUntil")
                            }
                        },
                        plus_active: obj_bool(&obj, "plusActive"),
                        plus_expires_at: obj_ms(&obj, "plusExpiresAt"),
                    })
                    .collect();
                tx.send(Message::AdminUsersUpdated(users)).is_ok()
            },
        )
        .await;
    })
}

pub(crate) fn my_call_subscription(
    client: ApiClient,
    token: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job("my-call-subscription", async move {
        let _ = token;
        client.ensure_ws();
        let mut rx = client.subscribe_events();
        let mut ticker = tokio::time::interval(DEFAULT_TICK);
        // `calls:myCall` has no REST GET (degradation table: dispatch
        // returns null), so the active call is reconstructed locally from
        // `call.*` events. The tick is only a best-effort reconciliation for
        // the day the endpoint exists -- a null result never wipes the
        // event-built state.
        let mut current: Option<MyCallInfo> = None;
        loop {
            let dirty = tokio::select! {
                _ = ticker.tick() => {
                    match client.query("calls:myCall", BTreeMap::new()).await {
                        Ok(FunctionResult::Value(Value::Object(obj))) => {
                            current = Some(MyCallInfo {
                                call_id: obj_str(&obj, "callId"),
                                is_caller: obj_bool(&obj, "isCaller"),
                                status: obj_str(&obj, "status"),
                                peer_display_name: obj_str(&obj, "peerDisplayName"),
                                offer_sdp: obj_str(&obj, "offerSdp"),
                            });
                            true
                        }
                        Ok(_) => false,
                        Err(err) => {
                            eprintln!("[net] calls:myCall fetch failed (state kept): {err}");
                            false
                        }
                    }
                }
                event = rx.recv() => match event {
                    Ok(event) => match event.kind.as_str() {
                        "call.incoming" => {
                            current = Some(MyCallInfo {
                                call_id: json_str(&event.payload, &["call_id", "callId"]),
                                is_caller: false,
                                status: "ringing".to_string(),
                                peer_display_name: json_str(
                                    &event.payload,
                                    &[
                                        "caller_display_name",
                                        "callerDisplayName",
                                        "peer_display_name",
                                        "peerDisplayName",
                                        "display_name",
                                    ],
                                ),
                                offer_sdp: json_str(&event.payload, &["offer_sdp", "offerSdp"]),
                            });
                            true
                        }
                        "call.answered" => {
                            if let Some(call) = current.as_mut() {
                                call.status = "active".to_string();
                                true
                            } else {
                                false
                            }
                        }
                        "call.declined" | "call.ended" => {
                            if current.is_some() {
                                current = None;
                                true
                            } else {
                                false
                            }
                        }
                        _ => false,
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[net] calls:myCall dropped {n} WS events");
                        false
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        client.ensure_ws();
                        rx = client.subscribe_events();
                        false
                    }
                },
            };
            if dirty && tx.send(Message::MyCallUpdated(current.clone())).is_err() {
                return;
            }
        }
    })
}

pub(crate) fn call_subscription(
    key: String,
    client: ApiClient,
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
    client: ApiClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("messages-subscription:{conversation_id}"),
        async move {
            let _ = token;
            let conv = conversation_id.clone();
            run_query_subscription(
                client,
                "messages:list",
                btreemap! {
                    "conversationId".to_string() => Value::String(conversation_id),
                },
                DEFAULT_TICK,
                move |event: &WsEvent| {
                    (event.kind.starts_with("message.") || event.kind.starts_with("reaction."))
                        && event.channel == conv
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
    client: ApiClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("pins-subscription:{conversation_id}"), async move {
        let _ = token;
        let conv = conversation_id.clone();
        run_query_subscription(
            client,
            "messages:listPinned",
            btreemap! {
                "conversationId".to_string() => Value::String(conversation_id),
            },
            DEFAULT_TICK,
            move |event: &WsEvent| {
                (event.kind.starts_with("message.") || event.kind.starts_with("reaction."))
                    && event.channel == conv
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
    client: ApiClient,
    token: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(
        format!("typing-subscription:{conversation_id}"),
        async move {
            let _ = token;
            client.ensure_ws();
            let mut rx = client.subscribe_events();
            let mut sweep = tokio::time::interval(TYPING_SWEEP);
            // There is no REST GET for "who is typing" -- rebuild the list
            // locally from `typing` events (channel == conversationId) and
            // expire entries after the server's ~5 s TTL.
            let mut typers: HashMap<String, (String, Instant)> = HashMap::new();
            let mut emitted: Vec<String> = Vec::new();
            loop {
                tokio::select! {
                    _ = sweep.tick() => {
                        let before = typers.len();
                        typers.retain(|_, (_, last)| last.elapsed() < TYPING_TTL);
                        if typers.len() == before {
                            continue;
                        }
                    }
                    event = rx.recv() => match event {
                        Ok(event) => {
                            if event.kind != "typing" || event.channel != conversation_id {
                                continue;
                            }
                            match typing_event_user(&event.payload) {
                                Some((user_id, name)) => {
                                    typers.insert(user_id, (name, Instant::now()));
                                }
                                None => continue,
                            }
                        }
                        // Typing state is transient -- a lagged/closed
                        // receiver needs no catch-up, the TTL converges it.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            client.ensure_ws();
                            rx = client.subscribe_events();
                            continue;
                        }
                    },
                }
                let names = typing_names(&typers);
                if names != emitted {
                    emitted = names.clone();
                    if tx.send(Message::TypingUpdated(names)).is_err() {
                        return;
                    }
                }
            }
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
    client: ApiClient,
    token: String,
    peer_user_id: String,
    conversation_id: String,
    tx: UnboundedSender<Message>,
) -> Job {
    job(format!("peerseal-invite:{peer_user_id}"), async move {
        let _ = token;
        // Last forwarded payload: dedups re-fetches of the same invite,
        // while still delivering a *changed* payload (host republish after
        // a relay drop).
        let mut last_payload = String::new();
        run_query_subscription(
            client,
            "peer:getInvite",
            btreemap! {
                "conversationId".to_string() => Value::String(conversation_id),
            },
            PEER_INVITE_TICK,
            |_: &WsEvent| false, // no WS event for peer invites -- tick only
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

/// Runs the in-app QR invite scanner (`media::qr_scan`) as a background
/// job for as long as `App::qr_scan_active` stays true (see
/// `App::subscription`). The OS camera thread is owned entirely by this
/// job: `StopOnDrop` guarantees the camera is released the moment the job
/// falls out of the desired set (user cancels, a code decodes, or the
/// popup closes) -- reconciliation aborts this future, which drops the
/// guard, which flips the stop flag the camera thread polls every ~200ms.
/// Same idiom `room_voice.rs` uses for its own capture threads.
pub(crate) fn qr_scan_subscription(tx: UnboundedSender<Message>) -> Job {
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    job("qr-scan", async move {
        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopOnDrop(stop.clone());
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::media::qr_scan::spawn_qr_scan_thread(frame_tx, stop);
        use crate::media::qr_scan::QrScanEvent;
        while let Some(event) = frame_rx.recv().await {
            let forwarded = match event {
                QrScanEvent::Preview(jpeg) => tx.send(Message::QrScanPreview(jpeg)).is_ok(),
                QrScanEvent::Decoded(content) => {
                    let _ = tx.send(Message::QrScanDecoded(content));
                    break;
                }
                QrScanEvent::Error(err) => {
                    let _ = tx.send(Message::QrScanError(err));
                    break;
                }
            };
            if !forwarded {
                break;
            }
        }
    })
}
