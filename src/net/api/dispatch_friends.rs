//! Path dispatch for the friends domain: `friends:*`.
//!
//! Each Convex path is translated into REST calls against api.vyrapp.pro
//! (Bearer token is attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON responses are rebuilt into the
//! camelCase `Value` shapes that the subscription parsers
//! (`friends_subscription`, `requests_subscription`,
//! `outgoing_requests_subscription`, `social_stats_subscription`,
//! `suggestions_subscription`, `blocked_subscription`) and the update.rs
//! call sites already expect.
//!
//! The exact JSON layout of the new API's friends endpoints is only
//! documented at the path level, so the row mappers below are deliberately
//! tolerant: they accept flat snake_case rows as well as nested
//! `{user: {...}, meta: {...}}` / `{from: {...}}` / `{to: {...}}` shapes.
//!
//! Degradations per the migration table:
//! - `friends:socialStats`   → zero-filled stats object (expected shape),
//! - `friends:suggestPeople` → empty list.

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
        // ---------- queries ----------
        ("friends", "listFriends") => {
            let resp = c.rest(Method::GET, "/friends", None).await?;
            let mut rows = list_from(&resp, &["friends", "items", "data"]);
            // API nie zwraca presence w /friends — dociągamy per friend
            // z GET /presence/:userId (best-effort, równolegle). Gdy serwer
            // zacznie zwracać presence w liście, fan-out sam się wyłączy.
            if rows.iter().any(|r| {
                jstr_any(person_part(r, &["user", "friend"]), &["presence", "status"]).is_empty()
            }) {
                let fetches: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        let uid = friend_user_id(r);
                        async move {
                            if uid.is_empty() {
                                return None;
                            }
                            c.rest(Method::GET, &format!("/presence/{uid}"), None)
                                .await
                                .ok()
                        }
                    })
                    .collect();
                let presences = futures::future::join_all(fetches).await;
                for (row, pres) in rows.iter_mut().zip(presences) {
                    let Some(p) = pres else { continue };
                    // Wstrzykuj tam, skąd czyta person_part: zagnieżdżony
                    // user/friend jeśli jest, inaczej top-level wiersza.
                    let target = if row.get("user").is_some_and(|x| x.is_object()) {
                        row.get_mut("user")
                    } else if row.get("friend").is_some_and(|x| x.is_object()) {
                        row.get_mut("friend")
                    } else {
                        Some(row)
                    };
                    let Some(serde_json::Value::Object(map)) = target else { continue };
                    map.entry("presence".to_string())
                        .or_insert_with(|| p.get("status").cloned().unwrap_or(serde_json::Value::Null));
                    map.entry("last_seen_at".to_string()).or_insert_with(|| {
                        p.get("last_seen_at").cloned().unwrap_or(serde_json::Value::Null)
                    });
                }
            }
            ok(Value::Array(
                rows.iter().map(|row| friend_object(c, row)).collect(),
            ))
        }
        ("friends", "listIncomingRequests") => {
            let resp = c.rest(Method::GET, "/friends/requests", None).await?;
            let rows = list_from(&resp, &["incoming", "incoming_requests"]);
            ok(Value::Array(
                rows.iter().map(|row| incoming_object(c, row)).collect(),
            ))
        }
        ("friends", "listOutgoingRequests") => {
            let resp = c.rest(Method::GET, "/friends/requests", None).await?;
            let rows = list_from(&resp, &["outgoing", "outgoing_requests"]);
            ok(Value::Array(
                rows.iter().map(|row| outgoing_object(c, row)).collect(),
            ))
        }
        ("friends", "listBlocked") => {
            let resp = c.rest(Method::GET, "/blocks", None).await?;
            let rows = list_from(&resp, &["blocks", "items", "data"]);
            ok(Value::Array(
                rows.iter().map(|row| blocked_object(c, row)).collect(),
            ))
        }
        // Degradation: no counters endpoint — zero-filled object in the
        // exact shape `social_stats_subscription` parses (Float64 counts).
        ("friends", "socialStats") => ok(Value::Object(BTreeMap::from([
            ("friendsTotal".to_string(), Value::Float64(0.0)),
            ("friendsOnline".to_string(), Value::Float64(0.0)),
            ("incomingPending".to_string(), Value::Float64(0.0)),
            ("outgoingPending".to_string(), Value::Float64(0.0)),
        ]))),
        // Degradation: no suggestions endpoint — empty list.
        ("friends", "suggestPeople") => ok(Value::Array(Vec::new())),
        // Not in the degradation table and cheap to serve: count the
        // pending incoming requests from the same endpoint the list uses.
        ("friends", "countPendingIncoming") => {
            let resp = c.rest(Method::GET, "/friends/requests", None).await?;
            let rows = list_from(&resp, &["incoming", "incoming_requests"]);
            ok(Value::Object(BTreeMap::from([(
                "count".to_string(),
                Value::Float64(rows.len() as f64),
            )])))
        }
        ("friends", "searchPeople") => {
            let q = arg_str(&args, "query");
            let path = format!("/users/search?q={}", url_encode(&q));
            let resp = c.rest(Method::GET, &path, None).await?;
            let rows = list_from(&resp, &["users", "items", "data"]);
            ok(Value::Array(
                rows.iter().map(|row| people_hit_object(c, row)).collect(),
            ))
        }
        // No relationship-summary endpoint on the new API and nothing calls
        // this path today — return the "no relation" shape instead of an
        // error so a future caller sees a sane default, not a crash.
        ("friends", "getRelationship") => ok(Value::Object(BTreeMap::from([
            ("relation".to_string(), Value::String("none".to_string())),
            ("requestId".to_string(), Value::String(String::new())),
            ("canSupportDm".to_string(), Value::Boolean(false)),
            ("mutualServers".to_string(), Value::Array(Vec::new())),
            ("favorite".to_string(), Value::Boolean(false)),
            ("nickname".to_string(), Value::String(String::new())),
            ("privateNote".to_string(), Value::String(String::new())),
        ]))),

        // ---------- friend requests ----------
        ("friends", "sendRequest") => {
            let mut body = serde_json::Map::new();
            body.insert(
                "username".to_string(),
                json!(arg_str(&args, "toUsername")),
            );
            let note = arg_str(&args, "note");
            if !note.is_empty() {
                body.insert("note".to_string(), json!(note));
            }
            match c
                .rest(Method::POST, "/friends/requests", Some(json!(body)))
                .await
            {
                Ok(_) => ok_null(),
                // 400 self-add, 403 blocked/privacy, 404 unknown user,
                // 409 duplicate/already friends — all carry user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("friends", "respondRequest") => {
            let request_id = arg_str(&args, "requestId");
            let action = if arg_bool(&args, "accept").unwrap_or(false) {
                "accept"
            } else {
                "decline"
            };
            match c
                .rest(
                    Method::POST,
                    &format!("/friends/requests/{request_id}/{action}"),
                    None,
                )
                .await
            {
                Ok(_) => ok_null(),
                // 404 "Request not found" / 409 "no longer pending".
                Err(err) => human_or(err),
            }
        }
        // No bulk endpoint — fan out per request, counting every row we
        // processed (the old mutation counted all of them too).
        ("friends", "respondAllIncoming") => {
            let accept = arg_bool(&args, "accept").unwrap_or(false);
            let action = if accept { "accept" } else { "decline" };
            let resp = c.rest(Method::GET, "/friends/requests", None).await?;
            let rows = list_from(&resp, &["incoming", "incoming_requests"]);
            let mut count = 0f64;
            for row in &rows {
                let id = jstr_any(row, &["id", "request_id", "requestId"]);
                if id.is_empty() {
                    continue;
                }
                if c
                    .rest(
                        Method::POST,
                        &format!("/friends/requests/{id}/{action}"),
                        None,
                    )
                    .await
                    .is_ok()
                {
                    count += 1.0;
                }
            }
            ok(Value::Object(BTreeMap::from([(
                "count".to_string(),
                Value::Float64(count),
            )])))
        }
        // No sender-cancel endpoint (decline is recipient-only on the new
        // API). Attempt the decline route so a stale request can still be
        // retracted server-side; 404/409 surface the server's message.
        ("friends", "cancelRequest") => {
            let request_id = arg_str(&args, "requestId");
            match c
                .rest(
                    Method::POST,
                    &format!("/friends/requests/{request_id}/decline"),
                    None,
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // ---------- friendship management ----------
        ("friends", "removeFriend") => {
            let user_id = arg_str(&args, "friendUserId");
            match c
                .rest(Method::DELETE, &format!("/friends/{user_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                // Idempotent in the old mutation — 404 means "already gone".
                Err(err) if err.is_status(404) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("friends", "setFriendMeta") => {
            let user_id = arg_str(&args, "friendUserId");
            let mut body = serde_json::Map::new();
            // Only send keys the caller actually passed; the API contract
            // spells these camelCase: {nickname?, favorite?, privateNote?}.
            if let Some(v) = arg_opt_str(&args, "nickname") {
                body.insert("nickname".to_string(), json!(v));
            }
            if let Some(v) = arg_bool(&args, "favorite") {
                body.insert("favorite".to_string(), json!(v));
            }
            if let Some(v) = arg_opt_str(&args, "privateNote") {
                body.insert("privateNote".to_string(), json!(v));
            }
            if body.is_empty() {
                return ok_null();
            }
            match c
                .rest(
                    Method::PATCH,
                    &format!("/friends/{user_id}/meta"),
                    Some(json!(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                // 403 "not friends with this user" etc.
                Err(err) => human_or(err),
            }
        }
        // The meta endpoint takes the *new* value, so read the current
        // favorite flag from the friends list and flip it.
        ("friends", "toggleFavorite") => {
            let user_id = arg_str(&args, "friendUserId");
            let resp = c.rest(Method::GET, "/friends", None).await?;
            let rows = list_from(&resp, &["friends", "items", "data"]);
            let current = rows
                .iter()
                .find(|row| friend_user_id(row) == user_id)
                .map(|row| friend_favorite(row))
                .unwrap_or(false);
            let next = !current;
            let body = json!({ "favorite": next });
            match c
                .rest(
                    Method::PATCH,
                    &format!("/friends/{user_id}/meta"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok(Value::Object(BTreeMap::from([(
                    "favorite".to_string(),
                    Value::Boolean(next),
                )]))),
                Err(err) => human_or(err),
            }
        }
        // Presence preference lives on the user record in the new API.
        ("friends", "setPresenceStatus") => {
            let body = json!({ "presence_status": arg_str(&args, "status") });
            match c.rest(Method::PATCH, "/users/me", Some(body)).await {
                Ok(_) => ok_null(),
                // 400 invalid enum — surface the server's message.
                Err(err) => human_or(err),
            }
        }

        // ---------- blocks ----------
        ("friends", "blockUser") => {
            let user_id = arg_str(&args, "userId");
            match c
                .rest(Method::POST, &format!("/blocks/{user_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                // 400 self-block / 404 unknown user / 409 already blocked.
                Err(err) => human_or(err),
            }
        }
        ("friends", "unblockUser") => {
            let user_id = arg_str(&args, "userId");
            match c
                .rest(Method::DELETE, &format!("/blocks/{user_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                // Idempotent in the old mutation — 404 means "not blocked".
                Err(err) if err.is_status(404) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // Admin maintenance mutation — no equivalent on the new API.
        ("friends", "purgeAutoAcceptedFriendshipsAsAdmin") => {
            err("Admin panel not available yet")
        }

        _ => Err(ApiError(format!("unmapped path {module}:{name}"))),
    }
}

// ---------- small result helpers ----------

fn ok(value: Value) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(value))
}

fn ok_null() -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(Value::Null))
}

fn err(msg: &str) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::ErrorMessage(msg.to_string()))
}

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// mutations surfaced 4xx bodies as user-facing messages ("You're already
/// friends", "This user is not accepting friend requests", …), so
/// downgrade those to `ErrorMessage` for the UI; anything else stays a
/// hard transport error.
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

/// True when the `"{status}: {error}"` payload carries this HTTP status.
trait ApiErrorStatus {
    fn is_status(&self, code: u16) -> bool;
}

impl ApiErrorStatus for ApiError {
    fn is_status(&self, code: u16) -> bool {
        self.0.starts_with(&format!("{code}:"))
    }
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

/// Read the first present key as a string.
fn jstr_any(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn jbool_any(v: &serde_json::Value, keys: &[&str], default: bool) -> bool {
    for key in keys {
        if let Some(b) = v.get(*key).and_then(|x| x.as_bool()) {
            return b;
        }
    }
    default
}

fn jf64_any(v: &serde_json::Value, keys: &[&str]) -> f64 {
    for key in keys {
        if let Some(n) = v.get(*key).and_then(|x| x.as_f64()) {
            return n;
        }
    }
    0.0
}

fn jstr_list_any(v: &serde_json::Value, keys: &[&str]) -> Vec<Value> {
    for key in keys {
        if let Some(arr) = v.get(*key).and_then(|x| x.as_array()) {
            return arr
                .iter()
                .filter_map(|x| x.as_str().map(|s| Value::String(s.to_string())))
                .collect();
        }
    }
    Vec::new()
}

/// Normalize a list response: plain array, or an object wrapping the rows
/// under one of `keys`.
fn list_from(resp: &serde_json::Value, keys: &[&str]) -> Vec<serde_json::Value> {
    if let Some(arr) = resp.as_array() {
        return arr.clone();
    }
    for key in keys {
        if let Some(arr) = resp.get(*key).and_then(|x| x.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

/// The "person" half of a row: nested object when present, else the row
/// itself (flat snake_case layout).
fn person_part<'a>(row: &'a serde_json::Value, nested: &[&str]) -> &'a serde_json::Value {
    for key in nested {
        if let Some(obj) = row.get(*key).filter(|x| x.is_object()) {
            return obj;
        }
    }
    row
}

/// Storage key → download URL; empty key stays empty. If the server
/// already sent a full URL (`avatar_url`/`avatar_image_url`), keep it.
fn avatar_url(c: &ApiClient, person: &serde_json::Value) -> String {
    let key = jstr_any(person, &["avatar_storage_key", "avatarStorageKey"]);
    if !key.is_empty() {
        return c.file_url(&key);
    }
    jstr_any(person, &["avatar_url", "avatar_image_url", "avatarImageUrl"])
}

/// `role`-based staff flag with an `is_staff` override when provided.
fn is_staff(person: &serde_json::Value) -> bool {
    if let Some(b) = person
        .get("is_staff")
        .or_else(|| person.get("isStaff"))
        .and_then(|x| x.as_bool())
    {
        return b;
    }
    matches!(
        jstr_any(person, &["role"]).as_str(),
        "admin" | "moderator" | "owner"
    )
}

fn friend_user_id(row: &serde_json::Value) -> String {
    let person = person_part(row, &["user", "friend"]);
    let id = jstr_any(person, &["id", "user_id", "userId"]);
    if !id.is_empty() {
        return id;
    }
    jstr_any(row, &["user_id", "userId", "friend_id", "friendUserId"])
}

fn friend_favorite(row: &serde_json::Value) -> bool {
    if let Some(meta) = row.get("meta").filter(|x| x.is_object()) {
        return jbool_any(meta, &["favorite"], false);
    }
    jbool_any(row, &["favorite"], false)
}

/// `friends:listFriends` row — parsed by `friends_subscription` into
/// `Friend`. Numeric ms fields are Float64, as Convex used to send them.
fn friend_object(c: &ApiClient, row: &serde_json::Value) -> Value {
    let person = person_part(row, &["user", "friend"]);
    let meta = row.get("meta").filter(|x| x.is_object()).unwrap_or(row);

    let mut obj = BTreeMap::new();
    obj.insert("userId".to_string(), Value::String(friend_user_id(row)));
    obj.insert(
        "username".to_string(),
        Value::String(jstr_any(person, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr_any(person, &["display_name", "displayName"])),
    );
    obj.insert(
        "lastSeenAt".to_string(),
        Value::Float64(jf64_any(person, &["last_seen_at", "lastSeenAt"])),
    );
    obj.insert(
        "presence".to_string(),
        Value::String(jstr_any(person, &["presence"])),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr_any(person, &["avatar_color", "avatarColor"])),
    );
    obj.insert(
        "avatarImageUrl".to_string(),
        Value::String(avatar_url(c, person)),
    );
    obj.insert(
        "publicKey".to_string(),
        Value::String(jstr_any(person, &["public_key", "publicKey"])),
    );
    obj.insert(
        "statusMessage".to_string(),
        Value::String(jstr_any(person, &["status_message", "statusMessage"])),
    );
    obj.insert(
        "bio".to_string(),
        Value::String(jstr_any(person, &["bio"])),
    );
    obj.insert(
        "nickname".to_string(),
        Value::String(jstr_any(meta, &["nickname"])),
    );
    obj.insert(
        "favorite".to_string(),
        Value::Boolean(jbool_any(meta, &["favorite"], false)),
    );
    obj.insert(
        "privateNote".to_string(),
        Value::String(jstr_any(meta, &["private_note", "privateNote"])),
    );
    obj.insert(
        "friendsSince".to_string(),
        Value::Float64(jf64_any(row, &["friends_since", "friendsSince"])),
    );
    obj.insert(
        "mutualServers".to_string(),
        Value::Array(jstr_list_any(person, &["mutual_servers", "mutualServers"])),
    );
    obj.insert("isStaff".to_string(), Value::Boolean(is_staff(person)));
    Value::Object(obj)
}

/// `friends:listIncomingRequests` row — parsed by `requests_subscription`.
fn incoming_object(c: &ApiClient, row: &serde_json::Value) -> Value {
    let person = person_part(row, &["from", "sender", "user"]);

    let mut obj = BTreeMap::new();
    obj.insert(
        "requestId".to_string(),
        Value::String(jstr_any(row, &["id", "request_id", "requestId"])),
    );
    let from_user_id = {
        let id = jstr_any(person, &["id", "user_id", "userId"]);
        if id.is_empty() {
            jstr_any(row, &["from_user_id", "fromUserId"])
        } else {
            id
        }
    };
    obj.insert("fromUserId".to_string(), Value::String(from_user_id));
    obj.insert(
        "fromUsername".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["from_username", "fromUsername"]),
            jstr_any(person, &["username"]),
        ])),
    );
    obj.insert(
        "fromDisplayName".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["from_display_name", "fromDisplayName"]),
            jstr_any(person, &["display_name", "displayName"]),
        ])),
    );
    obj.insert(
        "fromAvatarColor".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["from_avatar_color", "fromAvatarColor"]),
            jstr_any(person, &["avatar_color", "avatarColor"]),
        ])),
    );
    obj.insert(
        "fromAvatarImageUrl".to_string(),
        Value::String(avatar_url(c, person)),
    );
    obj.insert(
        "fromStatusMessage".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["from_status_message", "fromStatusMessage"]),
            jstr_any(person, &["status_message", "statusMessage"]),
        ])),
    );
    obj.insert(
        "note".to_string(),
        Value::String(jstr_any(row, &["note"])),
    );
    obj.insert(
        "sentAt".to_string(),
        Value::Float64(jf64_any(row, &["sent_at", "sentAt"])),
    );
    obj.insert(
        "lastSeenAt".to_string(),
        Value::Float64(jf64_any(person, &["last_seen_at", "lastSeenAt"])),
    );
    obj.insert(
        "presence".to_string(),
        Value::String(jstr_any(person, &["presence"])),
    );
    obj.insert(
        "mutualServers".to_string(),
        Value::Array(jstr_list_any(row, &["mutual_servers", "mutualServers"])),
    );
    obj.insert("isStaff".to_string(), Value::Boolean(is_staff(person)));
    Value::Object(obj)
}

