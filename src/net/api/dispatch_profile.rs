//! Path dispatch for the profile/E2EE domain: `profile:*` and `groupKeys:*`.
//!
//! Each Convex path is translated into REST calls against api.vyrapp.pro
//! (Bearer token is attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON response is rebuilt into the
//! camelCase `Value` shape that `convex_parse.rs` (`parse_profile_view`) and
//! the call sites in `src/state/update.rs` / `src/state/app.rs` already
//! expect.
//!
//! `profile:getProfile` has no dedicated endpoint on the new API, so the
//! profile card is composed from `GET /users/me`, `GET /friends`,
//! `GET /friends/requests` and `GET /presence/:userId` — anything the
//! composed sources don't cover degrades to empty-but-correct defaults
//! (never a crash). Avatar upload itself is NOT mapped here
//! (`generateAvatarUploadUrl` → ErrorMessage); the call sites upload via
//! `client.upload_file` and only attach the resulting key through
//! `profile:setAvatarImage`.
//!
//! `groupKeys:*` maps onto the opaque-ciphertext key-package endpoints
//! (`PUT`/`GET /conversations/:id/key-packages`); sealed payloads pass
//! through unchanged — the server never sees plaintext keys.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Method;
use serde_json::json;

use super::client::{ApiClient, ApiError};
use super::value::{FunctionResult, Value};

/// Same online window the old Convex `profile:getProfile` used.
const ONLINE_MS: i64 = 90_000;

