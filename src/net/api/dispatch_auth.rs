//! Path dispatch for the auth domain: `auth:*`, `email:*`, `prefs:*`,
//! `presence:*` and `typing:*`.
//!
//! Each Convex path is translated into a REST call against api.vyrapp.pro
//! (Bearer token is attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON response is rebuilt into the
//! camelCase `Value` shape that `convex_parse.rs` (`parse_session` /
//! `parse_me`) and the call sites already expect. Paths with no endpoint on
//! shapes the UI already expects. Session list/revoke and change-password
//! hit live REST; per-conversation storage prefs stay local defaults.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::json;

use super::client::{ApiClient, ApiError};
use super::value::{FunctionResult, Value};

pub async fn dispatch(
    c: &ApiClient,
    module: &str,
    name: &str,
    args: BTreeMap<String, Value>,
) -> Result<FunctionResult, ApiError> {
    match (module, name) {
        // ---------- auth ----------
        ("auth", "signIn") => {
            let body = json!({
                "username": arg_str(&args, "username"),
                "password": arg_str(&args, "password"),
            });
            let resp = match c.rest(Method::POST, "/auth/login", Some(body)).await {
                Ok(resp) => resp,
                // 401 invalid credentials / 429 locked — show the server's message.
                Err(err) => return human_or(err),
            };
            let token = jstr(&resp, "token");
            Ok(FunctionResult::Value(user_object(
                c,
                resp.get("user").unwrap_or(&serde_json::Value::Null),
                Some(token),
            )))
        }
        ("auth", "signUp") => {
            let body = json!({
                "username": arg_str(&args, "username"),
                "displayName": arg_str(&args, "displayName"),
                "email": arg_str(&args, "email"),
                "password": arg_str(&args, "password"),
            });
            let resp = match c.rest(Method::POST, "/auth/register", Some(body)).await {
                Ok(resp) => resp,
                // "This username is taken" etc. — user-facing copy.
                Err(err) => return human_or(err),
            };
            let token = jstr(&resp, "token");
            Ok(FunctionResult::Value(user_object(
                c,
                resp.get("user").unwrap_or(&serde_json::Value::Null),
                Some(token),
            )))
        }
        ("auth", "signOut") => {
            c.rest(Method::POST, "/auth/logout", None).await?;
            ok_null()
        }
        ("auth", "me") => {
            let resp = c.rest(Method::GET, "/auth/me", None).await?;
            Ok(FunctionResult::Value(user_object(
                c,
                resp.get("user").unwrap_or(&serde_json::Value::Null),
                None,
            )))
        }
        ("auth", "changePassword") => {
            let body = json!({
                "currentPassword": arg_str(&args, "currentPassword"),
                "newPassword": arg_str(&args, "newPassword"),
            });
            match c
                .rest(Method::POST, "/auth/change-password", Some(body))
                .await
            {
                Ok(_) => ok_null(),
                // "invalid current password" etc. — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("auth", "requestPasswordReset") => {
            let body = json!({ "email": arg_str(&args, "email") });
            match c
                .rest(Method::POST, "/auth/request-password-reset", Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("auth", "resetPasswordWithCode") => {
            let body = json!({
                "email": arg_str(&args, "email"),
                "code": arg_str(&args, "code"),
                "newPassword": arg_str(&args, "newPassword"),
            });
            match c
                .rest(Method::POST, "/auth/confirm-password-reset", Some(body))
                .await
            {
                Ok(_) => ok_null(),
                // "Invalid or expired code" etc. — user-facing copy.
                Err(err) => human_or(err),
            }
        }

        // ---------- email ----------
        ("email", "requestEmailVerification") => {
            match c.rest(Method::POST, "/auth/resend-verification", None).await {
                Ok(_) => ok_null(),
                // 429 resend throttle — show the wait message.
                Err(err) => human_or(err),
            }
        }
        ("email", "verifyEmailCode") => {
            let body = json!({ "code": arg_str(&args, "code") });
            match c.rest(Method::POST, "/auth/verify-email", Some(body)).await {
                Ok(_) => ok_null(),
                // "Incorrect code" etc. — user-facing copy.
                Err(err) => human_or(err),
            }
        }

        // ---------- prefs ----------
        // Degradation: no session-touch endpoint — harmless no-op.
        ("prefs", "touchSession") => ok_null(),
        ("prefs", "setStoreChatHistory") => {
            let body = json!({ "store_chat_history": arg_bool(&args, "store").unwrap_or(true) });
            c.rest(Method::PATCH, "/users/me", Some(body)).await?;
            ok_null()
        }
        ("prefs", "setPrivacyFlags") => {
            let mut body = serde_json::Map::new();
            if let Some(v) = arg_bool(&args, "hideOnlineStatus") {
                body.insert("hide_online_status".into(), json!(v));
            }
            if let Some(v) = arg_bool(&args, "friendsOnlyDms") {
                body.insert("friends_only_dms".into(), json!(v));
            }
            if let Some(v) = arg_bool(&args, "discoverable") {
                body.insert("discoverable".into(), json!(v));
            }
            if let Some(v) = arg_opt_str(&args, "friendRequestPrivacy") {
                body.insert("friend_request_privacy".into(), json!(v));
            }
            if body.is_empty() {
                return ok_null();
            }
            c.rest(Method::PATCH, "/users/me", Some(json!(body)))
                .await?;
            ok_null()
        }
        // Degradation: per-conversation storage prefs don't exist yet —
        // report "store" (the effective default) and accept writes as no-ops.
        // Shape mirrors the old query ({store, globalStore,
        // conversationAllowsStorage}); the call site reads `store` and
        // `conversationAllowsStorage`.
        ("prefs", "getConversationStore") => {
            let mut obj = BTreeMap::new();
            obj.insert("store".to_string(), Value::Boolean(true));
            obj.insert("globalStore".to_string(), Value::Boolean(true));
            obj.insert(
                "conversationAllowsStorage".to_string(),
                Value::Boolean(true),
            );
            Ok(FunctionResult::Value(Value::Object(obj)))
        }
        ("prefs", "setConversationStore") => ok_null(),
        // DELETE /auth/sessions?others=true — kills every session except the
        // current Bearer. Call site expects `{ killed: n }`.
        ("prefs", "signOutOtherSessions") => {
            let killed = match c.rest(Method::GET, "/auth/sessions", None).await {
                Ok(resp) => session_rows(&resp)
                    .iter()
                    .filter(|s| !jbool(s, "current", false))
                    .count() as f64,
                Err(_) => 0.0,
            };
            match c
                .rest(Method::DELETE, "/auth/sessions?others=true", None)
                .await
            {
                Ok(_) => {
                    let mut obj = BTreeMap::new();
                    obj.insert("killed".to_string(), Value::Float64(killed));
                    Ok(FunctionResult::Value(Value::Object(obj)))
                }
                Err(err) => human_or(err),
            }
        }
        // GET /auth/sessions → [{id, created_at, last_seen_at, platform, current, ...}]
        ("prefs", "listSessions") => {
            let resp = match c.rest(Method::GET, "/auth/sessions", None).await {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            let rows = session_rows(&resp);
            let mut out = Vec::with_capacity(rows.len());
            for s in rows {
                let mut obj = BTreeMap::new();
                obj.insert(
                    "sessionId".to_string(),
                    Value::String(jstr(s, "id")),
                );
                obj.insert(
                    "createdAt".to_string(),
                    Value::Float64(jnum(s, "created_at")),
                );
                obj.insert(
                    "lastActiveAt".to_string(),
                    Value::Float64(jnum(s, "last_seen_at")),
                );
                obj.insert(
                    "deviceName".to_string(),
                    Value::String({
                        let d = jstr(s, "device_name");
                        if d.is_empty() {
                            jstr(s, "user_agent")
                        } else {
                            d
                        }
                    }),
                );
                obj.insert(
                    "platform".to_string(),
                    Value::String(jstr(s, "platform")),
                );
                obj.insert(
                    "current".to_string(),
                    Value::Boolean(jbool(s, "current", false)),
                );
                out.push(Value::Object(obj));
            }
            Ok(FunctionResult::Value(Value::Array(out)))
        }
        // DELETE /auth/sessions/:id
        ("prefs", "revokeSession") => {
            let session_id = arg_str(&args, "sessionId");
            if session_id.is_empty() {
                return Ok(FunctionResult::ErrorMessage("Session id required".into()));
            }
            match c
                .rest(Method::DELETE, &format!("/auth/sessions/{session_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // ---------- presence ----------
        ("presence", "heartbeat") => {
            // Best-effort activity share: servers that don't know `activity`
            // simply ignore the extra field.
            let mut body = serde_json::Map::new();
            if let Some(status) = arg_opt_str(&args, "status") {
                body.insert("status".into(), json!(status));
            }
            if let Some(activity) = arg_opt_str(&args, "activity") {
                if activity.is_empty() {
                    body.insert("activity".into(), serde_json::Value::Null);
                } else {
                    body.insert("activity".into(), json!(activity));
                }
            }
            c.rest(
                Method::POST,
                "/presence/heartbeat",
                Some(serde_json::Value::Object(body)),
            )
            .await?;
            ok_null()
        }

        // ---------- typing ----------
        ("typing", "setTyping") => {
            // The REST endpoint only marks "typing now"; there is no stop
            // call (the server-side TTL expires on its own), so
            // typing=false is a no-op.
            if arg_bool(&args, "typing").unwrap_or(false) {
                let conversation_id = arg_str(&args, "conversationId");
                c.rest(
                    Method::POST,
                    &format!("/conversations/{conversation_id}/typing"),
                    None,
                )
                .await?;
            }
            ok_null()
        }
        // No GET endpoint — subscriptions maintain this list from WS typing
        // events instead; the query path returns an empty list.
        ("typing", "whoIsTyping") => Ok(FunctionResult::Value(Value::Array(Vec::new()))),

        _ => Err(ApiError(format!("unmapped path {module}:{name}"))),
    }
}

fn ok_null() -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(Value::Null))
}

// ---------- arg helpers ----------

fn arg_str(args: &BTreeMap<String, Value>, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn arg_opt_str(args: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn arg_bool(args: &BTreeMap<String, Value>, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Boolean(b)) => Some(*b),
        _ => None,
    }
}

// ---------- JSON helpers ----------

fn jstr(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn jstr_any(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn jbool(v: &serde_json::Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

fn jnum(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|x| {
            x.as_f64()
                .or_else(|| x.as_i64().map(|n| n as f64))
                .or_else(|| x.as_u64().map(|n| n as f64))
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0.0)
}

fn session_rows(resp: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(arr) = resp.as_array() {
        return arr.iter().collect();
    }
    for key in ["sessions", "items", "data"] {
        if let Some(arr) = resp.get(key).and_then(|v| v.as_array()) {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

/// Build the camelCase session/user object `parse_session` / `parse_me`
/// expect, from the new API's snake_case `user` record. `token` is only
/// present for signIn/signUp (parse_me gets the token from the caller).
/// Plus fields don't exist on the new API and are omitted — the parsers
/// default them (plusActive=false, plusExpiresAt=0, profileBannerUrl="").
fn user_object(c: &ApiClient, user: &serde_json::Value, token: Option<String>) -> Value {
    let mut obj = BTreeMap::new();
    if let Some(token) = token {
        obj.insert("token".to_string(), Value::String(token));
    }
    obj.insert("userId".to_string(), Value::String(jstr(user, "id")));
    obj.insert(
        "username".to_string(),
        Value::String(jstr(user, "username")),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr(user, "display_name")),
    );
    obj.insert("role".to_string(), Value::String(jstr(user, "role")));
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr(user, "avatar_color")),
    );
    obj.insert(
        "statusMessage".to_string(),
        Value::String(jstr(user, "status_message")),
    );
    obj.insert("bio".to_string(), Value::String(jstr(user, "bio")));
    // Storage key → download URL; also accept camelCase / prebuilt URL fields
    // (auth:me / login shapes have varied after the Convex → VyrApp move).
    let avatar_key = jstr_any(
        user,
        &[
            "avatar_storage_key",
            "avatarStorageKey",
            "avatar_key",
            "avatarKey",
        ],
    );
    let avatar_url = if !avatar_key.is_empty() {
        c.file_url(&avatar_key)
    } else {
        jstr_any(
            user,
            &[
                "avatar_url",
                "avatarUrl",
                "avatar_image_url",
                "avatarImageUrl",
            ],
        )
    };
    obj.insert("avatarImageUrl".to_string(), Value::String(avatar_url));
    obj.insert(
        "storeChatHistory".to_string(),
        Value::Boolean(jbool(user, "store_chat_history", true)),
    );
    obj.insert(
        "hideOnlineStatus".to_string(),
        Value::Boolean(jbool(user, "hide_online_status", false)),
    );
    obj.insert(
        "friendsOnlyDms".to_string(),
        Value::Boolean(jbool(user, "friends_only_dms", false)),
    );
    obj.insert(
        "discoverable".to_string(),
        Value::Boolean(jbool(user, "discoverable", true)),
    );
    obj.insert(
        "friendRequestPrivacy".to_string(),
        Value::String(jstr(user, "friend_request_privacy")),
    );
    obj.insert(
        "presenceStatus".to_string(),
        Value::String(jstr(user, "presence_status")),
    );
    obj.insert("email".to_string(), Value::String(jstr(user, "email")));
    obj.insert(
        "emailVerified".to_string(),
        Value::Boolean(jbool(user, "email_verified", false)),
    );
    Value::Object(obj)
}

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// actions surfaced 4xx bodies as user-facing messages (e.g. "This
/// username is taken", "Incorrect code"), so downgrade those to
/// `ErrorMessage` for the UI; anything else stays a hard error.
fn human_or(err: ApiError) -> Result<FunctionResult, ApiError> {
    let msg = err.0;
    let mut parts = msg.splitn(2, ": ");
    let status = parts.next().unwrap_or("");
    let is_4xx = status.len() == 3
        && status.starts_with('4')
        && status.chars().all(|ch| ch.is_ascii_digit());
    if is_4xx {
        let body = parts.next().unwrap_or("").trim();
        return Ok(FunctionResult::ErrorMessage(if body.is_empty() {
            "Request failed".to_string()
        } else {
            body.to_string()
        }));
    }
    Err(ApiError(msg))
}
