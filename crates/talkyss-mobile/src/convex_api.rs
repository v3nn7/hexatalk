//! Convex **HTTP** client (rustls only — no OpenSSL / native-tls).
//! Same deployment as desktop. Uses poll instead of WS subscriptions.

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Production deployment (same as desktop). Override at runtime with
/// `CONVEX_URL` / `NEXT_PUBLIC_CONVEX_URL` for local Convex dev.
pub fn convex_url() -> String {
    std::env::var("CONVEX_URL")
        .or_else(|_| std::env::var("NEXT_PUBLIC_CONVEX_URL"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            "https://scrupulous-bear-861.eu-west-1.convex.cloud".to_string()
        })
}

#[derive(Clone, Debug)]
pub struct AuthSession {
    pub token: String,
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Clone, Debug, Default)]
pub struct ConversationRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub unread: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MessageRow {
    pub id: String,
    pub author_id: String,
    pub author_name: String,
    pub body: String,
    pub encrypted: bool,
    pub deleted: bool,
    pub attachment_url: String,
    pub sent_at: f64,
}

#[derive(Clone, Debug, Default)]
pub struct FriendRow {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub online: bool,
    pub presence: String,
    pub status_message: String,
    pub nickname: String,
    pub favorite: bool,
    pub mutual_servers: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SocialStats {
    pub friends_total: u32,
    pub friends_online: u32,
    pub incoming_pending: u32,
    pub outgoing_pending: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SuggestionRow {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub mutual_servers: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FriendRequestRow {
    pub request_id: String,
    pub from_user_id: String,
    pub from_username: String,
    pub from_display_name: String,
    pub note: String,
    pub sent_at: f64,
}

#[derive(Clone, Debug, Default)]
pub struct OutgoingRequestRow {
    pub request_id: String,
    pub to_username: String,
    pub to_display_name: String,
    pub note: String,
    pub sent_at: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ServerRow {
    pub server_id: String,
    pub name: String,
    pub is_owner: bool,
    pub invite_code: String,
}

#[derive(Clone, Debug, Default)]
pub struct ChannelRow {
    pub conversation_id: String,
    pub name: String,
    pub channel_type: String,
}

/// Must match `AVATAR_PALETTE` in desktop's src/main.rs and convex/profile.ts —
/// the server rejects any color outside this list.
pub const AVATAR_PALETTE: [&str; 8] = [
    "#3FB36B", "#2E9E6B", "#7FCBA0", "#2F8F57", "#A9B85E", "#5FB98C", "#27814F", "#9FD3B5",
];

#[derive(Clone, Debug, Default)]
pub struct ProfileData {
    pub display_name: String,
    pub status_message: String,
    pub bio: String,
    pub avatar_color: String,
}

#[derive(Debug)]
pub enum NetEvent {
    AuthOk(AuthSession),
    AuthErr(String),
    Conversations(Vec<ConversationRow>),
    Messages(Vec<MessageRow>),
    Friends(Vec<FriendRow>),
    FriendRequests(Vec<FriendRequestRow>),
    OutgoingRequests(Vec<OutgoingRequestRow>),
    SocialStats(SocialStats),
    Suggestions(Vec<SuggestionRow>),
    Servers(Vec<ServerRow>),
    Channels(Vec<ChannelRow>),
    Typing(Vec<String>),
    Status(String),
    Error(String),
    SentOk,
    Profile(ProfileData),
    /// Current member's permission bitmask for the server just opened (see
    /// `PERM_*` constants) — `None` while not yet loaded.
    MyServerPermissions(u32),
}

struct LiveWatch {
    kind: WatchKind,
    token: String,
    extra_id: String, // conversation or server id
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchKind {
    Home,
    Chat,
    Channels,
}

pub struct Backend {
    rt: tokio::runtime::Runtime,
    http: reqwest::Client,
    pub tx: mpsc::UnboundedSender<NetEvent>,
    rx: Mutex<mpsc::UnboundedReceiver<NetEvent>>,
    watch: Arc<Mutex<Option<LiveWatch>>>,
}

impl Backend {
    pub fn new() -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .use_rustls_tls()
            .build()?;
        let watch = Arc::new(Mutex::new(None));
        let backend = Self {
            rt,
            http,
            tx: tx.clone(),
            rx: Mutex::new(rx),
            watch: watch.clone(),
        };
        // Poller task — ~1.5s refresh (smooth enough, low CPU).
        let http = backend.http.clone();
        let watch_c = watch;
        let tx_c = tx;
        backend.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let snap = { watch_c.lock().await.clone() };
                let Some(w) = snap else { continue };
                match w.kind {
                    WatchKind::Home => {
                        poll_home(&http, &tx_c, &w.token).await;
                    }
                    WatchKind::Chat => {
                        poll_chat(&http, &tx_c, &w.token, &w.extra_id).await;
                    }
                    WatchKind::Channels => {
                        poll_channels(&http, &tx_c, &w.token, &w.extra_id).await;
                    }
                }
            }
        });
        Ok(backend)
    }

    pub fn poll(&self) -> Vec<NetEvent> {
        let mut out = Vec::new();
        let mut guard = self.rx.blocking_lock();
        while let Ok(ev) = guard.try_recv() {
            out.push(ev);
        }
        out
    }

    pub fn ensure_connected(&self) {
        // HTTP is stateless — nothing to open.
    }

    pub fn sign_in(&self, username: String, password: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "username": username, "password": password });
            match convex_call(&http, "action", "auth:signIn", args).await {
                Ok(v) => match parse_auth(&v) {
                    Some(s) => {
                        let _ = tx.send(NetEvent::AuthOk(s));
                    }
                    None => {
                        let _ = tx.send(NetEvent::AuthErr("Bad auth response".into()));
                    }
                },
                Err(e) => {
                    let _ = tx.send(NetEvent::AuthErr(e));
                }
            }
        });
    }

    pub fn sign_up(&self, username: String, password: String, display_name: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "username": username,
                "password": password,
                "displayName": display_name,
            });
            match convex_call(&http, "action", "auth:signUp", args).await {
                Ok(v) => match parse_auth(&v) {
                    Some(s) => {
                        let _ = tx.send(NetEvent::AuthOk(s));
                    }
                    None => {
                        let _ = tx.send(NetEvent::AuthErr("Bad auth response".into()));
                    }
                },
                Err(e) => {
                    let _ = tx.send(NetEvent::AuthErr(e));
                }
            }
        });
    }

    pub fn subscribe_home(&self, token: String) {
        let watch = self.watch.clone();
        let http = self.http.clone();
        let tx = self.tx.clone();
        let token_c = token.clone();
        self.rt.spawn(async move {
            *watch.lock().await = Some(LiveWatch {
                kind: WatchKind::Home,
                token: token_c.clone(),
                extra_id: String::new(),
            });
            poll_home(&http, &tx, &token_c).await;
        });
    }

    pub fn subscribe_chat(&self, token: String, conversation_id: String) {
        let watch = self.watch.clone();
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            *watch.lock().await = Some(LiveWatch {
                kind: WatchKind::Chat,
                token: token.clone(),
                extra_id: conversation_id.clone(),
            });
            poll_chat(&http, &tx, &token, &conversation_id).await;
        });
    }

    pub fn subscribe_channels(&self, token: String, server_id: String) {
        let watch = self.watch.clone();
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            *watch.lock().await = Some(LiveWatch {
                kind: WatchKind::Channels,
                token: token.clone(),
                extra_id: server_id.clone(),
            });
            poll_channels(&http, &tx, &token, &server_id).await;
        });
    }

    pub fn send_message(&self, token: String, conversation_id: String, body: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "conversationId": conversation_id,
                "body": body,
            });
            match convex_call(&http, "mutation", "messages:send", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::SentOk);
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn open_dm(&self, token: String, friend_user_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "friendUserId": friend_user_id,
            });
            match convex_call(&http, "mutation", "conversations:getOrCreateDirect", args).await {
                Ok(v) => {
                    if let Some(id) = value_id(&v) {
                        let _ = tx.send(NetEvent::Status(format!("OPEN_DM:{id}")));
                    } else {
                        let _ = tx.send(NetEvent::Error("open dm: bad id".into()));
                    }
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn heartbeat(&self, token: String) {
        let http = self.http.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token });
            let _ = convex_call(&http, "mutation", "presence:heartbeat", args).await;
        });
    }

    pub fn set_typing(&self, token: String, conversation_id: String, typing: bool) {
        let http = self.http.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "conversationId": conversation_id,
                "typing": typing,
            });
            let _ = convex_call(&http, "mutation", "typing:setTyping", args).await;
        });
    }

    pub fn send_friend_request(&self, token: String, to_username: String, note: Option<String>) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let mut args = json!({ "sessionToken": token, "toUsername": to_username });
            if let Some(n) = note {
                if !n.trim().is_empty() {
                    args["note"] = json!(n.trim());
                }
            }
            match convex_call(&http, "mutation", "friends:sendRequest", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Friend request sent".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn respond_friend_request(&self, token: String, request_id: String, accept: bool) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "requestId": request_id,
                "accept": accept,
            });
            match convex_call(&http, "mutation", "friends:respondRequest", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status(if accept {
                        "Friend added".into()
                    } else {
                        "Request declined".into()
                    }));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn cancel_friend_request(&self, token: String, request_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "requestId": request_id,
            });
            match convex_call(&http, "mutation", "friends:cancelRequest", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Request cancelled".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn open_support_dm(&self, token: String, peer_user_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "peerUserId": peer_user_id,
            });
            match convex_call(&http, "mutation", "conversations:openSupportDm", args).await {
                Ok(v) => {
                    if let Some(id) = value_id(&v) {
                        let _ = tx.send(NetEvent::Status(format!("OPEN_DM:{id}")));
                    } else {
                        let _ = tx.send(NetEvent::Status("Support chat opened".into()));
                    }
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn create_server(&self, token: String, name: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token, "name": name });
            match convex_call(&http, "mutation", "servers:createServer", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Server created".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn join_server(&self, token: String, invite_code: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token, "inviteCode": invite_code });
            match convex_call(&http, "mutation", "servers:joinByInviteCode", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Joined server".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn fetch_profile(&self, token: String, user_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token, "userId": user_id });
            match convex_call(&http, "query", "profile:getProfile", args).await {
                Ok(v) => {
                    let _ = tx.send(NetEvent::Profile(ProfileData {
                        display_name: jstr(&v, "displayName"),
                        status_message: jstr(&v, "statusMessage"),
                        bio: jstr(&v, "bio"),
                        avatar_color: jstr(&v, "avatarColor"),
                    }));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn update_profile(
        &self,
        token: String,
        display_name: String,
        status_message: String,
        bio: String,
        avatar_color: String,
    ) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "displayName": display_name,
                "statusMessage": status_message,
                "bio": bio,
                "avatarColor": avatar_color,
            });
            match convex_call(&http, "mutation", "profile:updateProfile", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Profile saved".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn fetch_my_permissions(&self, token: String, server_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token, "serverId": server_id });
            match convex_call(&http, "query", "roles:myPermissions", args).await {
                Ok(v) => {
                    let is_owner = jbool(&v, "isOwner");
                    let perms = jf64(&v, "permissions") as u32;
                    // Owner already gets every bit server-side, but be
                    // defensive in case a permissions query races ahead of
                    // an ownership change.
                    let perms = if is_owner { u32::MAX } else { perms };
                    let _ = tx.send(NetEvent::MyServerPermissions(perms));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn create_channel(&self, token: String, server_id: String, name: String, is_voice: bool) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "serverId": server_id,
                "name": name,
                "channelType": if is_voice { "voice" } else { "text" },
            });
            match convex_call(&http, "mutation", "servers:createChannel", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Channel created".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn rename_channel(&self, token: String, conversation_id: String, name: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "conversationId": conversation_id,
                "name": name,
            });
            match convex_call(&http, "mutation", "servers:renameChannel", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Channel renamed".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn delete_channel(&self, token: String, conversation_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "sessionToken": token, "conversationId": conversation_id });
            match convex_call(&http, "mutation", "servers:deleteChannel", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Channel deleted".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn create_group(&self, token: String, name: String, member_ids: Vec<String>) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "name": name,
                "memberUserIds": member_ids,
            });
            match convex_call(&http, "mutation", "conversations:createGroup", args).await {
                Ok(v) => {
                    if let Some(id) = value_id(&v) {
                        let _ = tx.send(NetEvent::Status(format!("OPEN_DM:{id}")));
                        let _ = tx.send(NetEvent::Status("Group created".into()));
                    } else {
                        let _ = tx.send(NetEvent::Status("Group created".into()));
                    }
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn clear_conversation(&self, token: String, conversation_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let mut done = false;
            let mut total = 0i64;
            while !done {
                let args = json!({
                    "sessionToken": token,
                    "conversationId": conversation_id,
                });
                match convex_call(&http, "mutation", "messages:clearConversation", args).await {
                    Ok(v) => {
                        total += v.get("purged").and_then(|x| x.as_i64()).unwrap_or(0);
                        done = v.get("done").and_then(|x| x.as_bool()).unwrap_or(true);
                    }
                    Err(e) => {
                        let _ = tx.send(NetEvent::Error(e));
                        return;
                    }
                }
            }
            let _ = tx.send(NetEvent::Status(format!("Cleared ({total})")));
        });
    }

    pub fn toggle_reaction(&self, token: String, message_id: String, emoji: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "messageId": message_id,
                "emoji": emoji,
            });
            if let Err(e) = convex_call(&http, "mutation", "messages:toggleReaction", args).await {
                let _ = tx.send(NetEvent::Error(e));
            }
        });
    }

    pub fn delete_message(&self, token: String, message_id: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": token,
                "messageId": message_id,
            });
            match convex_call(&http, "mutation", "messages:remove", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::Status("Message deleted".into()));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }

    pub fn send_message_reply(
        &self,
        token: String,
        conversation_id: String,
        body: String,
        reply_to: Option<String>,
    ) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let mut args = json!({
                "sessionToken": token,
                "conversationId": conversation_id,
                "body": body,
            });
            if let Some(rid) = reply_to {
                args["replyToMessageId"] = json!(rid);
            }
            match convex_call(&http, "mutation", "messages:send", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::SentOk);
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Error(e));
                }
            }
        });
    }
}

