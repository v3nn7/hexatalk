//! `ApiClient` — drop-in replacement for `convex::ConvexClient`.
//!
//! Same call surface (`query` / `mutation` / `action` taking a
//! `"module:name"` path and a `BTreeMap<String, Value>` of args), but every
//! path is routed to a `dispatch_*` module that translates it into a REST
//! call against the new backend. Live updates arrive over a single
//! WebSocket (`ws.rs`) and are fanned out through a `broadcast` channel of
//! [`WsEvent`]s; `ensure_ws` keeps that socket running.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Method;
use tokio::sync::{broadcast, watch};

use super::value::{FunctionResult, Value};
use super::{dispatch_auth, dispatch_conv, dispatch_friends, dispatch_media, dispatch_misc};
use super::{dispatch_profile, dispatch_servers, ws};

/// Hard error from the REST layer. The message is `"{status}: {error}"` for
/// HTTP failures (status = 3-digit code — `dispatch_auth::human_or` relies
/// on that exact shape to downgrade 4xx to `FunctionResult::ErrorMessage`),
/// or a plain transport/parse description otherwise.
#[derive(Debug, Clone)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ApiError {}

/// One live-update event from the WebSocket. `kind` is the server-side
/// event type (`message.new`, `friend_request.accepted`, `typing`, ...),
/// `channel` the conversationId or userId it belongs to, `payload` the
/// raw JSON body.
#[derive(Clone, Debug)]
pub struct WsEvent {
    pub kind: String,
    pub channel: String,
    pub payload: serde_json::Value,
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    /// Session token; `watch` so the WS task wakes up on login/logout.
    token_tx: watch::Sender<Option<String>>,
    events_tx: broadcast::Sender<WsEvent>,
    ws_started: AtomicBool,
}

/// Cheap to clone (an `Arc` inside) — the state layer passes it around by
/// value exactly like it did `ConvexClient`.
#[derive(Clone)]
pub struct ApiClient {
    inner: Arc<Inner>,
}

impl ApiClient {
    /// `base_url` e.g. `https://api.vyrapp.pro` (trailing slash tolerated).
    pub fn new(base_url: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let (token_tx, _) = watch::channel(None);
        let (events_tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                http,
                base_url: base_url.trim_end_matches('/').to_string(),
                token_tx,
                events_tx,
                ws_started: AtomicBool::new(false),
            }),
        }
    }

    /// Login/logout. A `None` (logout) makes the WS task drop its socket
    /// and wait for the next token.
    pub fn set_session_token(&self, token: Option<String>) {
        self.inner.token_tx.send_replace(token);
    }

    pub fn session_token(&self) -> Option<String> {
        self.inner.token_tx.borrow().clone()
    }

    pub async fn query(
        &self,
        path: &str,
        args: BTreeMap<String, Value>,
    ) -> Result<FunctionResult, ApiError> {
        self.dispatch(path, args).await
    }

    pub async fn mutation(
        &self,
        path: &str,
        args: BTreeMap<String, Value>,
    ) -> Result<FunctionResult, ApiError> {
        self.dispatch(path, args).await
    }

    pub async fn action(
        &self,
        path: &str,
        args: BTreeMap<String, Value>,
    ) -> Result<FunctionResult, ApiError> {
        self.dispatch(path, args).await
    }

    /// Route a `"module:name"` path to the owning dispatch module.
    async fn dispatch(
        &self,
        path: &str,
        args: BTreeMap<String, Value>,
    ) -> Result<FunctionResult, ApiError> {
        let Some((module, name)) = path.split_once(':') else {
            return Err(ApiError(format!("unmapped path {path}")));
        };
        match module {
            "auth" | "email" | "prefs" | "presence" | "typing" => {
                dispatch_auth::dispatch(self, module, name, args).await
            }
            "friends" => dispatch_friends::dispatch(self, module, name, args).await,
            "conversations" | "messages" => dispatch_conv::dispatch(self, module, name, args).await,
            "servers" | "channels" | "roles" => {
                dispatch_servers::dispatch(self, module, name, args).await
            }
            "calls" | "voice" | "peer" => dispatch_media::dispatch(self, module, name, args).await,
            "profile" | "groupKeys" => dispatch_profile::dispatch(self, module, name, args).await,
            "admin" | "reports" | "bots" | "plus" => {
                dispatch_misc::dispatch(self, module, name, args).await
            }
            _ => Err(ApiError(format!("unmapped path {path}"))),
        }
    }

    /// Receiver for live WebSocket events. Subscriptions filter on
    /// `WsEvent.kind` / `WsEvent.channel`.
    pub fn subscribe_events(&self) -> broadcast::Receiver<WsEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Start the background WS task if it isn't running yet. Idempotent.
    pub fn ensure_ws(&self) {
        if self
            .inner
            .ws_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            tokio::spawn(ws::run(self.clone(), self.inner.token_tx.subscribe()));
        }
    }

    /// Public download URL for an uploaded file key.
    pub fn file_url(&self, key: &str) -> String {
        format!("{}/files/{key}", self.inner.base_url)
    }

    /// POST /files (multipart field "file") → storage key.
    pub async fn upload_file(&self, bytes: Vec<u8>, filename: &str) -> Result<String, ApiError> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = format!("{}/files", self.inner.base_url);
        let mut req = self.inner.http.post(url).multipart(form);
        if let Some(token) = self.session_token() {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError(format!("request failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError(format!("read failed: {e}")))?;
        if !status.is_success() {
            return Err(ApiError(format!(
                "{}: {}",
                status.as_u16(),
                error_message(&text)
            )));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ApiError(format!("bad json: {e}")))?;
        match json.get("key").and_then(|k| k.as_str()) {
            Some(key) if !key.is_empty() => Ok(key.to_string()),
            _ => Err(ApiError("bad json: missing file key".to_string())),
        }
    }

    /// Shared helper for every `dispatch_*` module: prepends the base URL,
    /// attaches `Authorization: Bearer` when a session token is set, sends
    /// an optional JSON body and returns the parsed JSON response. Any
    /// non-2xx becomes `ApiError("{status}: {error}")` with the server's
    /// `{"error": ...}` message when present.
    pub async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}{path}", self.inner.base_url);
        let mut req = self.inner.http.request(method, url);
        if let Some(token) = self.session_token() {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError(format!("request failed: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError(format!("read failed: {e}")))?;
        if !status.is_success() {
            return Err(ApiError(format!(
                "{}: {}",
                status.as_u16(),
                error_message(&text)
            )));
        }
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| ApiError(format!("bad json: {e}")))
    }

    // ---- internals for the WS task ----

    pub(crate) fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    pub(crate) fn publish_event(&self, event: WsEvent) {
        // No receivers (nothing subscribed yet) is fine — drop the event.
        let _ = self.inner.events_tx.send(event);
    }
}

/// Pull the human message out of an error body (`{"error": "..."}`), falling
/// back to the raw text when it isn't JSON.
fn error_message(text: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(text).ok();
    if let Some(msg) = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.as_str())
    {
        return msg.to_string();
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "request failed".to_string()
    } else {
        trimmed.to_string()
    }
}
