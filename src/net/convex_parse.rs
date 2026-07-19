//! Turning raw Convex `FunctionResult`/`Value` payloads into typed Rust:
//! the small `obj_*` field-accessors used throughout the subscription
//! parsers, `expect_null`/`expect_string` for plain mutation results, and
//! the handful of one-off response shapes (session/profile/clear-chat)
//! that need more than a single field pulled out.

use std::collections::BTreeMap;

use convex::{FunctionResult, Value};

use crate::state::types::{AdminStats, AdminUserDetail, ProfileView, ServerStats, Session};

// ---------- Convex parsing helpers ----------

pub(crate) fn obj_str(obj: &BTreeMap<String, Value>, key: &str) -> String {
    match obj.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

pub(crate) fn obj_f64(obj: &BTreeMap<String, Value>, key: &str) -> f64 {
    match obj.get(key) {
        Some(Value::Float64(f)) => *f,
        Some(Value::Int64(i)) => *i as f64,
        _ => 0.0,
    }
}

/// Millisecond timestamps arrive from Convex as JSON numbers; convert to
/// integer ms-since-epoch once, here at the parse boundary, so the rest of
/// the app only ever threads `i64` timestamps around.
pub(super) fn obj_ms(obj: &BTreeMap<String, Value>, key: &str) -> i64 {
    match obj.get(key) {
        Some(Value::Float64(f)) => *f as i64,
        Some(Value::Int64(i)) => *i,
        _ => 0,
    }
}

pub(super) fn obj_bool(obj: &BTreeMap<String, Value>, key: &str) -> bool {
    matches!(obj.get(key), Some(Value::Boolean(true)))
}

pub(super) fn obj_opt_str(obj: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

pub(super) fn obj_object_array(
    obj: &BTreeMap<String, Value>,
    key: &str,
) -> Vec<BTreeMap<String, Value>> {
    match obj.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Object(o) => Some(o.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn obj_object(
    obj: &BTreeMap<String, Value>,
    key: &str,
) -> Option<BTreeMap<String, Value>> {
    match obj.get(key) {
        Some(Value::Object(o)) => Some(o.clone()),
        _ => None,
    }
}

pub(crate) fn obj_str_list(obj: &BTreeMap<String, Value>, key: &str) -> Vec<String> {
    match obj.get(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn expect_null(result: FunctionResult) -> Result<(), String> {
    match result {
        FunctionResult::Value(_) => Ok(()),
        FunctionResult::ErrorMessage(err) => Err(err),
        FunctionResult::ConvexError(err) => Err(format!("{err:?}")),
    }
}

pub(crate) fn expect_string(result: FunctionResult) -> Result<String, String> {
    match result {
        FunctionResult::Value(Value::String(s)) => Ok(s),
        FunctionResult::Value(_) => Err("Unexpected server response".to_string()),
        FunctionResult::ErrorMessage(err) => Err(err),
        FunctionResult::ConvexError(err) => Err(format!("{err:?}")),
    }
}

fn parse_platform_role(obj: &BTreeMap<String, Value>) -> String {
    let r = obj_str(obj, "role");
    match r.as_str() {
        "owner" | "admin" | "moderator" => r,
        _ => "user".into(),
    }
}

pub(crate) fn parse_session(result: FunctionResult) -> Result<Session, String> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => {
            let platform_role = parse_platform_role(&obj);
            Ok(Session {
                token: obj_str(&obj, "token"),
                user_id: obj_str(&obj, "userId"),
                username: obj_str(&obj, "username"),
                display_name: obj_str(&obj, "displayName"),
                is_admin: platform_role == "admin" || platform_role == "owner",
                is_moderator: matches!(platform_role.as_str(), "moderator" | "admin" | "owner"),
                platform_role,
                avatar_color: obj_str(&obj, "avatarColor"),
                status_message: obj_str(&obj, "statusMessage"),
                bio: obj_str(&obj, "bio"),
                avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                store_chat_history: obj
                    .get("storeChatHistory")
                    .map(value_as_bool)
                    .unwrap_or(true),
                hide_online_status: obj
                    .get("hideOnlineStatus")
                    .map(value_as_bool)
                    .unwrap_or(false),
                friends_only_dms: obj
                    .get("friendsOnlyDms")
                    .map(value_as_bool)
                    .unwrap_or(false),
                discoverable: obj.get("discoverable").map(value_as_bool).unwrap_or(true),
                friend_request_privacy: {
                    let p = obj_str(&obj, "friendRequestPrivacy");
                    if p.is_empty() {
                        "everyone".to_string()
                    } else {
                        p
                    }
                },
                presence_status: {
                    let p = obj_str(&obj, "presenceStatus");
                    if p.is_empty() {
                        "online".to_string()
                    } else {
                        p
                    }
                },
                email: obj_str(&obj, "email"),
                email_verified: obj.get("emailVerified").map(value_as_bool).unwrap_or(false),
            })
        }
        FunctionResult::Value(_) => Err("Unexpected server response".to_string()),
        FunctionResult::ErrorMessage(err) => Err(humanize_error(&err)),
        FunctionResult::ConvexError(err) => Err(humanize_error(&format!("{err:?}"))),
    }
}

pub(crate) fn parse_me(result: FunctionResult, token: String) -> Result<Session, String> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => {
            let platform_role = parse_platform_role(&obj);
            Ok(Session {
                token,
                user_id: obj_str(&obj, "userId"),
                username: obj_str(&obj, "username"),
                display_name: obj_str(&obj, "displayName"),
                is_admin: platform_role == "admin" || platform_role == "owner",
                is_moderator: matches!(platform_role.as_str(), "moderator" | "admin" | "owner"),
                platform_role,
                avatar_color: obj_str(&obj, "avatarColor"),
                status_message: obj_str(&obj, "statusMessage"),
                bio: obj_str(&obj, "bio"),
                avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                store_chat_history: obj
                    .get("storeChatHistory")
                    .map(value_as_bool)
                    .unwrap_or(true),
                hide_online_status: obj
                    .get("hideOnlineStatus")
                    .map(value_as_bool)
                    .unwrap_or(false),
                friends_only_dms: obj
                    .get("friendsOnlyDms")
                    .map(value_as_bool)
                    .unwrap_or(false),
                discoverable: obj.get("discoverable").map(value_as_bool).unwrap_or(true),
                friend_request_privacy: {
                    let p = obj_str(&obj, "friendRequestPrivacy");
                    if p.is_empty() {
                        "everyone".to_string()
                    } else {
                        p
                    }
                },
                presence_status: {
                    let p = obj_str(&obj, "presenceStatus");
                    if p.is_empty() {
                        "online".to_string()
                    } else {
                        p
                    }
                },
                email: obj_str(&obj, "email"),
                email_verified: obj.get("emailVerified").map(value_as_bool).unwrap_or(false),
            })
        }
        FunctionResult::Value(_) => Err("Unexpected server response".to_string()),
        FunctionResult::ErrorMessage(err) => Err(humanize_error(&err)),
        FunctionResult::ConvexError(err) => Err(humanize_error(&format!("{err:?}"))),
    }
}

pub(crate) fn value_as_bool(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        _ => true,
    }
}

pub(crate) fn parse_clear_conversation_result(
    result: FunctionResult,
) -> Result<(u64, bool), String> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => {
            let purged = match obj.get("purged") {
                Some(Value::Float64(n)) => *n as u64,
                Some(Value::Int64(n)) => *n as u64,
                _ => 0,
            };
            let done = match obj.get("done") {
                Some(Value::Boolean(b)) => *b,
                _ => true,
            };
            Ok((purged, done))
        }
        FunctionResult::Value(_) => Err("Unexpected server response".to_string()),
        FunctionResult::ErrorMessage(err) => Err(humanize_error(&err)),
        FunctionResult::ConvexError(err) => Err(humanize_error(&format!("{err:?}"))),
    }
}