/// `friends:listOutgoingRequests` row — parsed by
/// `outgoing_requests_subscription`.
fn outgoing_object(c: &ApiClient, row: &serde_json::Value) -> Value {
    let person = person_part(row, &["to", "target", "user"]);

    let mut obj = BTreeMap::new();
    obj.insert(
        "requestId".to_string(),
        Value::String(jstr_any(row, &["id", "request_id", "requestId"])),
    );
    let to_user_id = {
        let id = jstr_any(person, &["id", "user_id", "userId"]);
        if id.is_empty() {
            jstr_any(row, &["to_user_id", "toUserId"])
        } else {
            id
        }
    };
    obj.insert("toUserId".to_string(), Value::String(to_user_id));
    obj.insert(
        "toUsername".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["to_username", "toUsername"]),
            jstr_any(person, &["username"]),
        ])),
    );
    obj.insert(
        "toDisplayName".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["to_display_name", "toDisplayName"]),
            jstr_any(person, &["display_name", "displayName"]),
        ])),
    );
    obj.insert(
        "toAvatarColor".to_string(),
        Value::String(first_non_empty(&[
            jstr_any(row, &["to_avatar_color", "toAvatarColor"]),
            jstr_any(person, &["avatar_color", "avatarColor"]),
        ])),
    );
    obj.insert(
        "toAvatarImageUrl".to_string(),
        Value::String(avatar_url(c, person)),
    );
    obj.insert(
        "note".to_string(),
        Value::String(jstr_any(row, &["note"])),
    );
    obj.insert(
        "sentAt".to_string(),
        Value::Float64(jf64_any(row, &["sent_at", "sentAt"])),
    );
    Value::Object(obj)
}

