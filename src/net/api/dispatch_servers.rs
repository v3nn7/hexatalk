//! Path dispatch for the server domain: `servers:*`, `channels:*` and
//! `roles:*`.
//!
//! Each Convex path is translated into REST calls against api.vyrapp.pro
//! (Bearer token attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON responses are rebuilt into the
//! camelCase `Value` shapes that `convex_parse.rs` / `subscriptions.rs`
//! (`ServerSummary`, `ChannelSummary`, `ServerMemberRow`, `ServerRoleRow`)
//! already expect.
//!
//! Permission bits differ between the old client and the new API, so every
//! permissions value is translated at the boundary (see `perms_to_rest` /
//! `perms_to_client`). Paths with no endpoint on the new API degrade per
//! the migration doc's table — empty but correct shapes, never a crash.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::json;

use super::client::{ApiClient, ApiError};
use super::value::{FunctionResult, Value};

// ---------- Permission bits ----------
//
// Old client bitfield (convex/roles.ts, mirrored in src/state/types.rs):
const OLD_VIEW_CHANNELS: i64 = 1 << 0;
const OLD_SEND_MESSAGES: i64 = 1 << 1;
const OLD_MANAGE_CHANNELS: i64 = 1 << 2;
const OLD_KICK_MEMBERS: i64 = 1 << 3;
const OLD_MANAGE_ROLES: i64 = 1 << 4;
const OLD_MANAGE_SERVER: i64 = 1 << 5;
const OLD_CONNECT_VOICE: i64 = 1 << 6;
const OLD_SPEAK: i64 = 1 << 7;
const OLD_ANNOUNCE: i64 = 1 << 8;
const OLD_ALL: i64 = (1 << 9) - 1;
// New REST bitfield (API_REFERENCE.md):
const NEW_MANAGE_SERVER: i64 = 1;
const NEW_MANAGE_CHANNELS: i64 = 2;
const NEW_MANAGE_ROLES: i64 = 4;
const NEW_KICK: i64 = 8;
const NEW_SEND_MESSAGES: i64 = 16;

/// Client (old) bits → REST (new) bits. Bits with no server-side
/// representation (VIEW/CONNECT/SPEAK/ANNOUNCE) are dropped.
fn perms_to_rest(old: i64) -> i64 {
    let mut new = 0;
    if old & OLD_MANAGE_SERVER != 0 {
        new |= NEW_MANAGE_SERVER;
    }
    if old & OLD_MANAGE_CHANNELS != 0 {
        new |= NEW_MANAGE_CHANNELS;
    }
    if old & OLD_MANAGE_ROLES != 0 {
        new |= NEW_MANAGE_ROLES;
    }
    if old & OLD_KICK_MEMBERS != 0 {
        new |= NEW_KICK;
    }
    if old & OLD_SEND_MESSAGES != 0 {
        new |= NEW_SEND_MESSAGES;
    }
    new
}

/// REST (new) bits → client (old) bits, delta-only (used for overwrites,
/// where adding phantom baseline bits would corrupt the semantics).
fn perms_to_client_raw(new: i64) -> i64 {
    let mut old = 0;
    if new & NEW_MANAGE_SERVER != 0 {
        old |= OLD_MANAGE_SERVER;
    }
    if new & NEW_MANAGE_CHANNELS != 0 {
        old |= OLD_MANAGE_CHANNELS;
    }
    if new & NEW_MANAGE_ROLES != 0 {
        old |= OLD_MANAGE_ROLES;
    }
    if new & NEW_KICK != 0 {
        old |= OLD_KICK_MEMBERS;
    }
    if new & NEW_SEND_MESSAGES != 0 {
        old |= OLD_SEND_MESSAGES;
    }
    old
}

/// REST (new) bits → client (old) bits for role/member permission sets.
/// The new API has no VIEW/CONNECT/SPEAK concepts (membership implies
/// them), so the client-side baseline is added to keep the UI honest.
fn perms_to_client(new: i64) -> i64 {
    perms_to_client_raw(new) | OLD_VIEW_CHANNELS | OLD_CONNECT_VOICE | OLD_SPEAK
}

/// Palette mirrored from convex/roles.ts so a freshly created role looks
/// the same as it did on the old backend.
const ROLE_COLORS: [&str; 8] = [
    "#33FF66", "#88FFAA", "#00CC55", "#CCFF33", "#66DD88", "#FAA61A", "#EB459E", "#ED4245",
];

