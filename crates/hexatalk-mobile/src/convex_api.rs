//! VyrApp REST client (rustls only — no OpenSSL / native-tls).
//! Same API as desktop (`https://api.vyrapp.pro`). Poll instead of WS.

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

/// Production VyrApp API. Override with `API_URL` for local experiments.
pub fn api_url() -> String {
    std::env::var("API_URL")
        .or_else(|_| std::env::var("HEXATALK_API_URL"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.vyrapp.pro".to_string())
}

/// Back-compat alias used by older call sites in this crate.
pub fn convex_url() -> String {
    api_url()
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
    pub author_plus_active: bool,
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
    /// Forgot-password: code email accepted (always "ok" from server).
    PasswordResetCodeSent,
    /// Forgot-password: password changed; go back to sign-in.
    PasswordResetOk,
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
    /// Raw JSON string from messages:search (debug / future UI).
    SearchResults(String),
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

    /// Register FCM/device token (no-op until FCM_SERVER_KEY is set server-side).
    pub fn register_push_token(&self, session_token: String, token: String) {
        let http = self.http.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": session_token.clone(),
                "token": token,
                "platform": "android",
            });
            let _ = convex_call(&http, "mutation", "push:registerToken", args).await;
            let touch = json!({
                "sessionToken": session_token,
                "deviceName": "Android",
                "platform": "android",
            });
            let _ = convex_call(&http, "mutation", "prefs:touchSession", touch).await;
        });
    }

    pub fn search_messages(&self, session_token: String, query: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "sessionToken": session_token,
                "query": query,
            });
            match convex_call(&http, "query", "messages:search", args).await {
                Ok(v) => {
                    let _ = tx.send(NetEvent::SearchResults(format!("{v}")));
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::AuthErr(e));
                }
            }
        });
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

    pub fn request_password_reset(&self, email: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({ "email": email });
            match convex_call(&http, "action", "auth:requestPasswordReset", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::PasswordResetCodeSent);
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::AuthErr(e));
                }
            }
        });
    }

    pub fn reset_password_with_code(&self, email: String, code: String, new_password: String) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "email": email,
                "code": code,
                "newPassword": new_password,
            });
            match convex_call(&http, "action", "auth:resetPasswordWithCode", args).await {
                Ok(_) => {
                    let _ = tx.send(NetEvent::PasswordResetOk);
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::AuthErr(e));
                }
            }
        });
    }

    pub fn sign_up(
        &self,
        username: String,
        password: String,
        display_name: String,
        email: String,
    ) {
        let http = self.http.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let args = json!({
                "username": username,
                "password": password,
                "displayName": display_name,
                "email": email,
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
/// Never returns empty — empty banners look like "login does nothing".
pub fn clean_error(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return "Something went wrong".to_string();
    }

    let lower = s.to_lowercase();
    if lower.contains("error sending request")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("dns error")
        || lower.contains("certificate")
        || lower.contains("tls handshake")
        || lower.contains("failed to connect")
        || lower.contains("network unreachable")
    {
        return "Network error — check your connection and try again".to_string();
    }

    // ConvexError payload may arrive as JSON: {"message":"…"}.
    if let Ok(v) = serde_json::from_str::<Value>(&s) {
        if let Some(m) = v
            .get("message")
            .and_then(|x| x.as_str())
            .or_else(|| v.as_str())
        {
            s = m.to_string();
        }
    }

    // Prefer the real application message if present.
    for marker in ["Uncaught Error: ", "ConvexError: ", "Uncaught ConvexError: "] {
        if let Some(idx) = s.find(marker) {
            s = s[idx + marker.len()..].to_string();
            break;
        }
    }

    // Drop stack frames.
    if let Some(idx) = s.find("\n    at ") {
        s = s[..idx].to_string();
    }
    if let Some(idx) = s.find('\n') {
        s = s[..idx].to_string();
    }

    // Strip "[Request ID: …] " prefix when a real message follows.
    if let Some(idx) = s.find("] ") {
        let after = s[idx + 2..].trim();
        if !after.is_empty() && !after.eq_ignore_ascii_case("server error") {
            s = after.to_string();
        }
    }

    let out = s.trim().to_string();
    // Public HTTP API redacts plain `throw new Error` to a bare "Server Error".
    if out.is_empty()
        || out.eq_ignore_ascii_case("server error")
        || (out.contains("Request ID") && out.to_lowercase().contains("server error"))
    {
        return "Login failed — check username and password".to_string();
    }
    out
}

fn extract_error_message(v: &Value) -> String {
    // ConvexError over the public HTTP API: errorMessage is often the
    // redacted shell ("[Request ID] Server Error") while the real text is
    // in `errorData` (string or { message }). Prefer errorData first.
    if let Some(data) = v.get("errorData").or_else(|| v.get("error")) {
        if let Some(msg) = data.as_str() {
            let cleaned = clean_error(msg);
            if !cleaned.to_lowercase().contains("server error") {
                return cleaned;
            }
        }
        if let Some(msg) = data.get("message").and_then(|x| x.as_str()) {
            return clean_error(msg);
        }
    }
    if let Some(msg) = v.get("errorMessage").and_then(|x| x.as_str()) {
        return clean_error(msg);
    }
    if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
        return clean_error(msg);
    }
    clean_error("request failed")
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn token_from_args(args: &Value) -> Option<String> {
    let t = arg_str(args, "sessionToken");
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

async fn rest(
    http: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{path}", api_url());
    let mut req = http.request(method, &url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| clean_error(&e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read failed: {}", clean_error(&e.to_string())))?;
    if text.trim().is_empty() {
        if status.is_success() {
            return Ok(Value::Null);
        }
        return Err(format!("{status}"));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))?;
    if !status.is_success() {
        return Err(extract_error_message(&v));
    }
    Ok(v)
}

/// Drop-in replacement for the old Convex `path` + args dispatcher.
/// Returns values in the **camelCase** shapes the mobile parsers expect.
async fn convex_call(
    http: &reqwest::Client,
    _kind: &str,
    path: &str,
    args: Value,
) -> Result<Value, String> {
    let token = token_from_args(&args);
    let tok = token.as_deref();
    match path {
        "auth:signIn" => {
            let body = json!({
                "username": arg_str(&args, "username"),
                "password": arg_str(&args, "password"),
            });
            let resp = rest(http, reqwest::Method::POST, "/auth/login", None, Some(body)).await?;
            Ok(mobile_user_from_login(&resp))
        }
        "auth:signUp" => {
            let body = json!({
                "username": arg_str(&args, "username"),
                "displayName": arg_str(&args, "displayName"),
                "email": arg_str(&args, "email"),
                "password": arg_str(&args, "password"),
            });
            let resp =
                rest(http, reqwest::Method::POST, "/auth/register", None, Some(body)).await?;
            Ok(mobile_user_from_login(&resp))
        }
        "auth:me" => {
            let resp = rest(http, reqwest::Method::GET, "/auth/me", tok, None).await?;
            Ok(mobile_user_from_login(&resp))
        }
        "auth:requestPasswordReset" => {
            let body = json!({ "email": arg_str(&args, "email") });
            rest(
                http,
                reqwest::Method::POST,
                "/auth/request-password-reset",
                None,
                Some(body),
            )
            .await?;
            Ok(Value::Null)
        }
        "auth:resetPasswordWithCode" => {
            let body = json!({
                "email": arg_str(&args, "email"),
                "code": arg_str(&args, "code"),
                "newPassword": arg_str(&args, "newPassword"),
            });
            rest(
                http,
                reqwest::Method::POST,
                "/auth/confirm-password-reset",
                None,
                Some(body),
            )
            .await?;
            Ok(Value::Null)
        }
        "conversations:listMyConversations" => {
            let resp = rest(http, reqwest::Method::GET, "/conversations", tok, None).await?;
            Ok(map_conversations(&resp))
        }
        "messages:list" => {
            let cid = arg_str(&args, "conversationId");
            let resp = rest(
                http,
                reqwest::Method::GET,
                &format!("/conversations/{cid}/messages?limit=100"),
                tok,
                None,
            )
            .await?;
            Ok(map_messages(&resp))
        }
        "messages:send" => {
            let cid = arg_str(&args, "conversationId");
            let body = json!({
                "body": arg_str(&args, "body"),
                "encrypted": arg_bool(&args, "encrypted"),
            });
            rest(
                http,
                reqwest::Method::POST,
                &format!("/conversations/{cid}/messages"),
                tok,
                Some(body),
            )
            .await?;
            Ok(Value::Null)
        }
        "conversations:getOrCreateDirect" => {
            let body = json!({ "userId": arg_str(&args, "friendUserId") });
            let resp = rest(
                http,
                reqwest::Method::POST,
                "/conversations/direct",
                tok,
                Some(body),
            )
            .await?;
            Ok(Value::String(extract_conv_id(&resp)))
        }
        "conversations:openSupportDm" => {
            let peer = arg_str(&args, "peerUserId");
            let body = if peer.is_empty() {
                json!({})
            } else {
                json!({ "userId": peer })
            };
            let resp = rest(
                http,
                reqwest::Method::POST,
                "/conversations/support",
                tok,
                Some(body),
            )
            .await?;
            Ok(Value::String(extract_conv_id(&resp)))
        }
        "friends:listFriends" => {
            let resp = rest(http, reqwest::Method::GET, "/friends", tok, None).await?;
            Ok(map_friends_list(&resp))
        }
        "friends:listIncomingRequests" => {
            let resp = rest(http, reqwest::Method::GET, "/friends/requests", tok, None).await?;
            Ok(map_incoming(&resp))
        }
        "friends:listOutgoingRequests" => {
            let resp = rest(http, reqwest::Method::GET, "/friends/requests", tok, None).await?;
            Ok(map_outgoing(&resp))
        }
        "friends:socialStats" => {
            let resp = rest(http, reqwest::Method::GET, "/friends/stats", tok, None).await?;
            Ok(json!({
                "friendsTotal": jf64_any(&resp, &["friends_total", "friendsTotal"]),
                "friendsOnline": jf64_any(&resp, &["friends_online", "friendsOnline"]),
                "incomingPending": jf64_any(&resp, &["incoming_pending", "incomingPending"]),
                "outgoingPending": jf64_any(&resp, &["outgoing_pending", "outgoingPending"]),
            }))
        }
        "friends:suggestPeople" => {
            let resp = rest(http, reqwest::Method::GET, "/friends/suggestions", tok, None).await?;
            Ok(map_suggestions_list(&resp))
        }
        "friends:sendRequest" => {
            let mut body = serde_json::Map::new();
            body.insert("username".into(), json!(arg_str(&args, "toUsername")));
            let note = arg_str(&args, "note");
            if !note.is_empty() {
                body.insert("note".into(), json!(note));
            }
            rest(
                http,
                reqwest::Method::POST,
                "/friends/requests",
                tok,
                Some(Value::Object(body)),
            )
            .await?;
            Ok(Value::Null)
        }
        "friends:respondRequest" => {
            let rid = arg_str(&args, "requestId");
            let accept = arg_bool(&args, "accept");
            let path = if accept {
                format!("/friends/requests/{rid}/accept")
            } else {
                format!("/friends/requests/{rid}/decline")
            };
            rest(http, reqwest::Method::POST, &path, tok, None).await?;
            Ok(Value::Null)
        }
        "friends:cancelRequest" => {
            let rid = arg_str(&args, "requestId");
            // Prefer decline/cancel paths; ignore 404.
            let _ = rest(
                http,
                reqwest::Method::POST,
                &format!("/friends/requests/{rid}/decline"),
                tok,
                None,
            )
            .await;
            Ok(Value::Null)
        }
        "servers:listMyServers" => {
            let resp = rest(http, reqwest::Method::GET, "/servers", tok, None).await?;
            Ok(map_servers(&resp))
        }
        "servers:listChannels" => {
            let sid = arg_str(&args, "serverId");
            let resp = rest(http, reqwest::Method::GET, &format!("/servers/{sid}"), tok, None)
                .await?;
            Ok(map_channels(&resp))
        }
        "servers:createServer" => {
            let body = json!({ "name": arg_str(&args, "name") });
            rest(http, reqwest::Method::POST, "/servers", tok, Some(body)).await?;
            Ok(Value::Null)
        }
        "servers:joinByInviteCode" => {
            let body = json!({ "inviteCode": arg_str(&args, "inviteCode") });
            rest(http, reqwest::Method::POST, "/servers/join", tok, Some(body)).await?;
            Ok(Value::Null)
        }
        "presence:heartbeat" => {
            rest(
                http,
                reqwest::Method::POST,
                "/presence/heartbeat",
                tok,
                Some(json!({})),
            )
            .await?;
            Ok(Value::Null)
        }
        "typing:setTyping" => {
            if arg_bool(&args, "typing") {
                let cid = arg_str(&args, "conversationId");
                let _ = rest(
                    http,
                    reqwest::Method::POST,
                    &format!("/conversations/{cid}/typing"),
                    tok,
                    None,
                )
                .await;
            }
            Ok(Value::Null)
        }
        "typing:whoIsTyping" => Ok(Value::Array(vec![])),
        "profile:getProfile" => {
            let uid = arg_str(&args, "userId");
            let path = if uid.is_empty() {
                "/users/me".to_string()
            } else {
                format!("/users/{uid}")
            };
            let resp = match rest(http, reqwest::Method::GET, &path, tok, None).await {
                Ok(r) => r,
                Err(_) => rest(http, reqwest::Method::GET, "/users/me", tok, None).await?,
            };
            let u = resp.get("user").unwrap_or(&resp);
            Ok(json!({
                "displayName": jstr_any(u, &["display_name", "displayName"]),
                "statusMessage": jstr_any(u, &["status_message", "statusMessage"]),
                "bio": jstr_any(u, &["bio"]),
                "avatarColor": jstr_any(u, &["avatar_color", "avatarColor"]),
            }))
        }
        "prefs:touchSession" | "push:registerToken" => Ok(Value::Null),
        "messages:search" => Err("Search not available yet".into()),
        other => Err(format!("unmapped mobile path {other}")),
    }
}

fn jstr_any(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn jf64_any(v: &Value, keys: &[&str]) -> f64 {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(n) = x
                .as_f64()
                .or_else(|| x.as_i64().map(|i| i as f64))
                .or_else(|| x.as_u64().map(|i| i as f64))
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            {
                return n;
            }
        }
    }
    0.0
}

fn list_arr<'a>(v: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    if let Some(a) = v.as_array() {
        return a.iter().collect();
    }
    for k in keys {
        if let Some(a) = v.get(*k).and_then(|x| x.as_array()) {
            return a.iter().collect();
        }
    }
    Vec::new()
}

fn extract_conv_id(resp: &Value) -> String {
    if let Some(s) = resp.as_str() {
        return s.to_string();
    }
    for path in [
        "id",
        "conversation_id",
        "conversationId",
        "conversation.id",
    ] {
        if path.contains('.') {
            let mut cur = resp;
            let mut ok = true;
            for p in path.split('.') {
                if let Some(n) = cur.get(p) {
                    cur = n;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                if let Some(s) = cur.as_str() {
                    return s.to_string();
                }
            }
        } else if let Some(s) = resp.get(path).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn mobile_user_from_login(resp: &Value) -> Value {
    let token = jstr_any(resp, &["token"]);
    let user = resp.get("user").unwrap_or(resp);
    json!({
        "token": token,
        "userId": jstr_any(user, &["id", "user_id", "userId"]),
        "username": jstr_any(user, &["username"]),
        "displayName": jstr_any(user, &["display_name", "displayName"]),
        "role": jstr_any(user, &["role"]),
    })
}

fn map_conversations(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["conversations", "items", "data"])
        .into_iter()
        .map(|m| {
            let title = jstr_any(m, &["title", "name", "peer_display_name"]);
            let title = if title.is_empty() {
                "Chat".to_string()
            } else {
                title
            };
            let last = jf64_any(m, &["last_message_at", "lastMessageAt"]);
            let last_read = jf64_any(m, &["last_read_at", "lastReadAt"]);
            let unread = m
                .get("unread")
                .and_then(|x| x.as_bool())
                .unwrap_or(last > last_read);
            json!({
                "conversationId": jstr_any(m, &["id", "conversation_id", "conversationId"]),
                "kind": jstr_any(m, &["kind"]),
                "title": title,
                "unread": unread,
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_messages(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["messages", "items", "data"])
        .into_iter()
        .map(|m| {
            let mut sent = jf64_any(m, &["sent_at", "created_at", "sentAt", "createdAt"]);
            if sent <= 0.0 {
                // ULID fallback — first 10 chars encode ms epoch.
                sent = ulid_ms(&jstr_any(m, &["id"])) as f64;
            }
            json!({
                "id": jstr_any(m, &["id"]),
                "authorId": jstr_any(m, &["author_id", "authorId"]),
                "authorName": jstr_any(m, &["author_name", "authorName"]),
                "body": jstr_any(m, &["body"]),
                "encrypted": m.get("encrypted").and_then(|x| x.as_bool()).unwrap_or(false),
                "deleted": m.get("deleted").and_then(|x| x.as_bool()).unwrap_or(false),
                "attachmentUrl": jstr_any(m, &["attachment_url", "attachmentUrl"]),
                "sentAt": sent,
                "authorPlusActive": false,
            })
        })
        .collect();
    Value::Array(rows)
}

fn ulid_ms(id: &str) -> i64 {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if id.len() < 10 {
        return 0;
    }
    let mut ts: u64 = 0;
    for c in id.chars().take(10) {
        let u = c.to_ascii_uppercase() as u8;
        let Some(idx) = ALPHABET.iter().position(|&b| b == u) else {
            return 0;
        };
        ts = (ts << 5) | (idx as u64);
    }
    if !(1_000_000_000_000..=10_000_000_000_000).contains(&ts) {
        return 0;
    }
    ts as i64
}

fn map_friends_list(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["friends", "items", "data"])
        .into_iter()
        .map(|row| {
            let person = row
                .get("user")
                .or_else(|| row.get("friend"))
                .unwrap_or(row);
            json!({
                "userId": jstr_any(person, &["id", "user_id", "userId"]),
                "username": jstr_any(person, &["username"]),
                "displayName": jstr_any(person, &["display_name", "displayName"]),
                "lastSeenAt": jf64_any(person, &["last_seen_at", "lastSeenAt"]),
                "presence": jstr_any(person, &["presence", "status"]),
                "statusMessage": jstr_any(person, &["status_message", "statusMessage"]),
                "nickname": jstr_any(row, &["nickname"]),
                "favorite": row.get("favorite").and_then(|x| x.as_bool()).unwrap_or(false),
                "mutualServers": [],
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_incoming(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["incoming", "incoming_requests", "items"])
        .into_iter()
        .map(|row| {
            let from = row.get("from").or_else(|| row.get("user")).unwrap_or(row);
            json!({
                "requestId": jstr_any(row, &["id", "request_id", "requestId"]),
                "fromUserId": jstr_any(from, &["id", "user_id", "userId"]),
                "fromUsername": jstr_any(from, &["username"]),
                "fromDisplayName": jstr_any(from, &["display_name", "displayName"]),
                "note": jstr_any(row, &["note"]),
                "sentAt": jf64_any(row, &["created_at", "sent_at", "sentAt"]),
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_outgoing(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["outgoing", "outgoing_requests", "items"])
        .into_iter()
        .map(|row| {
            let to = row.get("to").or_else(|| row.get("user")).unwrap_or(row);
            json!({
                "requestId": jstr_any(row, &["id", "request_id", "requestId"]),
                "toUsername": jstr_any(to, &["username"]),
                "toDisplayName": jstr_any(to, &["display_name", "displayName"]),
                "note": jstr_any(row, &["note"]),
                "sentAt": jf64_any(row, &["created_at", "sent_at", "sentAt"]),
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_suggestions_list(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["suggestions", "users", "items"])
        .into_iter()
        .map(|u| {
            json!({
                "userId": jstr_any(u, &["id", "user_id", "userId"]),
                "username": jstr_any(u, &["username"]),
                "displayName": jstr_any(u, &["display_name", "displayName"]),
                "mutualServers": [],
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_servers(resp: &Value) -> Value {
    let rows: Vec<Value> = list_arr(resp, &["servers", "items", "data"])
        .into_iter()
        .map(|s| {
            json!({
                "serverId": jstr_any(s, &["id", "server_id", "serverId"]),
                "name": jstr_any(s, &["name"]),
                "isOwner": s.get("is_owner").and_then(|x| x.as_bool()).unwrap_or(false),
                "inviteCode": jstr_any(s, &["invite_code", "inviteCode"]),
            })
        })
        .collect();
    Value::Array(rows)
}

fn map_channels(resp: &Value) -> Value {
    let channels = list_arr(resp, &["channels", "items"]);
    let rows: Vec<Value> = channels
        .into_iter()
        .map(|ch| {
            let t = jstr_any(ch, &["channel_type", "channelType"]);
            json!({
                "conversationId": jstr_any(ch, &["id", "conversation_id", "conversationId"]),
                "name": jstr_any(ch, &["name"]),
                "channelType": if t.is_empty() { "text".into() } else { t },
            })
        })
        .collect();
    Value::Array(rows)
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
    // Auth may nest under `value` if a caller passed the full HTTP envelope.
    let v = v.get("value").unwrap_or(v);
    let token = v.get("token")?.as_str()?.to_string();
    if token.is_empty() {
        return None;
    }
    let user_id = value_id(v.get("userId")?)?;
    let username = v.get("username")?.as_str()?.to_string();
    let display_name = v
        .get("displayName")
        .and_then(|x| x.as_str())
        .unwrap_or(username.as_str())
        .to_string();
    Some(AuthSession {
        token,
        user_id,
        username,
        display_name,
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
            author_plus_active: jbool(o, "authorPlusActive"),
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