/// `friends:listBlocked` row — parsed by `blocked_subscription`
/// (`BlockedUser` needs `userId` + `displayName`; the extra keys match the
/// old Convex shape and cost nothing).
fn blocked_object(c: &ApiClient, row: &serde_json::Value) -> Value {
    let person = person_part(row, &["user", "blocked", "target"]);

    let mut obj = BTreeMap::new();
    let user_id = {
        let id = jstr_any(person, &["id", "user_id", "userId"]);
        if id.is_empty() {
            jstr_any(row, &["blocked_id", "blockedId", "user_id", "userId"])
        } else {
            id
        }
    };
    obj.insert("userId".to_string(), Value::String(user_id));
    obj.insert(
        "username".to_string(),
        Value::String(jstr_any(person, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr_any(person, &["display_name", "displayName"])),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr_any(person, &["avatar_color", "avatarColor"])),
    );
    obj.insert(
        "avatarImageUrl".to_string(),
        Value::String(avatar_url(c, person)),
    );
    Value::Object(obj)
}

/// `friends:searchPeople` row — parsed in update.rs (AddFriendInputChanged)
/// into `PeopleHit`. `GET /users/search` returns no relation/presence data,
/// so those degrade to "none"/"offline" (the UI shows a plain "Add"
/// button; clicking it on an existing friend yields the server's 409
/// message, which is acceptable feedback).
fn people_hit_object(c: &ApiClient, row: &serde_json::Value) -> Value {
    let person = person_part(row, &["user"]);

    let mut obj = BTreeMap::new();
    obj.insert(
        "userId".to_string(),
        Value::String(jstr_any(person, &["id", "user_id", "userId"])),
    );
    obj.insert(
        "username".to_string(),
        Value::String(jstr_any(person, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr_any(person, &["display_name", "displayName"])),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr_any(person, &["avatar_color", "avatarColor"])),
    );
    obj.insert(
        "avatarImageUrl".to_string(),
        Value::String(avatar_url(c, person)),
    );
    obj.insert(
        "statusMessage".to_string(),
        Value::String(jstr_any(person, &["status_message", "statusMessage"])),
    );
    obj.insert(
        "presence".to_string(),
        Value::String(jstr_any(person, &["presence"])),
    );
    obj.insert(
        "relation".to_string(),
        Value::String({
            let r = jstr_any(person, &["relation"]);
            if r.is_empty() {
                "none".to_string()
            } else {
                r
            }
        }),
    );
    obj.insert(
        "incomingRequestId".to_string(),
        Value::String(jstr_any(person, &["incoming_request_id", "incomingRequestId"])),
    );
    obj.insert(
        "mutualServers".to_string(),
        Value::Array(jstr_list_any(person, &["mutual_servers", "mutualServers"])),
    );
    obj.insert("isStaff".to_string(), Value::Boolean(is_staff(person)));
    Value::Object(obj)
}

fn first_non_empty(candidates: &[String]) -> String {
    for s in candidates {
        if !s.is_empty() {
            return s.clone();
        }
    }
    String::new()
}

/// Minimal percent-encoding for a query-string value (unreserved chars
/// pass through) — avoids pulling in a new dependency for one call.
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