impl Clone for LiveWatch {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            token: self.token.clone(),
            extra_id: self.extra_id.clone(),
        }
    }
}

/// Strip Convex HTTP error wrappers into a short human message.
pub fn clean_error(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // "[Request ID: …] Server Error\nUncaught Error: Actual message\n    at …"
    if let Some(idx) = s.find("Uncaught Error: ") {
        s = s[idx + "Uncaught Error: ".len()..].to_string();
    } else if let Some(idx) = s.find("Server Error") {
        s = s[idx..].to_string();
        s = s.replacen("Server Error", "", 1).trim().to_string();
    }
    // Drop stack frames.
    if let Some(idx) = s.find("\n    at ") {
        s = s[..idx].to_string();
    }
    if let Some(idx) = s.find('\n') {
        s = s[..idx].to_string();
    }
    s.trim().to_string()
}

async fn convex_call(
    http: &reqwest::Client,
    kind: &str, // "query" | "mutation" | "action"
    path: &str,
    args: Value,
) -> Result<Value, String> {
    let url = format!("{}/api/{kind}", convex_url());
    let body = json!({
        "path": path,
        "args": args,
        "format": "json",
    });
    let resp = http
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| clean_error(&e.to_string()))?;
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| clean_error(&e.to_string()))?;
    if !status.is_success() {
        let msg = v
            .get("errorMessage")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("message").and_then(|x| x.as_str()))
            .unwrap_or("request failed");
        return Err(clean_error(msg));
    }
    // Success shapes: { status: "success", value: ... } or raw value
    if let Some(st) = v.get("status").and_then(|x| x.as_str()) {
        if st == "error" || st == "failure" {
            let msg = v
                .get("errorMessage")
                .or_else(|| v.get("error"))
                .and_then(|x| x.as_str())
                .unwrap_or("convex error");
            return Err(clean_error(msg));
        }
        if let Some(val) = v.get("value") {
            return Ok(val.clone());
        }
    }
    Ok(v)
}