/// Strip noisy Convex/transport wrappers into short, user-facing copy.
pub(crate) fn humanize_error(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Something went wrong".to_string();
    }
    // Common Convex client noise.
    let cleaned = trimmed
        .trim_start_matches("Error: ")
        .trim_start_matches("ConvexError(")
        .trim_end_matches(')')
        .trim()
        .trim_matches('"');
    if cleaned.contains("Failed to fetch")
        || cleaned.contains("error sending request")
        || cleaned.contains("connection")
        || cleaned.contains("timed out")
        || cleaned.contains("dns error")
    {
        return "Network error — check your connection and try again".to_string();
    }
    if cleaned.contains("Unauthorized") || cleaned.contains("Invalid session") {
        return "Session expired — please log in again".to_string();
    }
    // Prefer the last non-empty line (server often stacks context).
    cleaned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or(cleaned)
        .to_string()
}

pub(crate) fn parse_profile_view(result: FunctionResult) -> Result<ProfileView, String> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => Ok(ProfileView {
            user_id: obj_str(&obj, "userId"),
            username: obj_str(&obj, "username"),
            display_name: obj_str(&obj, "displayName"),
            avatar_color: obj_str(&obj, "avatarColor"),
            avatar_image_url: obj_str(&obj, "avatarImageUrl"),
            status_message: obj_str(&obj, "statusMessage"),
            bio: obj_str(&obj, "bio"),
            last_seen_at: obj_ms(&obj, "lastSeenAt"),
            presence: {
                let p = obj_str(&obj, "presence");
                if p.is_empty() { "offline".into() } else { p }
            },
            is_staff: obj.get("isStaff").map(value_as_bool).unwrap_or(false),
            is_friend: obj.get("isFriend").map(value_as_bool).unwrap_or(false),
            can_support_dm: obj.get("canSupportDm").map(value_as_bool).unwrap_or(false),
            relation: obj_str(&obj, "relation"),
            request_id: obj_str(&obj, "requestId"),
            mutual_servers: obj_str_list(&obj, "mutualServers"),
            favorite: obj.get("favorite").map(value_as_bool).unwrap_or(false),
            nickname: obj_str(&obj, "nickname"),
            private_note: obj_str(&obj, "privateNote"),
        }),
        FunctionResult::Value(_) => Err("Unexpected server response".to_string()),
        FunctionResult::ErrorMessage(err) => Err(err),
        FunctionResult::ConvexError(err) => Err(format!("{err:?}")),
    }
}