pub async fn dispatch(
    c: &ApiClient,
    module: &str,
    name: &str,
    args: BTreeMap<String, Value>,
) -> Result<FunctionResult, ApiError> {
    match (module, name) {
        // ---------- profile ----------
        ("profile", "setPublicKey") => {
            let body = json!({ "public_key": arg_str(&args, "publicKey") });
            match c.rest(Method::PATCH, "/users/me", Some(body)).await {
                Ok(_) => ok_null(),
                // "Invalid public key" style validation — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("profile", "updateProfile") => {
            let body = json!({
                "display_name": arg_str(&args, "displayName"),
                "status_message": arg_str(&args, "statusMessage"),
                "bio": arg_str(&args, "bio"),
                "avatar_color": arg_str(&args, "avatarColor"),
            });
            match c.rest(Method::PATCH, "/users/me", Some(body)).await {
                Ok(_) => ok_null(),
                // "Display name is too long" etc. — shown in settings.
                Err(err) => human_or(err),
            }
        }
        ("profile", "getProfile") => get_profile(c, &args).await,
        ("profile", "search") => {
            let q = arg_opt_str(&args, "query")
                .or_else(|| arg_opt_str(&args, "q"))
                .unwrap_or_default();
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/users/search?q={}", urlencode(&q)),
                    None,
                )
                .await?;
            let mut out = Vec::new();
            if let Some(users) = resp.get("users").and_then(|u| u.as_array()) {
                for u in users {
                    out.push(search_user_object(c, u));
                }
            }
            Ok(FunctionResult::Value(Value::Array(out)))
        }
        // Upload flow is handled by the call sites via `client.upload_file`;
        // the legacy two-step upload-url dance no longer exists.
        ("profile", "generateAvatarUploadUrl") => Ok(FunctionResult::ErrorMessage(
            "Avatar upload is handled internally now".to_string(),
        )),
        ("profile", "setAvatarImage") => {
            let key = arg_str(&args, "storageId");
            let body = json!({ "avatar_storage_key": key });
            match c.rest(Method::PATCH, "/users/me", Some(body)).await {
                Ok(_) => {
                    let key = arg_str(&args, "storageId");
                    Ok(FunctionResult::Value(Value::String(if key.is_empty() {
                        String::new()
                    } else {
                        c.file_url(&key)
                    })))
                }
                Err(err) => human_or(err),
            }
        }
        ("profile", "removeAvatarImage") => {
            let body = json!({ "avatar_storage_key": serde_json::Value::Null });
            c.rest(Method::PATCH, "/users/me", Some(body)).await?;
            ok_null()
        }

        // ---------- groupKeys (E2EE) ----------
        // My sealed package for the conversation (latest epoch). The new API
        // keys packages by epoch; without a known epoch we ask for whatever
        // the server considers current and pick the newest entry.
        ("groupKeys", "myPackage") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = match c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}/key-packages"),
                    None,
                )
                .await
            {
                Ok(resp) => resp,
                // 404 = no package sealed for me yet — the bootstrap flow
                // treats that exactly like Convex returning null.
                Err(err) if err.0.starts_with("404") => {
                    return Ok(FunctionResult::Value(Value::Null));
                }
                Err(err) => return Err(err),
            };
            match newest_package(&resp) {
                Some((epoch, sealed, eph)) => {
                    let mut obj = BTreeMap::new();
                    obj.insert("epoch".to_string(), Value::Float64(epoch as f64));
                    obj.insert("sealedKey".to_string(), Value::String(sealed));
                    obj.insert("ephPublicKey".to_string(), Value::String(eph));
                    // Call sites only read epoch/sealedKey/ephPublicKey; the
                    // conversation epoch mirrors the package epoch here.
                    obj.insert(
                        "conversationEpoch".to_string(),
                        Value::Float64(epoch as f64),
                    );
                    Ok(FunctionResult::Value(Value::Object(obj)))
                }
                None => Ok(FunctionResult::Value(Value::Null)),
            }
        }
        // Member public keys for sealing: composed from
        // `GET /conversations/:id` (conversation + members). DMs don't use
        // group keys — mirror the old `{epoch: 0, members: []}`.
        ("groupKeys", "listMemberPublicKeys") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}"),
                    None,
                )
                .await?;
            let conv = resp.get("conversation").unwrap_or(&resp);
            let kind = jstr_any(conv, &["kind", "type"]);
            if !kind.is_empty() && kind != "group" && kind != "channel" {
                let mut obj = BTreeMap::new();
                obj.insert("epoch".to_string(), Value::Float64(0.0));
                obj.insert("members".to_string(), Value::Array(Vec::new()));
                return Ok(FunctionResult::Value(Value::Object(obj)));
            }
            let epoch = jnum_any(conv, &["key_epoch", "keyEpoch"]);
            let mut members = Vec::new();
            if let Some(list) = resp.get("members").and_then(|m| m.as_array()) {
                for m in list {
                    let user = m.get("user").unwrap_or(m);
                    let user_id = jstr_any(user, &["id", "user_id", "userId"]);
                    if user_id.is_empty() {
                        continue;
                    }
                    let public_key =
                        jstr_any(user, &["public_key", "publicKey"]);
                    let mut entry = BTreeMap::new();
                    entry.insert("userId".to_string(), Value::String(user_id));
                    entry.insert(
                        "publicKey".to_string(),
                        Value::String(public_key),
                    );
                    members.push(Value::Object(entry));
                }
            }
            let mut obj = BTreeMap::new();
            obj.insert("epoch".to_string(), Value::Float64(epoch));
            obj.insert("members".to_string(), Value::Array(members));
            Ok(FunctionResult::Value(Value::Object(obj)))
        }
        // Bootstrap/rotation: one PUT per member package. Payloads are
        // opaque ciphertext — forwarded unchanged. First-writer-wins race
        // detection from the Convex version happens client-side already
        // (the caller checks `listMemberPublicKeys` epoch first), so a
        // successful publish reports `created: true`.
        ("groupKeys", "publishPackages") => {
            let conversation_id = arg_str(&args, "conversationId");
            let epoch = arg_f64(&args, "epoch").unwrap_or(1.0);
            let packages = match args.get("packages") {
                Some(Value::Array(list)) => list.clone(),
                _ => Vec::new(),
            };
            if packages.is_empty() {
                return Ok(FunctionResult::ErrorMessage(
                    "No key packages".to_string(),
                ));
            }
            for pkg in &packages {
                let Value::Object(p) = pkg else { continue };
                let body = json!({
                    "epoch": epoch as i64,
                    "userId": obj_get_str(p, "userId"),
                    "sealedKey": obj_get_str(p, "sealedKey"),
                    "ephPublicKey": obj_get_str(p, "ephPublicKey"),
                });
                match c
                    .rest(
                        Method::PUT,
                        &format!("/conversations/{conversation_id}/key-packages"),
                        Some(body),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(err) => return human_or(err),
                }
            }
            let mut obj = BTreeMap::new();
            obj.insert("epoch".to_string(), Value::Float64(epoch));
            obj.insert("created".to_string(), Value::Boolean(true));
            Ok(FunctionResult::Value(Value::Object(obj)))
        }
        // Re-seal for late joiners at the CURRENT epoch (no rotation). The
        // result is ignored by the call site; best-effort by design.
        ("groupKeys", "shareWithMembers") => {
            let conversation_id = arg_str(&args, "conversationId");
            let conv_resp = c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}"),
                    None,
                )
                .await?;
            let conv = conv_resp.get("conversation").unwrap_or(&conv_resp);
            let epoch = jnum_any(conv, &["key_epoch", "keyEpoch"]);
            if epoch < 1.0 {
                return Ok(FunctionResult::ErrorMessage(
                    "No group key yet — bootstrap first".to_string(),
                ));
            }
            let packages = match args.get("packages") {
                Some(Value::Array(list)) => list.clone(),
                _ => Vec::new(),
            };
            let mut shared = 0i64;
            for pkg in &packages {
                let Value::Object(p) = pkg else { continue };
                let body = json!({
                    "epoch": epoch as i64,
                    "userId": obj_get_str(p, "userId"),
                    "sealedKey": obj_get_str(p, "sealedKey"),
                    "ephPublicKey": obj_get_str(p, "ephPublicKey"),
                });
                if c
                    .rest(
                        Method::PUT,
                        &format!("/conversations/{conversation_id}/key-packages"),
                        Some(body),
                    )
                    .await
                    .is_ok()
                {
                    shared += 1;
                }
            }
            let mut obj = BTreeMap::new();
            obj.insert("shared".to_string(), Value::Float64(shared as f64));
            obj.insert("epoch".to_string(), Value::Float64(epoch));
            Ok(FunctionResult::Value(Value::Object(obj)))
        }

        _ => Err(ApiError(format!("unmapped path {module}:{name}"))),
    }
}