pub async fn dispatch(
    c: &ApiClient,
    module: &str,
    name: &str,
    args: BTreeMap<String, Value>,
) -> Result<FunctionResult, ApiError> {
    match (module, name) {
        // ---------- servers ----------
        ("servers", "createServer") => {
            let body = json!({ "name": arg_str(&args, "name") });
            let resp = match c.rest(Method::POST, "/servers", Some(body)).await {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "server",
            ))))
        }
        ("servers", "listMyServers") => {
            let resp = c.rest(Method::GET, "/servers", None).await?;
            let servers = json_array(resp, "servers");
            let my_id = my_user_id(c).await.unwrap_or_default();
            let mut rows: Vec<Value> = servers
                .iter()
                .map(|s| {
                    let owner_id = jstr(s, "owner_id");
                    let is_owner = jbool(s, "is_owner", false)
                        || (!owner_id.is_empty() && owner_id == my_id);
                    let icon_key = jstr(s, "icon_storage_key");
                    let mut obj = BTreeMap::new();
                    obj.insert("serverId".to_string(), Value::String(jstr(s, "id")));
                    obj.insert("name".to_string(), Value::String(jstr(s, "name")));
                    obj.insert("isOwner".to_string(), Value::Boolean(is_owner));
                    // Old contract: invite code only leaves the wire for the
                    // owner (cheap hygiene), everyone else gets "".
                    obj.insert(
                        "inviteCode".to_string(),
                        Value::String(if is_owner { jstr(s, "invite_code") } else { String::new() }),
                    );
                    obj.insert(
                        "iconUrl".to_string(),
                        Value::String(if icon_key.is_empty() {
                            String::new()
                        } else {
                            c.file_url(&icon_key)
                        }),
                    );
                    obj.insert(
                        "customSlug".to_string(),
                        Value::String(jstr(s, "custom_slug")),
                    );
                    obj.insert(
                        "description".to_string(),
                        Value::String(jstr(s, "description")),
                    );
                    obj.insert(
                        "createdAt".to_string(),
                        Value::Float64(jnum(s, "created_at")),
                    );
                    obj.insert(
                        "welcomeChannelId".to_string(),
                        Value::String(jstr(s, "welcome_channel_id")),
                    );
                    obj.insert(
                        "invitesPaused".to_string(),
                        Value::Boolean(jbool(s, "invites_paused", false)),
                    );
                    Value::Object(obj)
                })
                .collect();
            rows.sort_by(|a, b| obj_key(a, "name").cmp(&obj_key(b, "name")));
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        // Upload flow moved to client.upload_file + PATCH icon_storage_key;
        // the call sites no longer hit this path.
        ("servers", "generateIconUploadUrl") => Ok(FunctionResult::ErrorMessage(
            "Not supported yet".to_string(),
        )),
        ("servers", "setServerIcon") => {
            let server_id = arg_str(&args, "serverId");
            let storage_id = arg_str(&args, "storageId");
            // Old mutation returned the public URL so the client can paint
            // the icon immediately — compute it before the body moves the id.
            let icon_url = c.file_url(&storage_id);
            let body = json!({ "icon_storage_key": storage_id });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => Ok(FunctionResult::Value(Value::String(icon_url))),
                Err(err) => human_or(err),
            }
        }
        ("servers", "removeServerIcon") => {
            let server_id = arg_str(&args, "serverId");
            let body = json!({ "icon_storage_key": serde_json::Value::Null });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "setCustomSlug") => {
            let server_id = arg_str(&args, "serverId");
            let slug = arg_str(&args, "slug");
            let body = json!({ "custom_slug": slug.clone() });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => Ok(FunctionResult::Value(Value::String(slug))),
                Err(err) => human_or(err),
            }
        }
        ("servers", "clearCustomSlug") => {
            let server_id = arg_str(&args, "serverId");
            let body = json!({ "custom_slug": serde_json::Value::Null });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // Degradation: no public slug-resolution endpoint.
        ("servers", "resolveCustomSlug") => Ok(FunctionResult::ErrorMessage(
            "Not found".to_string(),
        )),
        ("servers", "listChannels") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let my_id = my_user_id(c).await.unwrap_or_default();
            let (perms, _) = compute_perms(&bundle, &my_id);
            let can_send = perms & OLD_SEND_MESSAGES != 0;
            let channels = json_array(bundle, "channels");
            let mut rows: Vec<Value> = channels
                .iter()
                .map(|ch| {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "conversationId".to_string(),
                        Value::String(jstr(ch, "id")),
                    );
                    obj.insert("name".to_string(), Value::String(jstr(ch, "name")));
                    let channel_type = jstr(ch, "channel_type");
                    obj.insert(
                        "channelType".to_string(),
                        Value::String(if channel_type.is_empty() {
                            "text".to_string()
                        } else {
                            channel_type
                        }),
                    );
                    // No unread/mention scan on the new API — badge hidden.
                    obj.insert("mentionCount".to_string(), Value::Float64(0.0));
                    obj.insert(
                        "categoryId".to_string(),
                        Value::String(jstr(ch, "category_id")),
                    );
                    obj.insert(
                        "position".to_string(),
                        Value::Float64(jnum(ch, "position")),
                    );
                    obj.insert(
                        "isAnnouncement".to_string(),
                        Value::Boolean(jbool(ch, "is_announcement", false)),
                    );
                    obj.insert(
                        "isSystem".to_string(),
                        Value::Boolean(jbool(ch, "is_system", false)),
                    );
                    // No notification-prefs endpoint — nothing is muted.
                    obj.insert("muted".to_string(), Value::Boolean(false));
                    obj.insert("canSend".to_string(), Value::Boolean(can_send));
                    obj.insert(
                        "permissions".to_string(),
                        Value::Float64(perms as f64),
                    );
                    Value::Object(obj)
                })
                .collect();
            // Old ordering: announcements pinned top, text before voice,
            // then position, then name.
            rows.sort_by(|a, b| {
                let ann = obj_bool_key(b, "isAnnouncement").cmp(&obj_bool_key(a, "isAnnouncement"));
                if ann != std::cmp::Ordering::Equal {
                    return ann;
                }
                let ta = obj_key(a, "channelType");
                let tb = obj_key(b, "channelType");
                let ta_text = ta != "voice";
                let tb_text = tb != "voice";
                if ta_text != tb_text {
                    return tb_text.cmp(&ta_text);
                }
                let pa = obj_num_key(a, "position") as i64;
                let pb = obj_num_key(b, "position") as i64;
                if pa != pb {
                    return pa.cmp(&pb);
                }
                obj_key(a, "name").cmp(&obj_key(b, "name"))
            });
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        ("servers", "renameServer") => {
            let server_id = arg_str(&args, "serverId");
            let body = json!({ "name": arg_str(&args, "name") });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "deleteServer") => {
            let server_id = arg_str(&args, "serverId");
            match c
                .rest(Method::DELETE, &format!("/servers/{server_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // Degradation: no invite-regeneration endpoint.
        ("servers", "regenerateInviteCode") => Ok(FunctionResult::ErrorMessage(
            "Not supported yet".to_string(),
        )),
        ("servers", "setServerDescription") => {
            let server_id = arg_str(&args, "serverId");
            let body = json!({ "description": arg_str(&args, "description") });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // Degradation: no ownership-transfer endpoint.
        ("servers", "transferOwnership") => Ok(FunctionResult::ErrorMessage(
            "Not supported yet".to_string(),
        )),
        ("servers", "setWelcomeChannel") => {
            let server_id = arg_str(&args, "serverId");
            let channel_id = arg_str(&args, "channelId");
            let body = if channel_id.trim().is_empty() {
                json!({ "welcome_channel_id": serde_json::Value::Null })
            } else {
                json!({ "welcome_channel_id": channel_id })
            };
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "setInvitesPaused") => {
            let server_id = arg_str(&args, "serverId");
            let body = json!({ "invites_paused": arg_bool(&args, "paused").unwrap_or(false) });
            match c
                .rest(Method::PATCH, &format!("/servers/{server_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // Degradation: no stats endpoint — zeros/nulls in the expected
        // shape so the Overview card renders an empty state.
        ("servers", "serverStats") => {
            let mut obj = BTreeMap::new();
            obj.insert("memberCount".to_string(), Value::Float64(0.0));
            obj.insert("textChannels".to_string(), Value::Float64(0.0));
            obj.insert("voiceChannels".to_string(), Value::Float64(0.0));
            obj.insert("roleCount".to_string(), Value::Float64(0.0));
            obj.insert("messageCount".to_string(), Value::Float64(0.0));
            obj.insert("messagesCapped".to_string(), Value::Boolean(false));
            obj.insert("createdAt".to_string(), Value::Float64(0.0));
            obj.insert(
                "oldestMemberName".to_string(),
                Value::String(String::new()),
            );
            obj.insert("oldestMemberJoinedAt".to_string(), Value::Float64(0.0));
            Ok(FunctionResult::Value(Value::Object(obj)))
        }
        ("servers", "listMembers") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let server = bundle.get("server").cloned().unwrap_or(serde_json::Value::Null);
            let owner_id = jstr(&server, "owner_id");
            let roles = json_array(bundle.clone(), "roles");
            let members = json_array(bundle, "members");
            let mut rows: Vec<Value> = members
                .iter()
                .map(|m| {
                    let user_id = member_user_id(m);
                    let is_owner = !owner_id.is_empty() && user_id == owner_id;
                    let avatar_key = member_str(m, "avatar_storage_key");
                    // Only explicitly-assigned, non-default (position != 0)
                    // roles become badges — same as the old query.
                    let role_tags: Vec<Value> = if is_owner {
                        Vec::new()
                    } else {
                        member_role_ids(m)
                            .iter()
                            .filter_map(|rid| roles.iter().find(|r| jstr(r, "id") == *rid))
                            .filter(|r| jnum(r, "position") as i64 != 0)
                            .map(|r| {
                                let mut tag = BTreeMap::new();
                                tag.insert("roleId".to_string(), Value::String(jstr(r, "id")));
                                tag.insert("name".to_string(), Value::String(jstr(r, "name")));
                                tag.insert("color".to_string(), Value::String(jstr(r, "color")));
                                Value::Object(tag)
                            })
                            .collect()
                    };
                    let mut obj = BTreeMap::new();
                    obj.insert("userId".to_string(), Value::String(user_id));
                    obj.insert(
                        "displayName".to_string(),
                        Value::String(member_str(m, "display_name")),
                    );
                    obj.insert(
                        "username".to_string(),
                        Value::String(member_str(m, "username")),
                    );
                    obj.insert(
                        "avatarColor".to_string(),
                        Value::String(member_str(m, "avatar_color")),
                    );
                    obj.insert(
                        "avatarImageUrl".to_string(),
                        Value::String(if avatar_key.is_empty() {
                            String::new()
                        } else {
                            c.file_url(&avatar_key)
                        }),
                    );
                    obj.insert("isOwner".to_string(), Value::Boolean(is_owner));
                    obj.insert(
                        "isBot".to_string(),
                        Value::Boolean(member_bool(m, "is_bot")),
                    );
                    let role = member_str(m, "role");
                    obj.insert(
                        "platformRole".to_string(),
                        Value::String(match role.as_str() {
                            "owner" | "admin" | "moderator" => role,
                            _ => "user".to_string(),
                        }),
                    );
                    // No Plus on the new API.
                    obj.insert("plusActive".to_string(), Value::Boolean(false));
                    // Presence arrives via WS; no per-member presence fetch.
                    obj.insert("lastSeenAt".to_string(), Value::Float64(0.0));
                    obj.insert("roles".to_string(), Value::Array(role_tags));
                    Value::Object(obj)
                })
                .collect();
            // Old ordering: owner first, bots last, then display name
            // (online-first ordering is moot — lastSeenAt is 0 for all).
            rows.sort_by(|a, b| {
                let owner = obj_bool_key(b, "isOwner").cmp(&obj_bool_key(a, "isOwner"));
                if owner != std::cmp::Ordering::Equal {
                    return owner;
                }
                let bot = obj_bool_key(a, "isBot").cmp(&obj_bool_key(b, "isBot"));
                if bot != std::cmp::Ordering::Equal {
                    return bot;
                }
                obj_key(a, "displayName").cmp(&obj_key(b, "displayName"))
            });
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        ("servers", "kickMember") => {
            let server_id = arg_str(&args, "serverId");
            let user_id = arg_str(&args, "userId");
            match c
                .rest(
                    Method::DELETE,
                    &format!("/servers/{server_id}/members/{user_id}"),
                    None,
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "joinByInviteCode") => {
            let body = json!({ "inviteCode": arg_str(&args, "inviteCode") });
            let resp = match c.rest(Method::POST, "/servers/join", Some(body)).await {
                Ok(resp) => resp,
                // "Invalid invite code" / invites paused — user-facing copy.
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "server",
            ))))
        }
        ("servers", "leaveServer") => {
            let server_id = arg_str(&args, "serverId");
            match c
                .rest(Method::POST, &format!("/servers/{server_id}/leave"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "createChannel") => {
            let server_id = arg_str(&args, "serverId");
            let channel_type = arg_str(&args, "channelType");
            let body = json!({
                "name": arg_str(&args, "name"),
                "channelType": if channel_type.is_empty() { "text".to_string() } else { channel_type },
            });
            let resp = match c
                .rest(
                    Method::POST,
                    &format!("/servers/{server_id}/channels"),
                    Some(body),
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "channel",
            ))))
        }
        ("servers", "renameChannel") => {
            let conversation_id = arg_str(&args, "conversationId");
            let body = json!({ "name": arg_str(&args, "name") });
            match c
                .rest(
                    Method::PATCH,
                    &format!("/channels/{conversation_id}"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("servers", "deleteChannel") => {
            let conversation_id = arg_str(&args, "conversationId");
            match c
                .rest(Method::DELETE, &format!("/channels/{conversation_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }

        // ---------- channels ----------
        ("channels", "listCategories") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let cats = json_array(bundle, "categories");
            let mut rows: Vec<Value> = cats
                .iter()
                .map(|cat| {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "categoryId".to_string(),
                        Value::String(jstr(cat, "id")),
                    );
                    obj.insert("name".to_string(), Value::String(jstr(cat, "name")));
                    obj.insert(
                        "position".to_string(),
                        Value::Float64(jnum(cat, "position")),
                    );
                    Value::Object(obj)
                })
                .collect();
            rows.sort_by(|a, b| {
                let pa = obj_num_key(a, "position") as i64;
                let pb = obj_num_key(b, "position") as i64;
                if pa != pb {
                    return pa.cmp(&pb);
                }
                obj_key(a, "name").cmp(&obj_key(b, "name"))
            });
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        ("channels", "createCategory") => {
            let server_id = arg_str(&args, "serverId");
            // The REST endpoint wants an explicit position — mirror the old
            // "max + 1" behaviour.
            let bundle = server_bundle(c, &server_id).await?;
            let cats = json_array(bundle, "categories");
            let position = cats
                .iter()
                .map(|cat| jnum(cat, "position") as i64)
                .max()
                .unwrap_or(0)
                + 1;
            let body = json!({ "name": arg_str(&args, "name"), "position": position });
            let resp = match c
                .rest(
                    Method::POST,
                    &format!("/servers/{server_id}/categories"),
                    Some(body),
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "category",
            ))))
        }
        // Degradation: no PATCH /categories/:id on the new API.
        ("channels", "renameCategory") => Ok(FunctionResult::ErrorMessage(
            "Not supported yet".to_string(),
        )),
        ("channels", "deleteCategory") => {
            let category_id = arg_str(&args, "categoryId");
            match c
                .rest(Method::DELETE, &format!("/categories/{category_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("channels", "setChannelCategory") => {
            let conversation_id = arg_str(&args, "conversationId");
            let mut body = serde_json::Map::new();
            match arg_opt_str(&args, "categoryId") {
                Some(cat) if !cat.is_empty() => {
                    body.insert("categoryId".into(), json!(cat));
                }
                _ => {
                    body.insert("categoryId".into(), serde_json::Value::Null);
                }
            }
            if let Some(pos) = arg_f64(&args, "position") {
                body.insert("position".into(), json!(pos as i64));
            }
            match c
                .rest(
                    Method::PATCH,
                    &format!("/channels/{conversation_id}"),
                    Some(serde_json::Value::Object(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("channels", "moveChannel") => {
            let conversation_id = arg_str(&args, "conversationId");
            let direction = arg_str(&args, "direction");
            let Some((_, bundle)) = find_in_servers(c, "channels", &conversation_id).await? else {
                return Ok(FunctionResult::ErrorMessage("Not a server channel".to_string()));
            };
            let channels = json_array(bundle, "channels");
            let Some(target) = channels.iter().find(|ch| jstr(ch, "id") == conversation_id)
            else {
                return Ok(FunctionResult::ErrorMessage("Not a server channel".to_string()));
            };
            if jbool(target, "is_system", false) || jbool(target, "is_announcement", false) {
                return Ok(FunctionResult::ErrorMessage(
                    "System / announcements channels stay at the top".to_string(),
                ));
            }
            let channel_type = {
                let t = jstr(target, "channel_type");
                if t.is_empty() {
                    "text".to_string()
                } else {
                    t
                }
            };
            // Peers: same type, not system/announcement, ordered like the
            // sidebar (position, then name).
            let mut peers: Vec<&serde_json::Value> = channels
                .iter()
                .filter(|ch| {
                    let t = jstr(ch, "channel_type");
                    let t = if t.is_empty() { "text".to_string() } else { t };
                    t == channel_type
                        && !jbool(ch, "is_system", false)
                        && !jbool(ch, "is_announcement", false)
                })
                .collect();
            peers.sort_by(|a, b| {
                let pa = jnum(a, "position") as i64;
                let pb = jnum(b, "position") as i64;
                if pa != pb {
                    return pa.cmp(&pb);
                }
                jstr(a, "name").cmp(&jstr(b, "name"))
            });
            let Some(idx) = peers.iter().position(|ch| jstr(ch, "id") == conversation_id) else {
                return Ok(FunctionResult::ErrorMessage(
                    "Channel not found in list".to_string(),
                ));
            };
            let swap_with = if direction == "up" {
                idx.wrapping_sub(1)
            } else {
                idx + 1
            };
            if swap_with >= peers.len() || (direction == "up" && idx == 0) {
                return ok_null(); // already at the edge
            }
            let pos_a = jnum(peers[idx], "position") as i64;
            let pos_b = jnum(peers[swap_with], "position") as i64;
            // Distinct positions swap directly; equal/missing ones fall back
            // to the dense indices.
            let (new_a, new_b) = if pos_a != pos_b {
                (pos_b, pos_a)
            } else {
                (swap_with as i64, idx as i64)
            };
            c.rest(
                Method::PATCH,
                &format!("/channels/{conversation_id}"),
                Some(json!({ "position": new_a })),
            )
            .await?;
            let other_id = jstr(peers[swap_with], "id");
            c.rest(
                Method::PATCH,
                &format!("/channels/{other_id}"),
                Some(json!({ "position": new_b })),
            )
            .await?;
            ok_null()
        }
        ("channels", "listOverwrites") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/channels/{conversation_id}/overwrites"),
                    None,
                )
                .await?;
            let rows = json_array(resp, "overwrites");
            let out: Vec<Value> = rows
                .iter()
                .map(|ow| {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "overwriteId".to_string(),
                        Value::String(jstr(ow, "id")),
                    );
                    obj.insert(
                        "targetType".to_string(),
                        Value::String(jstr(ow, "target_type")),
                    );
                    obj.insert(
                        "targetId".to_string(),
                        Value::String(jstr(ow, "target_id")),
                    );
                    // Bit translation back to the client layout (delta-only,
                    // no baseline).
                    obj.insert(
                        "allow".to_string(),
                        Value::Float64(perms_to_client_raw(jnum(ow, "allow") as i64) as f64),
                    );
                    obj.insert(
                        "deny".to_string(),
                        Value::Float64(perms_to_client_raw(jnum(ow, "deny") as i64) as f64),
                    );
                    Value::Object(obj)
                })
                .collect();
            Ok(FunctionResult::Value(Value::Array(out)))
        }
        ("channels", "setOverwrite") => {
            let conversation_id = arg_str(&args, "conversationId");
            let target_type = arg_str(&args, "targetType");
            let target_id = arg_str(&args, "targetId");
            let allow_old = arg_f64(&args, "allow").unwrap_or(0.0) as i64;
            let deny_old = arg_f64(&args, "deny").unwrap_or(0.0) as i64;
            if allow_old == 0 && deny_old == 0 {
                // Old mutation deleted the row when both masks cleared —
                // resolve the overwrite id and DELETE it (no-op if absent).
                let resp = c
                    .rest(
                        Method::GET,
                        &format!("/channels/{conversation_id}/overwrites"),
                        None,
                    )
                    .await?;
                for ow in json_array(resp, "overwrites") {
                    if jstr(&ow, "target_type") == target_type
                        && jstr(&ow, "target_id") == target_id
                    {
                        let ow_id = jstr(&ow, "id");
                        if !ow_id.is_empty() {
                            c.rest(
                                Method::DELETE,
                                &format!("/channels/{conversation_id}/overwrites/{ow_id}"),
                                None,
                            )
                            .await?;
                        }
                        break;
                    }
                }
                return ok_null();
            }
            let body = json!({
                "targetType": target_type,
                "targetId": target_id,
                "allow": perms_to_rest(allow_old),
                "deny": perms_to_rest(deny_old),
            });
            match c
                .rest(
                    Method::PUT,
                    &format!("/channels/{conversation_id}/overwrites"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // No way to resolve the channel from a bare overwrite id on the new
        // API (DELETE needs /channels/:id/overwrites/:id) and no call site
        // uses this path — degrade to a harmless no-op.
        ("channels", "deleteOverwrite") => ok_null(),
        ("channels", "myChannelPermissions") => {
            let conversation_id = arg_str(&args, "conversationId");
            match find_in_servers(c, "channels", &conversation_id).await? {
                Some((_, bundle)) => {
                    let my_id = my_user_id(c).await.unwrap_or_default();
                    let (perms, _) = compute_perms(&bundle, &my_id);
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "permissions".to_string(),
                        Value::Float64(perms as f64),
                    );
                    obj.insert(
                        "canSend".to_string(),
                        Value::Boolean(perms & OLD_SEND_MESSAGES != 0),
                    );
                    obj.insert("canView".to_string(), Value::Boolean(true));
                    Ok(FunctionResult::Value(Value::Object(obj)))
                }
                // DM / group: full chat rights for members, same as the old
                // channelPermissions fallback.
                None => {
                    let mut obj = BTreeMap::new();
                    obj.insert(
                        "permissions".to_string(),
                        Value::Float64(OLD_ALL as f64),
                    );
                    obj.insert("canSend".to_string(), Value::Boolean(true));
                    obj.insert("canView".to_string(), Value::Boolean(true));
                    Ok(FunctionResult::Value(Value::Object(obj)))
                }
            }
        }
        // Degradation: no notification-prefs endpoints — the client updates
        // its own optimistic state, so accept writes as no-ops and report
        // nothing muted.
        ("channels", "setMute") => ok_null(),
        ("channels", "listMutes") => Ok(FunctionResult::Value(Value::Array(Vec::new()))),
        ("channels", "ensureAnnouncementChannel") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let channels = json_array(bundle, "channels");
            if let Some(existing) = channels
                .iter()
                .find(|ch| jbool(ch, "is_announcement", false))
            {
                return Ok(FunctionResult::Value(Value::String(jstr(
                    existing, "id",
                ))));
            }
            let body = json!({ "name": "announcements", "channelType": "text" });
            let resp = match c
                .rest(
                    Method::POST,
                    &format!("/servers/{server_id}/channels"),
                    Some(body),
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "channel",
            ))))
        }
        ("channels", "createTextChannel") => {
            let server_id = arg_str(&args, "serverId");
            let channel_type = arg_str(&args, "channelType");
            let mut body = serde_json::Map::new();
            body.insert("name".into(), json!(arg_str(&args, "name")));
            body.insert(
                "channelType".into(),
                json!(if channel_type.is_empty() {
                    "text".to_string()
                } else {
                    channel_type
                }),
            );
            if let Some(cat) = arg_opt_str(&args, "categoryId") {
                if !cat.is_empty() {
                    body.insert("categoryId".into(), json!(cat));
                }
            }
            let resp = match c
                .rest(
                    Method::POST,
                    &format!("/servers/{server_id}/channels"),
                    Some(serde_json::Value::Object(body)),
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(
                &resp, "channel",
            ))))
        }

        // ---------- roles ----------
        ("roles", "listRoles") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let roles = json_array(bundle, "roles");
            let mut rows: Vec<Value> = roles
                .iter()
                .map(|r| {
                    let mut obj = BTreeMap::new();
                    obj.insert("roleId".to_string(), Value::String(jstr(r, "id")));
                    obj.insert("name".to_string(), Value::String(jstr(r, "name")));
                    obj.insert("color".to_string(), Value::String(jstr(r, "color")));
                    obj.insert(
                        "position".to_string(),
                        Value::Float64(jnum(r, "position")),
                    );
                    obj.insert(
                        "permissions".to_string(),
                        Value::Float64(perms_to_client(jnum(r, "permissions") as i64) as f64),
                    );
                    Value::Object(obj)
                })
                .collect();
            // Old ordering: position desc, then name.
            rows.sort_by(|a, b| {
                let pa = obj_num_key(a, "position") as i64;
                let pb = obj_num_key(b, "position") as i64;
                if pa != pb {
                    return pb.cmp(&pa);
                }
                obj_key(a, "name").cmp(&obj_key(b, "name"))
            });
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        ("roles", "createRole") => {
            let server_id = arg_str(&args, "serverId");
            // The REST endpoint wants color/position/permissions up front —
            // mirror the old "max position + 1, rotating palette, everyone
            // defaults" behaviour.
            let bundle = server_bundle(c, &server_id).await?;
            let roles = json_array(bundle, "roles");
            let position = roles
                .iter()
                .map(|r| jnum(r, "position") as i64)
                .max()
                .unwrap_or(0)
                + 1;
            let color = ROLE_COLORS[roles.len() % ROLE_COLORS.len()];
            let body = json!({
                "name": arg_str(&args, "name"),
                "color": color,
                "position": position,
                "permissions": NEW_SEND_MESSAGES,
            });
            let resp = match c
                .rest(
                    Method::POST,
                    &format!("/servers/{server_id}/roles"),
                    Some(body),
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return human_or(err),
            };
            Ok(FunctionResult::Value(Value::String(extract_id(&resp, "role"))))
        }
        ("roles", "updateRole") => {
            let role_id = arg_str(&args, "roleId");
            let mut body = serde_json::Map::new();
            if let Some(name) = arg_opt_str(&args, "name") {
                body.insert("name".into(), json!(name));
            }
            if let Some(color) = arg_opt_str(&args, "color") {
                body.insert("color".into(), json!(color));
            }
            if let Some(perms) = arg_f64(&args, "permissions") {
                body.insert(
                    "permissions".into(),
                    json!(perms_to_rest(perms as i64)),
                );
            }
            if body.is_empty() {
                return ok_null();
            }
            match c
                .rest(
                    Method::PATCH,
                    &format!("/roles/{role_id}"),
                    Some(serde_json::Value::Object(body)),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("roles", "deleteRole") => {
            let role_id = arg_str(&args, "roleId");
            match c
                .rest(Method::DELETE, &format!("/roles/{role_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("roles", "toggleRole") => {
            let server_id = arg_str(&args, "serverId");
            let user_id = arg_str(&args, "userId");
            let role_id = arg_str(&args, "roleId");
            // The REST endpoint replaces the whole role set, so read the
            // member's current roles, toggle locally, write back.
            let bundle = match server_bundle(c, &server_id).await {
                Ok(bundle) => bundle,
                Err(err) => return human_or(err),
            };
            let members = json_array(bundle, "members");
            let Some(member) = members.iter().find(|m| member_user_id(m) == user_id) else {
                return Ok(FunctionResult::ErrorMessage(
                    "That user isn't a member of this server".to_string(),
                ));
            };
            let mut role_ids = member_role_ids(member);
            if let Some(pos) = role_ids.iter().position(|r| r == &role_id) {
                role_ids.remove(pos);
            } else {
                role_ids.push(role_id);
            }
            let body = json!({ "roleIds": role_ids });
            match c
                .rest(
                    Method::PUT,
                    &format!("/servers/{server_id}/members/{user_id}/roles"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("roles", "myPermissions") => {
            let server_id = arg_str(&args, "serverId");
            let bundle = server_bundle(c, &server_id).await?;
            let my_id = my_user_id(c).await.unwrap_or_default();
            let (perms, is_owner) = compute_perms(&bundle, &my_id);
            let mut obj = BTreeMap::new();
            obj.insert(
                "permissions".to_string(),
                Value::Float64(perms as f64),
            );
            obj.insert("isOwner".to_string(), Value::Boolean(is_owner));
            Ok(FunctionResult::Value(Value::Object(obj)))
        }
        ("roles", "moveRole") => {
            let role_id = arg_str(&args, "roleId");
            let direction = arg_str(&args, "direction");
            let Some((_, bundle)) = find_in_servers(c, "roles", &role_id).await? else {
                return Ok(FunctionResult::ErrorMessage("Role not found".to_string()));
            };
            let roles = json_array(bundle, "roles");
            let Some(target) = roles.iter().find(|r| jstr(r, "id") == role_id) else {
                return Ok(FunctionResult::ErrorMessage("Role not found".to_string()));
            };
            if jnum(target, "position") as i64 == 0 {
                return Ok(FunctionResult::ErrorMessage(
                    "The everyone role stays at the bottom".to_string(),
                ));
            }
            // Higher position = more power, so "up" raises the position.
            let mut movable: Vec<&serde_json::Value> = roles
                .iter()
                .filter(|r| jnum(r, "position") as i64 != 0)
                .collect();
            movable.sort_by(|a, b| {
                let pa = jnum(a, "position") as i64;
                let pb = jnum(b, "position") as i64;
                if pa != pb {
                    return pa.cmp(&pb);
                }
                jstr(a, "name").cmp(&jstr(b, "name"))
            });
            let Some(idx) = movable.iter().position(|r| jstr(r, "id") == role_id) else {
                return Ok(FunctionResult::ErrorMessage(
                    "Role not found in list".to_string(),
                ));
            };
            let swap_with = if direction == "up" {
                idx + 1
            } else {
                idx.wrapping_sub(1)
            };
            if swap_with >= movable.len() || (direction == "down" && idx == 0) {
                return ok_null(); // already at the edge
            }
            let pos_a = jnum(movable[idx], "position") as i64;
            let pos_b = jnum(movable[swap_with], "position") as i64;
            let (new_a, new_b) = if pos_a != pos_b {
                (pos_b, pos_a)
            } else {
                ((swap_with + 1) as i64, (idx + 1) as i64)
            };
            c.rest(
                Method::PATCH,
                &format!("/roles/{role_id}"),
                Some(json!({ "position": new_a })),
            )
            .await?;
            let other_id = jstr(movable[swap_with], "id");
            c.rest(
                Method::PATCH,
                &format!("/roles/{other_id}"),
                Some(json!({ "position": new_b })),
            )
            .await?;
            ok_null()
        }

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

fn arg_f64(args: &BTreeMap<String, Value>, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Int64(i)) => Some(*i as f64),
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

fn jnum(v: &serde_json::Value, key: &str) -> f64 {
    match v.get(key) {
        Some(x) => x.as_f64().unwrap_or(0.0),
        None => 0.0,
    }
}

fn jbool(v: &serde_json::Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(default)
}

/// Accepts both a bare array response and a `{ <key>: [...] }` envelope —
/// the reference documents the envelope form, but staying shape-tolerant
/// costs nothing.
fn json_array(resp: serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    match resp {
        serde_json::Value::Array(items) => items,
        other => other
            .get(key)
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default(),
    }
}

/// Pulls an id out of a creation/join response, tolerating both flat
/// (`{id}` / `{server_id}`) and nested (`{server: {id}}`) shapes.
fn extract_id(resp: &serde_json::Value, nested: &str) -> String {
    let nested_id = resp.get(nested).map(|x| jstr(x, "id")).unwrap_or_default();
    for cand in [
        nested_id,
        jstr(resp, "id"),
        jstr(resp, "server_id"),
        jstr(resp, "serverId"),
        jstr(resp, "conversation_id"),
        jstr(resp, "conversationId"),
    ] {
        if !cand.is_empty() {
            return cand;
        }
    }
    String::new()
}

// ---------- Value-object sort helpers (operate on built Value rows) ----------

fn obj_key(v: &Value, key: &str) -> String {
    match v {
        Value::Object(obj) => match obj.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn obj_num_key(v: &Value, key: &str) -> f64 {
    match v {
        Value::Object(obj) => match obj.get(key) {
            Some(Value::Float64(f)) => *f,
            Some(Value::Int64(i)) => *i as f64,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

fn obj_bool_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(obj) => matches!(obj.get(key), Some(Value::Boolean(true))),
        _ => false,
    }
}

// ---------- member helpers (flat or nested-user member rows) ----------

fn member_user_id(m: &serde_json::Value) -> String {
    let direct = jstr(m, "user_id");
    if !direct.is_empty() {
        return direct;
    }
    let nested = m.get("user").map(|u| jstr(u, "id")).unwrap_or_default();
    if !nested.is_empty() {
        return nested;
    }
    jstr(m, "id")
}

/// Reads `field` from the member row, falling back to a nested `user`
/// object (both shapes appear plausible for GET /servers/:id members).
fn member_str(m: &serde_json::Value, field: &str) -> String {
    let direct = jstr(m, field);
    if !direct.is_empty() {
        return direct;
    }
    m.get("user").map(|u| jstr(u, field)).unwrap_or_default()
}

fn member_bool(m: &serde_json::Value, field: &str) -> bool {
    if let Some(b) = m.get(field).and_then(|x| x.as_bool()) {
        return b;
    }
    m.get("user")
        .and_then(|u| u.get(field))
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

fn member_role_ids(m: &serde_json::Value) -> Vec<String> {
    for key in ["role_ids", "roleIds"] {
        if let Some(arr) = m.get(key).and_then(|x| x.as_array()) {
            return arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }
    Vec::new()
}

// ---------- server bundle helpers ----------

/// GET /servers/:id → `{server, channels, categories, roles, members}` —
/// the single aggregate read most legacy `list*` queries decompose into.
async fn server_bundle(c: &ApiClient, server_id: &str) -> Result<serde_json::Value, ApiError> {
    c.rest(Method::GET, &format!("/servers/{server_id}"), None).await
}

/// My user id, needed wherever the old code compared `ownerId === me._id`
/// or resolved "my" membership. Failures degrade to "" (is_owner=false,
/// @everyone-only perms) rather than breaking the whole query.
///
/// The id never changes within a session, but `listMyServers`,
/// `listChannels`, `myPermissions` and `myChannelPermissions` all call this
/// on *every* subscription refetch — an extra GET /users/me per refresh per
/// query. Cache the id process-locally, keyed by the session token so a
/// logout/login (or account switch) invalidates automatically.
async fn my_user_id(c: &ApiClient) -> Result<String, ApiError> {
    use std::sync::{OnceLock, RwLock};
    /// (session token, user id) — single-entry cache; the token is the key.
    static ME_ID: OnceLock<RwLock<(String, String)>> = OnceLock::new();
    let cache = ME_ID.get_or_init(|| RwLock::new((String::new(), String::new())));
    let token = c.session_token().unwrap_or_default();
    if !token.is_empty() {
        if let Ok(guard) = cache.read() {
            if guard.0 == token && !guard.1.is_empty() {
                return Ok(guard.1.clone());
            }
        }
    }
    let resp = c.rest(Method::GET, "/users/me", None).await?;
    let mut id = jstr(&resp, "id");
    if id.is_empty() {
        id = resp.get("user").map(|u| jstr(u, "id")).unwrap_or_default();
    }
    if !token.is_empty() && !id.is_empty() {
        if let Ok(mut guard) = cache.write() {
            *guard = (token, id.clone());
        }
    }
    Ok(id)
}

/// Union of @everyone (position 0) + explicitly assigned roles, translated
/// to client bits; the owner short-circuits to ALL, mirroring the old
/// `memberPermissions`. Returns (perms, is_owner).
fn compute_perms(bundle: &serde_json::Value, my_id: &str) -> (i64, bool) {
    let server = bundle.get("server").cloned().unwrap_or(serde_json::Value::Null);
    let owner_id = jstr(&server, "owner_id");
    if !owner_id.is_empty() && !my_id.is_empty() && owner_id == my_id {
        return (OLD_ALL, true);
    }
    let roles = bundle
        .get("roles")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    // @everyone baseline; default SEND_MESSAGES when the role is missing.
    let mut new_perms = NEW_SEND_MESSAGES;
    if let Some(everyone) = roles.iter().find(|r| jnum(r, "position") as i64 == 0) {
        new_perms = jnum(everyone, "permissions") as i64;
    }
    let my_role_ids: Vec<String> = bundle
        .get("members")
        .and_then(|m| m.as_array())
        .and_then(|ms| ms.iter().find(|m| member_user_id(m) == my_id))
        .map(member_role_ids)
        .unwrap_or_default();
    for role in &roles {
        let rid = jstr(role, "id");
        if !rid.is_empty() && my_role_ids.iter().any(|x| x == &rid) {
            new_perms |= jnum(role, "permissions") as i64;
        }
    }
    (perms_to_client(new_perms), false)
}

/// Locates the server bundle containing a given channel/role id by
/// scanning my servers (the new API has no "get channel by id" endpoint).
/// Returns (server_id, bundle).
async fn find_in_servers(
    c: &ApiClient,
    collection: &str,
    id: &str,
) -> Result<Option<(String, serde_json::Value)>, ApiError> {
    let resp = c.rest(Method::GET, "/servers", None).await?;
    for server in json_array(resp, "servers") {
        let server_id = jstr(&server, "id");
        if server_id.is_empty() {
            continue;
        }
        let bundle = server_bundle(c, &server_id).await?;
        let found = bundle
            .get(collection)
            .and_then(|x| x.as_array())
            .map(|items| items.iter().any(|item| jstr(item, "id") == id))
            .unwrap_or(false);
        if found {
            return Ok(Some((server_id, bundle)));
        }
    }
    Ok(None)
}

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// mutations surfaced 4xx bodies as user-facing messages (e.g. "Invalid
/// invite code", "Missing permission"), so downgrade those to
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