async fn poll_home(http: &reqwest::Client, tx: &mpsc::UnboundedSender<NetEvent>, token: &str) {
    let args = json!({ "sessionToken": token });
    match convex_call(
        http,
        "query",
        "conversations:listMyConversations",
        args.clone(),
    )
    .await
    {
        Ok(v) => {
            let _ = tx.send(NetEvent::Conversations(parse_conversations(&v)));
        }
        Err(e) => {
            let _ = tx.send(NetEvent::Error(e));
            return;
        }
    }
    if let Ok(v) = convex_call(http, "query", "friends:listFriends", args.clone()).await {
        let _ = tx.send(NetEvent::Friends(parse_friends(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "friends:listIncomingRequests", args.clone()).await {
        let _ = tx.send(NetEvent::FriendRequests(parse_friend_requests(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "friends:listOutgoingRequests", args.clone()).await {
        let _ = tx.send(NetEvent::OutgoingRequests(parse_outgoing_requests(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "friends:socialStats", args.clone()).await {
        let _ = tx.send(NetEvent::SocialStats(parse_social_stats(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "friends:suggestPeople", args.clone()).await {
        let _ = tx.send(NetEvent::Suggestions(parse_suggestions(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "servers:listMyServers", args).await {
        let _ = tx.send(NetEvent::Servers(parse_servers(&v)));
    }
}

fn value_id(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    // Convex sometimes wraps IDs
    if let Some(s) = v.get("$id").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = v.get("id").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    None
}

async fn poll_chat(
    http: &reqwest::Client,
    tx: &mpsc::UnboundedSender<NetEvent>,
    token: &str,
    conversation_id: &str,
) {
    let args = json!({
        "sessionToken": token,
        "conversationId": conversation_id,
    });
    if let Ok(v) = convex_call(http, "query", "messages:list", args.clone()).await {
        let _ = tx.send(NetEvent::Messages(parse_messages(&v)));
    }
    if let Ok(v) = convex_call(http, "query", "typing:whoIsTyping", args).await {
        let names = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let _ = tx.send(NetEvent::Typing(names));
    }
}

async fn poll_channels(
    http: &reqwest::Client,
    tx: &mpsc::UnboundedSender<NetEvent>,
    token: &str,
    server_id: &str,
) {
    let args = json!({
        "sessionToken": token,
        "serverId": server_id,
    });
    if let Ok(v) = convex_call(http, "query", "servers:listChannels", args).await {
        let _ = tx.send(NetEvent::Channels(parse_channels(&v)));
    }
}

fn parse_auth(v: &Value) -> Option<AuthSession> {
    Some(AuthSession {
        token: v.get("token")?.as_str()?.to_string(),
        user_id: v.get("userId")?.as_str()?.to_string(),
        username: v.get("username")?.as_str()?.to_string(),
        display_name: v.get("displayName")?.as_str()?.to_string(),
        role: v
            .get("role")
            .and_then(|x| x.as_str())
            .unwrap_or("user")
            .to_string(),
    })
}

fn jstr(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn jbool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn jf64(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
        .unwrap_or(0.0)
}

fn parse_conversations(v: &Value) -> Vec<ConversationRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| ConversationRow {
            id: jstr(o, "conversationId"),
            kind: jstr(o, "kind"),
            title: jstr(o, "title"),
            unread: jbool(o, "unread"),
        })
        .collect()
}

fn parse_messages(v: &Value) -> Vec<MessageRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| MessageRow {
            id: jstr(o, "id"),
            author_id: jstr(o, "authorId"),
            author_name: jstr(o, "authorName"),
            body: jstr(o, "body"),
            encrypted: jbool(o, "encrypted"),
            deleted: jbool(o, "deleted"),
            attachment_url: jstr(o, "attachmentUrl"),
            sent_at: jf64(o, "sentAt"),
        })
        .collect()
}

fn parse_friends(v: &Value) -> Vec<FriendRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    arr.iter()
        .map(|o| {
            let last = jf64(o, "lastSeenAt");
            let presence = jstr(o, "presence");
            let online = presence == "online"
                || presence == "idle"
                || presence == "dnd"
                || (last > 0.0 && (now - last) < 90_000.0);
            FriendRow {
                user_id: jstr(o, "userId"),
                username: jstr(o, "username"),
                display_name: jstr(o, "displayName"),
                online,
                presence,
                status_message: jstr(o, "statusMessage"),
                nickname: jstr(o, "nickname"),
                favorite: jbool(o, "favorite"),
                mutual_servers: jstr_list(o, "mutualServers"),
            }
        })
        .collect()
}

fn parse_social_stats(v: &Value) -> SocialStats {
    SocialStats {
        friends_total: jf64(v, "friendsTotal") as u32,
        friends_online: jf64(v, "friendsOnline") as u32,
        incoming_pending: jf64(v, "incomingPending") as u32,
        outgoing_pending: jf64(v, "outgoingPending") as u32,
    }
}

fn parse_suggestions(v: &Value) -> Vec<SuggestionRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| SuggestionRow {
            user_id: jstr(o, "userId"),
            username: jstr(o, "username"),
            display_name: jstr(o, "displayName"),
            mutual_servers: jstr_list(o, "mutualServers"),
        })
        .collect()
}

fn jstr_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_friend_requests(v: &Value) -> Vec<FriendRequestRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| FriendRequestRow {
            request_id: jstr(o, "requestId"),
            from_user_id: jstr(o, "fromUserId"),
            from_username: jstr(o, "fromUsername"),
            from_display_name: jstr(o, "fromDisplayName"),
            note: jstr(o, "note"),
            sent_at: jf64(o, "sentAt"),
        })
        .collect()
}

fn parse_outgoing_requests(v: &Value) -> Vec<OutgoingRequestRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| OutgoingRequestRow {
            request_id: jstr(o, "requestId"),
            to_username: jstr(o, "toUsername"),
            to_display_name: jstr(o, "toDisplayName"),
            note: jstr(o, "note"),
            sent_at: jf64(o, "sentAt"),
        })
        .collect()
}

fn parse_servers(v: &Value) -> Vec<ServerRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| ServerRow {
            server_id: jstr(o, "serverId"),
            name: jstr(o, "name"),
            is_owner: jbool(o, "isOwner"),
            invite_code: jstr(o, "inviteCode"),
        })
        .collect()
}

fn parse_channels(v: &Value) -> Vec<ChannelRow> {
    let Some(arr) = v.as_array() else { return vec![] };
    arr.iter()
        .map(|o| {
            let t = jstr(o, "channelType");
            ChannelRow {
                conversation_id: jstr(o, "conversationId"),
                name: jstr(o, "name"),
                channel_type: if t.is_empty() { "text".into() } else { t },
            }
        })
        .collect()
}