/// `servers:serverStats` → typed counts. `None` on any error (the caller
/// just leaves the stats card in its "loading" state).
pub(crate) fn parse_server_stats(result: FunctionResult) -> Option<ServerStats> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => Some(ServerStats {
            member_count: obj_ms(&obj, "memberCount"),
            text_channels: obj_ms(&obj, "textChannels"),
            voice_channels: obj_ms(&obj, "voiceChannels"),
            role_count: obj_ms(&obj, "roleCount"),
            message_count: obj_ms(&obj, "messageCount"),
            messages_capped: obj_bool(&obj, "messagesCapped"),
            created_at: obj_ms(&obj, "createdAt"),
            oldest_member_name: obj_str(&obj, "oldestMemberName"),
            oldest_member_joined_at: obj_ms(&obj, "oldestMemberJoinedAt"),
        }),
        _ => None,
    }
}

/// `admin:adminStats` → typed platform counters. `None` on error.
pub(crate) fn parse_admin_stats(result: FunctionResult) -> Option<AdminStats> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => Some(AdminStats {
            total_users: obj_ms(&obj, "totalUsers"),
            online: obj_ms(&obj, "online"),
            banned: obj_ms(&obj, "banned"),
            staff: obj_ms(&obj, "staff"),
            bots: obj_ms(&obj, "bots"),
            servers: obj_ms(&obj, "servers"),
        }),
        _ => None,
    }
}

/// `admin:adminUserDetail` → expanded user record. `None` on error.
pub(crate) fn parse_admin_user_detail(result: FunctionResult) -> Option<AdminUserDetail> {
    match result {
        FunctionResult::Value(Value::Object(obj)) => Some(AdminUserDetail {
            user_id: obj_str(&obj, "userId"),
            username: obj_str(&obj, "username"),
            display_name: obj_str(&obj, "displayName"),
            role: obj_str(&obj, "role"),
            banned: obj_bool(&obj, "banned"),
            is_bot: obj_bool(&obj, "isBot"),
            bio: obj_str(&obj, "bio"),
            status_message: obj_str(&obj, "statusMessage"),
            avatar_color: obj_str(&obj, "avatarColor"),
            avatar_image_url: obj_str(&obj, "avatarImageUrl"),
            created_at: obj_ms(&obj, "createdAt"),
            online: obj_bool(&obj, "online"),
            last_seen_at: obj_ms(&obj, "lastSeenAt"),
            server_names: obj_str_list(&obj, "serverNames"),
            friend_count: obj_ms(&obj, "friendCount"),
        }),
        _ => None,
    }
}

/// `reports:adminListReports` → the admin panel's report queue.
pub(crate) fn parse_message_reports(
    result: FunctionResult,
) -> Result<Vec<crate::state::types::MessageReport>, String> {
    match result {
        FunctionResult::Value(Value::Array(items)) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(obj) => Some(crate::state::types::MessageReport {
                    report_id: obj_str(&obj, "reportId"),
                    message_id: obj_str(&obj, "messageId"),
                    conversation_label: obj_str(&obj, "conversationLabel"),
                    reporter_username: obj_str(&obj, "reporterUsername"),
                    author_username: obj_str(&obj, "authorUsername"),
                    message_body: obj_str(&obj, "messageBody"),
                    reason: obj_str(&obj, "reason"),
                    status: obj_str(&obj, "status"),
                    created_at: obj_ms(&obj, "createdAt"),
                }),
                _ => None,
            })
            .collect()),
        FunctionResult::ErrorMessage(msg) => Err(msg),
        _ => Err("Unexpected response".to_string()),
    }
}

pub(crate) fn parse_object_array(result: FunctionResult) -> Vec<BTreeMap<String, Value>> {
    match result {
        FunctionResult::Value(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(obj) => Some(obj),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}
