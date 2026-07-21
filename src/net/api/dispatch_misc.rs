//! `admin:*`, `reports:*`, `bots:*`, `plus:*` dispatch against api.vyrapp.pro.
//!
//! Admin, reports (file + list + resolve), bots, and plus status are live REST.
//! Remaining gap: plus billing portal.

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
        // ---------- admin ----------
        // GET /admin/users → { users: [...] }
        // Shape expected by `admin_users_subscription` / `parse_object_array`:
        // {userId, username, displayName, role, banned, banExpiresAt}.
        ("admin", "listUsers") => {
            // Prefer a higher limit so the panel isn't stuck with a tiny seed page.
            let resp = match c
                .rest(Method::GET, "/admin/users?limit=500", None)
                .await
            {
                Ok(resp) => resp,
                Err(_) => match c.rest(Method::GET, "/admin/users", None).await {
                    Ok(resp) => resp,
                    Err(err) => return human_or(err),
                },
            };
            let rows = list_from(&resp, &["users", "items", "data"]);
            ok(Value::Array(
                rows.iter()
                    .filter(|row| !is_seed_or_bot_user(row))
                    .map(|row| admin_user_row(row))
                    .collect(),
            ))
        }
        // GET /admin/stats → snake_case counters → camelCase for parse_admin_stats.
        ("admin", "adminStats") => {
            let resp = match c.rest(Method::GET, "/admin/stats", None).await {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            // Some builds nest counters under `stats`.
            let src = resp
                .get("stats")
                .filter(|s| s.is_object())
                .unwrap_or(&resp);
            ok(Value::Object(BTreeMap::from([
                (
                    "totalUsers".to_string(),
                    Value::Float64(jf64(
                        src,
                        &["total_users", "totalUsers", "users", "user_count"],
                    )),
                ),
                (
                    "online".to_string(),
                    Value::Float64(jf64(src, &["online", "online_users", "onlineUsers"])),
                ),
                (
                    "banned".to_string(),
                    Value::Float64(jf64(src, &["banned", "banned_users", "bannedUsers"])),
                ),
                (
                    "staff".to_string(),
                    Value::Float64(jf64(src, &["staff", "staff_count", "staffCount"])),
                ),
                (
                    "bots".to_string(),
                    Value::Float64(jf64(src, &["bots", "bot_count", "botCount"])),
                ),
                (
                    "servers".to_string(),
                    Value::Float64(jf64(src, &["servers", "server_count", "serverCount"])),
                ),
            ])))
        }
        // GET /admin/users/:id → { user: {...} } for the per-user drawer.
        ("admin", "adminUserDetail") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            let resp = match c
                .rest(Method::GET, &format!("/admin/users/{user_id}"), None)
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            let user = resp
                .get("user")
                .filter(|u| u.is_object())
                .unwrap_or(&resp);
            ok(admin_user_detail(c, user))
        }
        // PATCH /admin/users/:id/role  { role }
        ("admin", "setRole") => {
            let user_id = arg_str(&args, "userId");
            let role = arg_str(&args, "role");
            if user_id.is_empty() || role.is_empty() {
                return err("User id and role required");
            }
            match c
                .rest(
                    Method::PATCH,
                    &format!("/admin/users/{user_id}/role"),
                    Some(json!({ "role": role })),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // POST /admin/users/:id/ban  { permanent | duration_ms | expires_at }
        // DELETE /admin/users/:id/ban  (unban; IP lifts are server-side)
        ("admin", "setBanned") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            let banned = arg_bool(&args, "banned").unwrap_or(true);
            if !banned {
                return match c
                    .rest(Method::DELETE, &format!("/admin/users/{user_id}/ban"), None)
                    .await
                {
                    Ok(_) => ok_null(),
                    Err(err) => human_or(err),
                };
            }
            // Temporary ban: durationHours / durationMs / expiresAt (ms).
            // Permanent: permanent:true or no duration fields.
            let duration_hours = arg_f64(&args, "durationHours");
            let duration_ms = arg_f64(&args, "durationMs").or_else(|| {
                duration_hours.map(|h| (h.max(0.0) * 3_600_000.0).round())
            });
            let expires_at = arg_f64(&args, "expiresAt").or_else(|| {
                duration_ms.map(|ms| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);
                    now + ms
                })
            });
            let permanent = arg_bool(&args, "permanent").unwrap_or(duration_ms.is_none());
            let mut body = serde_json::Map::new();
            if permanent || duration_ms.is_none() {
                body.insert("permanent".into(), json!(true));
            } else {
                body.insert("permanent".into(), json!(false));
                if let Some(ms) = duration_ms {
                    body.insert("duration_ms".into(), json!(ms as i64));
                    body.insert("durationMs".into(), json!(ms as i64));
                }
                if let Some(h) = duration_hours {
                    body.insert("duration_hours".into(), json!(h as i64));
                    body.insert("durationHours".into(), json!(h as i64));
                }
                if let Some(exp) = expires_at {
                    body.insert("expires_at".into(), json!(exp as i64));
                    body.insert("expiresAt".into(), json!(exp as i64));
                    body.insert("ban_expires_at".into(), json!(exp as i64));
                }
            }
            if let Some(reason) = arg_opt_str(&args, "reason") {
                if !reason.is_empty() {
                    body.insert("reason".into(), json!(reason));
                }
            }
            match c
                .rest(
                    Method::POST,
                    &format!("/admin/users/{user_id}/ban"),
                    Some(json!(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // POST /admin/users/:id/mute  { duration_ms, expires_at, reason } — always temporary in UI.
        // DELETE /admin/users/:id/mute
        ("admin", "setMuted") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            let muted = arg_bool(&args, "muted").unwrap_or(true);
            if !muted {
                return match c
                    .rest(Method::DELETE, &format!("/admin/users/{user_id}/mute"), None)
                    .await
                {
                    Ok(_) => ok_null(),
                    Err(err) => human_or(err),
                };
            }
            let hours = arg_f64(&args, "durationHours").unwrap_or(1.0).max(1.0);
            let duration_ms = (hours * 3_600_000.0).round() as i64;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let expires_at = now + duration_ms;
            let mut body = serde_json::Map::new();
            body.insert("permanent".into(), json!(false));
            body.insert("duration_ms".into(), json!(duration_ms));
            body.insert("durationMs".into(), json!(duration_ms));
            body.insert("duration_hours".into(), json!(hours as i64));
            body.insert("durationHours".into(), json!(hours as i64));
            body.insert("expires_at".into(), json!(expires_at));
            body.insert("expiresAt".into(), json!(expires_at));
            body.insert("mute_expires_at".into(), json!(expires_at));
            if let Some(reason) = arg_opt_str(&args, "reason") {
                if !reason.is_empty() {
                    body.insert("reason".into(), json!(reason));
                }
            }
            match c
                .rest(
                    Method::POST,
                    &format!("/admin/users/{user_id}/mute"),
                    Some(json!(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // DELETE /admin/users/:id/sessions
        ("admin", "adminRevokeSessions") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            match c
                .rest(
                    Method::DELETE,
                    &format!("/admin/users/{user_id}/sessions"),
                    None,
                )
                .await
            {
                Ok(resp) => {
                    // Call sites that expect a count get a number; expect_null
                    // also accepts any Value.
                    let n = jf64(&resp, &["revoked", "count", "sessions"]);
                    if n > 0.0 {
                        ok(Value::Float64(n))
                    } else {
                        ok_null()
                    }
                }
                Err(err) => human_or(err),
            }
        }

        // ---------- reports ----------
        // GET /admin/reports → { reports: [...] }
        ("reports", "adminListReports") => {
            let mut path = "/admin/reports".to_string();
            if let Some(status) = arg_opt_str(&args, "status") {
                if !status.is_empty() {
                    path.push_str(&format!("?status={status}"));
                }
            }
            let resp = match c.rest(Method::GET, &path, None).await {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            let rows = list_from(&resp, &["reports", "items", "data"]);
            ok(Value::Array(
                rows.iter().map(|row| report_row(row)).collect(),
            ))
        }
        // POST /admin/reports/:id/resolve  { action: dismiss|delete_message|ban_author }
        // UI still passes legacy status strings ("dismissed" / "actioned") —
        // map them to the API action vocabulary.
        ("reports", "adminResolveReport") => {
            let report_id = arg_str(&args, "reportId");
            let status_or_action = arg_str(&args, "status");
            let action = arg_opt_str(&args, "action").unwrap_or_default();
            let action = if !action.is_empty() {
                action
            } else {
                match status_or_action.as_str() {
                    "dismissed" | "dismiss" => "dismiss".to_string(),
                    "delete_message" | "delete" | "actioned" => "delete_message".to_string(),
                    "ban_author" | "ban" => "ban_author".to_string(),
                    other if !other.is_empty() => other.to_string(),
                    _ => String::new(),
                }
            };
            if report_id.is_empty() || action.is_empty() {
                return err("Report id and action required");
            }
            let mut body = serde_json::Map::new();
            body.insert("action".into(), json!(action));
            if let Some(note) = arg_opt_str(&args, "reviewNote") {
                if !note.is_empty() {
                    body.insert("review_note".into(), json!(note));
                }
            }
            match c
                .rest(
                    Method::POST,
                    &format!("/admin/reports/{report_id}/resolve"),
                    Some(json!(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // POST /reports  { messageId|message_id, reason, messageBody? }
        ("reports", "reportMessage") => {
            let message_id = arg_str(&args, "messageId");
            let reason = arg_str(&args, "reason");
            if message_id.is_empty() || reason.is_empty() {
                return err("Message and reason required");
            }
            let body = json!({
                "messageId": message_id,
                "reason": reason,
                "messageBody": arg_str(&args, "messageBody"),
            });
            match c.rest(Method::POST, "/reports", Some(body)).await {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // ---------- bots ----------
        // GET /bots/mine → { bots: [{id, username, display_name, avatar_color, ...}] }
        ("bots", "listMine") => {
            let resp = match c.rest(Method::GET, "/bots/mine", None).await {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            let rows = list_from(&resp, &["bots", "items", "data"]);
            ok(Value::Array(rows.iter().map(|row| bot_row(row)).collect()))
        }
        // POST /bots { username, display_name } → { id, token }
        ("bots", "create") => {
            let name = arg_str(&args, "name");
            let username = bot_username_from_name(&name);
            if username.len() < 2 {
                return err("Bot needs a name (min 2 chars)");
            }
            let body = json!({
                "username": username,
                "display_name": name,
            });
            match c.rest(Method::POST, "/bots", Some(body)).await {
                Ok(resp) => {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "botId".to_string(),
                        Value::String(jstr(&resp, &["id", "bot_id", "botId"])),
                    );
                    obj.insert(
                        "token".to_string(),
                        Value::String(jstr(&resp, &["token"])),
                    );
                    let uname = jstr(&resp, &["username"]);
                    obj.insert(
                        "username".to_string(),
                        Value::String(if uname.is_empty() {
                            username
                        } else {
                            uname
                        }),
                    );
                    obj.insert("displayName".to_string(), Value::String(name));
                    ok(Value::Object(obj))
                }
                Err(err) => human_or(err),
            }
        }
        // POST /bots/:id/invite { serverId } — resolve username via /bots/mine first.
        ("bots", "inviteToServer") => {
            let server_id = arg_str(&args, "serverId");
            let bot_username = arg_str(&args, "botUsername");
            if server_id.is_empty() || bot_username.is_empty() {
                return err("Server and bot username required");
            }
            let bot_id = match resolve_bot_id(c, &bot_username).await {
                Ok(id) => id,
                Err(msg) => return err(&msg),
            };
            match c
                .rest(
                    Method::POST,
                    &format!("/bots/{bot_id}/invite"),
                    Some(json!({ "serverId": server_id })),
                )
                .await
            {
                Ok(_) => ok(Value::String(bot_id)),
                Err(err) => human_or(err),
            }
        }
        // POST /bots/:id/regenerate-token → { token }
        ("bots", "regenerateToken") => {
            let bot_id = arg_str(&args, "botId");
            if bot_id.is_empty() {
                return err("Bot id required");
            }
            match c
                .rest(
                    Method::POST,
                    &format!("/bots/{bot_id}/regenerate-token"),
                    None,
                )
                .await
            {
                Ok(resp) => {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "token".to_string(),
                        Value::String(jstr(&resp, &["token"])),
                    );
                    ok(Value::Object(obj))
                }
                Err(err) => human_or(err),
            }
        }
        // DELETE /bots/:id
        ("bots", "destroy") => {
            let bot_id = arg_str(&args, "botId");
            if bot_id.is_empty() {
                return err("Bot id required");
            }
            match c
                .rest(Method::DELETE, &format!("/bots/{bot_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // ---------- plus ----------
        // GET /plus/status → { active, expires_at }
        ("plus", "getMyStatus") => {
            let resp = match c.rest(Method::GET, "/plus/status", None).await {
                Ok(resp) => resp,
                // Soft-fail to "no Plus" so settings still open if the route flaps.
                Err(_) => {
                    return ok(Value::Object(BTreeMap::from([
                        ("active".to_string(), Value::Boolean(false)),
                        ("expiresAt".to_string(), Value::Float64(0.0)),
                    ])));
                }
            };
            let expires = jf64(&resp, &["expires_at", "expiresAt"]);
            let active = jbool(&resp, "active", false) || expires > 0.0;
            ok(Value::Object(BTreeMap::from([
                ("active".to_string(), Value::Boolean(active)),
                ("expiresAt".to_string(), Value::Float64(expires)),
            ])))
        }
        ("plus", "createBillingPortal") => {
            err("Billing portal not available yet — visit https://buy.vyrapp.pro")
        }
        // Staff gift: max 30 days per API policy (1 month).
        // POST /admin/users/:id/plus  { days | duration_days }  (capped client-side)
        ("plus", "adminGrant") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            let days = arg_f64(&args, "days")
                .or_else(|| arg_f64(&args, "durationDays"))
                .unwrap_or(30.0)
                .clamp(1.0, 30.0)
                .round() as i64;
            let body = json!({
                "days": days,
                "duration_days": days,
                "durationDays": days,
            });
            // Try the dedicated plus route first, then a nested admin path.
            let paths = [
                format!("/admin/users/{user_id}/plus"),
                format!("/admin/plus/grant"),
            ];
            let mut last_err = None;
            for (i, path) in paths.iter().enumerate() {
                let payload = if path.ends_with("/grant") {
                    json!({
                        "user_id": user_id,
                        "userId": user_id,
                        "days": days,
                        "duration_days": days,
                    })
                } else {
                    body.clone()
                };
                match c.rest(Method::POST, path, Some(payload)).await {
                    Ok(_) => return ok_null(),
                    Err(err) => {
                        // 404 → try next shape; other errors surface.
                        if err.0.starts_with("404") && i + 1 < paths.len() {
                            last_err = Some(err);
                            continue;
                        }
                        return human_or(err);
                    }
                }
            }
            human_or(last_err.unwrap_or_else(|| ApiError("Plus grant failed".into())))
        }
        // DELETE /admin/users/:id/plus  — revoke gifted/subscription Plus.
        ("plus", "adminRevoke") => {
            let user_id = arg_str(&args, "userId");
            if user_id.is_empty() {
                return err("User id required");
            }
            match c
                .rest(Method::DELETE, &format!("/admin/users/{user_id}/plus"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) if err.0.starts_with("404") => {
                    // Fallback body form.
                    match c
                        .rest(
                            Method::POST,
                            "/admin/plus/revoke",
                            Some(json!({
                                "user_id": user_id,
                                "userId": user_id,
                            })),
                        )
                        .await
                    {
                        Ok(_) => ok_null(),
                        Err(err) => human_or(err),
                    }
                }
                Err(err) => human_or(err),
            }
        }

        // ---------- module catch-alls ----------
        ("admin", _) => err("Admin action not available yet"),
        ("reports", _) => err("Reports not available yet"),
        ("bots", _) => err("Bots not available yet"),
        ("plus", _) => err("Plus not available yet — visit https://buy.vyrapp.pro"),

        _ => Err(ApiError(format!("unmapped path {module}:{name}"))),
    }
}

// ---------- row mappers ----------

fn bot_row(row: &serde_json::Value) -> Value {
    let mut obj = BTreeMap::new();
    obj.insert(
        "botId".to_string(),
        Value::String(jstr(row, &["id", "bot_id", "botId"])),
    );
    obj.insert(
        "username".to_string(),
        Value::String(jstr(row, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr(row, &["display_name", "displayName"])),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr(row, &["avatar_color", "avatarColor"])),
    );
    Value::Object(obj)
}

/// Client sends a free-form display name; API requires a username (2–32).
/// Prefer `bot_<slug>` so bot accounts stay namespaced.
fn bot_username_from_name(name: &str) -> String {
    let raw = name.trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("bot_") {
        return raw
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    ch.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .take(32)
            .collect();
    }
    let mut slug: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let slug = slug.trim_matches('_');
    let mut out = format!("bot_{slug}");
    if out.len() > 32 {
        out.truncate(32);
    }
    out
}

async fn resolve_bot_id(c: &ApiClient, username_or_id: &str) -> Result<String, String> {
    let key = username_or_id.trim();
    if key.is_empty() {
        return Err("Bot username required".into());
    }
    // ULID-ish ids are 26 chars of Crockford base32 — accept any long id as-is.
    if key.len() >= 20 && key.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        // Still prefer a list match when the string is also a username.
        if let Ok(resp) = c.rest(Method::GET, "/bots/mine", None).await {
            let rows = list_from(&resp, &["bots", "items", "data"]);
            for row in rows {
                let id = jstr(row, &["id", "bot_id", "botId"]);
                if id == key {
                    return Ok(id);
                }
            }
        }
        return Ok(key.to_string());
    }
    let resp = c
        .rest(Method::GET, "/bots/mine", None)
        .await
        .map_err(|e| e.0)?;
    let rows = list_from(&resp, &["bots", "items", "data"]);
    let needle = key.to_ascii_lowercase();
    for row in rows {
        let uname = jstr(row, &["username"]);
        if uname.eq_ignore_ascii_case(&needle) {
            let id = jstr(row, &["id", "bot_id", "botId"]);
            if !id.is_empty() {
                return Ok(id);
            }
        }
    }
    Err(format!("Bot @{key} not found in your bots"))
}

/// Drop bots and load-test seed accounts (`xx1784598118@example.com`).
fn is_seed_or_bot_user(row: &serde_json::Value) -> bool {
    if jbool(row, "is_bot", false) || jbool(row, "isBot", false) {
        return true;
    }
    let email = jstr(row, &["email"]).to_ascii_lowercase();
    if email.ends_with("@example.com") || email.ends_with("@example.org") {
        return true;
    }
    let username = jstr(row, &["username"]);
    // Seed script usernames: 1–3 letters + long unix-ish digit run.
    let (letters, digits) = username
        .chars()
        .partition::<String, _>(|c| c.is_ascii_alphabetic());
    if letters.len() <= 3
        && digits.len() >= 10
        && letters.chars().all(|c| c.is_ascii_lowercase())
        && digits.chars().all(|c| c.is_ascii_digit())
        && username.len() == letters.len() + digits.len()
    {
        return true;
    }
    let id = jstr(row, &["id", "user_id", "userId"]);
    id.is_empty() || username.is_empty()
}

fn admin_user_row(row: &serde_json::Value) -> Value {
    let mut obj = BTreeMap::new();
    obj.insert(
        "userId".to_string(),
        Value::String(jstr(row, &["id", "user_id", "userId"])),
    );
    obj.insert(
        "username".to_string(),
        Value::String(jstr(row, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr(
            row,
            &["display_name", "displayName", "username"],
        )),
    );
    obj.insert(
        "role".to_string(),
        Value::String({
            let r = jstr(row, &["role"]);
            if r.is_empty() {
                "user".into()
            } else {
                r
            }
        }),
    );
    obj.insert(
        "banned".to_string(),
        Value::Boolean(jbool(row, "banned", false)),
    );
    // Temporary ban expiry (ms). Missing / null → 0 (treated as permanent).
    obj.insert(
        "banExpiresAt".to_string(),
        Value::Float64(jf64(
            row,
            &[
                "ban_expires_at",
                "banExpiresAt",
                "banned_until",
                "bannedUntil",
            ],
        )),
    );
    obj.insert(
        "muted".to_string(),
        Value::Boolean(jbool(row, "muted", false)),
    );
    obj.insert(
        "muteExpiresAt".to_string(),
        Value::Float64(jf64(
            row,
            &["mute_expires_at", "muteExpiresAt", "muted_until", "mutedUntil"],
        )),
    );
    let plus_exp = jf64(
        row,
        &["plus_expires_at", "plusExpiresAt", "plus_expires"],
    );
    let plus_active = jbool(row, "plus_active", false)
        || jbool(row, "plusActive", false)
        || plus_exp > chrono_now_ms();
    obj.insert("plusActive".to_string(), Value::Boolean(plus_active));
    obj.insert("plusExpiresAt".to_string(), Value::Float64(plus_exp));
    Value::Object(obj)
}

fn chrono_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn admin_user_detail(c: &ApiClient, user: &serde_json::Value) -> Value {
    let presence = jstr(user, &["presence_status", "presence", "status"]);
    let last_seen = jf64(user, &["last_seen_at", "lastSeenAt"]);
    let online = presence == "online" || presence == "Online";

    let avatar_key = jstr(user, &["avatar_storage_key", "avatarStorageKey"]);
    let avatar_url = if !avatar_key.is_empty() {
        c.file_url(&avatar_key)
    } else {
        jstr(user, &["avatar_url", "avatar_image_url", "avatarImageUrl"])
    };

    // API exposes counts, not names — keep the array empty rather than inventing labels.
    let server_names = match user.get("server_names").or_else(|| user.get("serverNames")) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
            .collect(),
        _ => Vec::new(),
    };

    let mut obj = BTreeMap::new();
    obj.insert(
        "userId".to_string(),
        Value::String(jstr(user, &["id", "user_id", "userId"])),
    );
    obj.insert(
        "username".to_string(),
        Value::String(jstr(user, &["username"])),
    );
    obj.insert(
        "displayName".to_string(),
        Value::String(jstr(user, &["display_name", "displayName"])),
    );
    obj.insert("role".to_string(), Value::String(jstr(user, &["role"])));
    obj.insert(
        "banned".to_string(),
        Value::Boolean(jbool(user, "banned", false)),
    );
    obj.insert(
        "banExpiresAt".to_string(),
        Value::Float64(jf64(
            user,
            &[
                "ban_expires_at",
                "banExpiresAt",
                "banned_until",
                "bannedUntil",
            ],
        )),
    );
    obj.insert(
        "muted".to_string(),
        Value::Boolean(jbool(user, "muted", false)),
    );
    obj.insert(
        "muteExpiresAt".to_string(),
        Value::Float64(jf64(
            user,
            &["mute_expires_at", "muteExpiresAt", "muted_until", "mutedUntil"],
        )),
    );
    obj.insert(
        "isBot".to_string(),
        Value::Boolean(jbool(user, "is_bot", false) || jbool(user, "isBot", false)),
    );
    obj.insert("bio".to_string(), Value::String(jstr(user, &["bio"])));
    obj.insert(
        "statusMessage".to_string(),
        Value::String(jstr(user, &["status_message", "statusMessage"])),
    );
    obj.insert(
        "avatarColor".to_string(),
        Value::String(jstr(user, &["avatar_color", "avatarColor"])),
    );
    obj.insert("avatarImageUrl".to_string(), Value::String(avatar_url));
    obj.insert(
        "createdAt".to_string(),
        Value::Float64(jf64(user, &["created_at", "createdAt"])),
    );
    obj.insert("online".to_string(), Value::Boolean(online));
    obj.insert("lastSeenAt".to_string(), Value::Float64(last_seen));
    obj.insert("serverNames".to_string(), Value::Array(server_names));
    obj.insert(
        "friendCount".to_string(),
        Value::Float64(jf64(user, &["friends_count", "friend_count", "friendCount"])),
    );
    Value::Object(obj)
}

fn report_row(row: &serde_json::Value) -> Value {
    // Nested user objects: { reporter: {username}, author: {username} }
    let reporter = row
        .get("reporter")
        .filter(|x| x.is_object())
        .unwrap_or(row);
    let author = row
        .get("author")
        .or_else(|| row.get("message_author"))
        .filter(|x| x.is_object())
        .unwrap_or(row);
    let conversation = row
        .get("conversation")
        .filter(|x| x.is_object())
        .unwrap_or(row);
    let message = row
        .get("message")
        .filter(|x| x.is_object())
        .unwrap_or(row);

    let mut obj = BTreeMap::new();
    obj.insert(
        "reportId".to_string(),
        Value::String(jstr(row, &["id", "report_id", "reportId"])),
    );
    obj.insert(
        "messageId".to_string(),
        Value::String({
            let id = jstr(row, &["message_id", "messageId"]);
            if id.is_empty() {
                jstr(message, &["id", "message_id", "messageId"])
            } else {
                id
            }
        }),
    );
    obj.insert(
        "conversationLabel".to_string(),
        Value::String({
            let label = jstr(
                row,
                &[
                    "conversation_label",
                    "conversationLabel",
                    "conversation_name",
                ],
            );
            if !label.is_empty() {
                label
            } else {
                let name = jstr(conversation, &["name", "title", "label"]);
                if name.is_empty() {
                    let kind = jstr(conversation, &["kind"]);
                    if kind == "direct" {
                        "Direct message".into()
                    } else {
                        "Conversation".into()
                    }
                } else {
                    name
                }
            }
        }),
    );
    obj.insert(
        "reporterUsername".to_string(),
        Value::String({
            let u = jstr(
                row,
                &["reporter_username", "reporterUsername", "reporter"],
            );
            if u.is_empty() {
                jstr(reporter, &["username", "display_name", "displayName"])
            } else {
                u
            }
        }),
    );
    obj.insert(
        "authorUsername".to_string(),
        Value::String({
            let u = jstr(row, &["author_username", "authorUsername", "author"]);
            if u.is_empty() {
                jstr(author, &["username", "display_name", "displayName"])
            } else {
                u
            }
        }),
    );
    obj.insert(
        "messageBody".to_string(),
        Value::String({
            let body = jstr(
                row,
                &[
                    "message_body",
                    "messageBody",
                    "message_body_snapshot",
                    "body_snapshot",
                    "body",
                ],
            );
            if body.is_empty() {
                jstr(message, &["body", "snippet", "content"])
            } else {
                body
            }
        }),
    );
    obj.insert(
        "reason".to_string(),
        Value::String(jstr(row, &["reason"])),
    );
    obj.insert(
        "status".to_string(),
        Value::String({
            let s = jstr(row, &["status"]);
            if s.is_empty() {
                "pending".into()
            } else {
                s
            }
        }),
    );
    obj.insert(
        "createdAt".to_string(),
        Value::Float64(jf64(row, &["created_at", "createdAt"])),
    );
    Value::Object(obj)
}

// ---------- helpers ----------

fn ok(value: Value) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(value))
}

fn ok_null() -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(Value::Null))
}

fn err(msg: &str) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::ErrorMessage(msg.to_string()))
}

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

fn arg_f64(args: &BTreeMap<String, Value>, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Int64(i)) => Some(*i as f64),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn list_from<'a>(resp: &'a serde_json::Value, keys: &[&str]) -> Vec<&'a serde_json::Value> {
    if let Some(arr) = resp.as_array() {
        return arr.iter().collect();
    }
    for key in keys {
        if let Some(arr) = resp.get(*key).and_then(|v| v.as_array()) {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

fn jstr(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn jbool(v: &serde_json::Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

fn jf64(v: &serde_json::Value, keys: &[&str]) -> f64 {
    for key in keys {
        if let Some(x) = v.get(*key) {
            if let Some(n) = x
                .as_f64()
                .or_else(|| x.as_i64().map(|n| n as f64))
                .or_else(|| x.as_u64().map(|n| n as f64))
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            {
                if n > 0.0 && n < 1e11 {
                    return (n * 1000.0).round();
                }
                return n;
            }
        }
    }
    0.0
}