/// `profile:getProfile` — the new API has no `GET /users/:id`, so the
/// profile card is composed from the endpoints that DO carry user data.
/// Whatever can't be sourced degrades to empty-but-correct fields.
async fn get_profile(
    c: &ApiClient,
    args: &BTreeMap<String, Value>,
) -> Result<FunctionResult, ApiError> {
    let user_id = arg_str(args, "userId");

    // My own record doubles as the "self" source and tells us my role
    // (for canSupportDm).
    let me_resp = c.rest(Method::GET, "/users/me", None).await?;
    let null = serde_json::Value::Null;
    let me = me_resp.get("user").unwrap_or(&null);
    let my_id = jstr(me, "id");
    let my_staff = is_staff_role(&jstr(me, "role"));

    // Start from a fully-degraded card and fill in what we find.
    let mut username = String::new();
    let mut display_name = String::new();
    let mut avatar_color = String::new();
    let mut avatar_key = String::new();
    let mut banner_key = String::new();
    let mut status_message = String::new();
    let mut bio = String::new();
    let mut relation = "none".to_string();
    let mut request_id = String::new();
    let mut is_friend = false;
    let mut favorite = false;
    let mut nickname = String::new();
    let mut private_note = String::new();
    let mut user_staff = false;

    if user_id == my_id && !user_id.is_empty() {
        relation = "self".to_string();
        username = jstr(me, "username");
        display_name = jstr(me, "display_name");
        avatar_color = jstr(me, "avatar_color");
        avatar_key = jstr(me, "avatar_storage_key");
        banner_key = jstr(me, "profile_banner_storage_key");
        status_message = jstr(me, "status_message");
        bio = jstr(me, "bio");
        user_staff = my_staff;
    } else {
        // Friends carry full user records + my friend_meta.
        if let Ok(fr) = c.rest(Method::GET, "/friends", None).await {
            let list = fr
                .get("friends")
                .and_then(|f| f.as_array())
                .or_else(|| fr.as_array());
            if let Some(list) = list {
                for entry in list {
                    let user = entry
                        .get("user")
                        .or_else(|| entry.get("friend"))
                        .unwrap_or(entry);
                    if jstr_any(user, &["id", "user_id", "userId"]) != user_id {
                        continue;
                    }
                    username = jstr(user, "username");
                    display_name = jstr(user, "display_name");
                    avatar_color = jstr(user, "avatar_color");
                    avatar_key = jstr(user, "avatar_storage_key");
                    banner_key = jstr(user, "profile_banner_storage_key");
                    status_message = jstr(user, "status_message");
                    bio = jstr(user, "bio");
                    user_staff = is_staff_role(&jstr(user, "role"));
                    is_friend = true;
                    relation = "friends".to_string();
                    let meta = entry
                        .get("meta")
                        .or_else(|| entry.get("friend_meta"))
                        .unwrap_or(entry);
                    favorite = jbool_any(meta, &["favorite"], false);
                    nickname = jstr_any(meta, &["nickname"]);
                    private_note = jstr_any(meta, &["private_note", "privateNote"]);
                    break;
                }
            }
        }
        // Pending requests decide incoming/outgoing + requestId.
        if relation != "friends" {
            if let Ok(rq) = c.rest(Method::GET, "/friends/requests", None).await
            {
                if let Some(outgoing) =
                    rq.get("outgoing").and_then(|o| o.as_array())
                {
                    for entry in outgoing {
                        if request_user_matches(entry, &user_id) {
                            relation = "outgoing".to_string();
                            request_id = jstr(entry, "id");
                            break;
                        }
                    }
                }
                if relation != "outgoing" {
                    if let Some(incoming) =
                        rq.get("incoming").and_then(|i| i.as_array())
                    {
                        for entry in incoming {
                            if request_user_matches(entry, &user_id) {
                                relation = "incoming".to_string();
                                request_id = jstr(entry, "id");
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Presence is best-effort — the server already hides offline status for
    // `hide_online_status` users, so whatever it says is display-ready.
    let mut last_seen_at: i64 = 0;
    let mut presence = "offline".to_string();
    if relation == "self" {
        let preferred = jstr(me, "presence_status");
        presence = if preferred.is_empty() || preferred == "invisible" {
            "online".to_string()
        } else {
            preferred
        };
    } else if let Ok(p) = c
        .rest(Method::GET, &format!("/presence/{user_id}"), None)
        .await
    {
        last_seen_at = jnum_any(&p, &["last_seen_at", "lastSeenAt"]) as i64;
        let status = jstr_any(&p, &["status", "presence_status", "presenceStatus"]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let online = last_seen_at > 0 && now - last_seen_at < ONLINE_MS;
        presence = if !status.is_empty() && status != "invisible" {
            if status == "idle" || status == "dnd" {
                status
            } else if online || status == "online" {
                "online".to_string()
            } else {
                "offline".to_string()
            }
        } else if online {
            "online".to_string()
        } else {
            "offline".to_string()
        };
    }

    let mut obj = BTreeMap::new();
    obj.insert("userId".to_string(), Value::String(user_id));
    obj.insert("username".to_string(), Value::String(username));
    obj.insert("displayName".to_string(), Value::String(display_name));
    obj.insert("avatarColor".to_string(), Value::String(avatar_color));
    obj.insert(
        "avatarImageUrl".to_string(),
        Value::String(if avatar_key.is_empty() {
            String::new()
        } else {
            c.file_url(&avatar_key)
        }),
    );
    obj.insert(
        "profileBannerUrl".to_string(),
        Value::String(if banner_key.is_empty() {
            String::new()
        } else {
            c.file_url(&banner_key)
        }),
    );
    obj.insert(
        "statusMessage".to_string(),
        Value::String(status_message),
    );
    obj.insert("bio".to_string(), Value::String(bio));
    obj.insert(
        "lastSeenAt".to_string(),
        Value::Float64(last_seen_at as f64),
    );
    obj.insert("presence".to_string(), Value::String(presence));
    obj.insert("isStaff".to_string(), Value::Boolean(user_staff));
    obj.insert("isFriend".to_string(), Value::Boolean(is_friend));
    obj.insert(
        "canSupportDm".to_string(),
        Value::Boolean(my_staff || user_staff),
    );
    obj.insert("relation".to_string(), Value::String(relation));
    obj.insert("requestId".to_string(), Value::String(request_id));
    // No per-user mutual-server endpoint — degrade to an empty list.
    obj.insert("mutualServers".to_string(), Value::Array(Vec::new()));
    obj.insert("favorite".to_string(), Value::Boolean(favorite));
    obj.insert("nickname".to_string(), Value::String(nickname));
    obj.insert("privateNote".to_string(), Value::String(private_note));
    obj.insert("plusActive".to_string(), Value::Boolean(false));
    Ok(FunctionResult::Value(Value::Object(obj)))
}

/// A friend-request entry "matches" when whichever embedded user object it
/// carries has the target id (shapes differ between incoming/outgoing).
fn request_user_matches(entry: &serde_json::Value, user_id: &str) -> bool {
    for key in ["user", "from_user", "to_user", "fromUser", "toUser"] {
        if let Some(u) = entry.get(key) {
            if jstr_any(u, &["id", "user_id", "userId"]) == user_id {
                return true;
            }
        }
    }
    // Flat shapes: from_user_id / to_user_id directly on the entry.
    jstr_any(entry, &["from_user_id", "to_user_id", "user_id"]) == user_id
}

fn is_staff_role(role: &str) -> bool {
    matches!(role, "owner" | "admin")
}

/// Extract the newest key package from a `GET .../key-packages` response,
/// tolerating both a single object and list shapes, snake_case or
/// camelCase field names.
fn newest_package(resp: &serde_json::Value) -> Option<(i64, String, String)> {
    let mut best: Option<(i64, String, String)> = None;
    let mut consider = |p: &serde_json::Value| {
        let epoch = jnum_any(p, &["epoch"]) as i64;
        let sealed = jstr_any(p, &["sealed_key", "sealedKey"]);
        let eph = jstr_any(p, &["eph_public_key", "ephPublicKey"]);
        if sealed.is_empty() || eph.is_empty() {
            return;
        }
        if best.as_ref().map(|(e, _, _)| epoch > *e).unwrap_or(true) {
            best = Some((epoch, sealed, eph));
        }
    };
    if let Some(list) = resp
        .get("packages")
        .and_then(|p| p.as_array())
        .or_else(|| resp.as_array())
    {
        for p in list {
            consider(p);
        }
    } else if resp.is_object() && resp.get("packages").is_none() {
        consider(resp);
    }
    best
}

fn search_user_object(c: &ApiClient, user: &serde_json::Value) -> Value {
    let mut obj = BTreeMap::new();
    obj.insert(
        "userId".to_string(),
        Value::String(jstr_any(user, &["id", "user_id", "userId"])),
    );
    obj.insert(
        "username".to_string(),
        Value::String(jstr(user, "username")),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr(user, "display_name")),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr(user, "avatar_color")),
    );
    let key = jstr(user, "avatar_storage_key");
    obj.insert(
        "avatarImageUrl".to_string(),
        Value::String(if key.is_empty() {
            String::new()
        } else {
            c.file_url(&key)
        }),
    );
    Value::Object(obj)
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

fn arg_f64(args: &BTreeMap<String, Value>, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Int64(i)) => Some(*i as f64),
        _ => None,
    }
}

fn obj_get_str(obj: &BTreeMap<String, Value>, key: &str) -> String {
    match obj.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
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
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn jnum_any(v: &serde_json::Value, keys: &[&str]) -> f64 {
    for key in keys {
        if let Some(n) = v.get(key).and_then(|x| x.as_f64()) {
            return n;
        }
    }
    0.0
}

fn jbool_any(v: &serde_json::Value, keys: &[&str], default: bool) -> bool {
    for key in keys {
        if let Some(b) = v.get(key).and_then(|x| x.as_bool()) {
            return b;
        }
    }
    default
}

/// Percent-encode a query value (unreserved characters pass through).
fn urlencode(input: &str) -> String {
    let mut out = String::new();
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

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// mutations surfaced 4xx bodies as user-facing messages (profile
/// validation, "Invalid public key", ...), so downgrade those to
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
