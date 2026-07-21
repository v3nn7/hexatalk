//! Path dispatch for the chat domain: `conversations:*` and `messages:*`.
//!
//! Each Convex path is translated into a REST call against api.vyrapp.pro
//! (Bearer token is attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON response is rebuilt into the
//! camelCase `Value` shape that `subscriptions.rs` (conversations / messages /
//! pins parsers) and the `update.rs` call sites already expect:
//!
//! - conversation summary: `{conversationId, kind, title, peerUserId?,
//!   lastMessageAt, unread, mentionCount}`
//! - message: `{id, authorId, authorName, authorAvatarColor,
//!   authorAvatarImageUrl, authorIsBot, authorPlusActive, body, kind,
//!   encrypted, attachmentUrl, reactions:[{emoji,count,reactedByMe}],
//!   replyTo:{authorName,snippet,encrypted}?, deleted, edited, pinned, sentAt}`
//! - pinned row: `{id, authorId, authorName, snippet, encrypted, pinned,
//!   sentAt}`
//!
//! Timestamps/counts go out as `Value::Float64` (as Convex numbers did).
//! Numeric fields may arrive as JSON strings on the new API (node `pg`
//! serializes BIGINT as text), so all numeric reads are string-tolerant.
//!
//! `messages:toggleReaction` has no toggle endpoint on the new API — only
//! idempotent PUT/DELETE — so this module keeps a small process-local cache
//! of `reacted_by_me` per (message, emoji), populated on every
//! `messages:list` fetch, to decide which verb to send. Paths with no
//! endpoint degrade per the migration doc's table.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

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
        // ---------- conversations ----------
        ("conversations", "listMyConversations") => {
            let resp = c.rest(Method::GET, "/conversations", None).await?;
            let items = jlist(&resp, &["conversations", "items"]);
            let mut out = Vec::with_capacity(items.len());
            for m in items {
                let mut obj = BTreeMap::new();
                obj.insert(
                    "conversationId".to_string(),
                    Value::String(js(m, &["conversation_id", "id"])),
                );
                obj.insert("kind".to_string(), Value::String(js(m, &["kind"])));
                let title = js(m, &["title", "name", "peer_display_name"]);
                obj.insert(
                    "title".to_string(),
                    Value::String(if title.is_empty() {
                        "Chat".to_string()
                    } else {
                        title
                    }),
                );
                let peer = js(m, &["peer_user_id", "peer_id", "peerUserId"]);
                if !peer.is_empty() {
                    obj.insert("peerUserId".to_string(), Value::String(peer));
                }
                let last_message_at = jnum(m, &["last_message_at", "lastMessageAt"]);
                obj.insert(
                    "lastMessageAt".to_string(),
                    Value::Float64(last_message_at),
                );
                // Prefer the server-computed flag; fall back to the legacy
                // rule (lastMessageAt > lastReadAt) when it isn't sent.
                let unread = match jopt_bool(m, "unread") {
                    Some(b) => b,
                    None => last_message_at > jnum(m, &["last_read_at", "lastReadAt"]),
                };
                obj.insert("unread".to_string(), Value::Boolean(unread));
                obj.insert(
                    "mentionCount".to_string(),
                    Value::Float64(jnum(m, &["mention_count", "mentionCount"])),
                );
                out.push(Value::Object(obj));
            }
            // Server order is already last_message_at DESC — keep it.
            ok(Value::Array(out))
        }
        ("conversations", "getOrCreateDirect") => {
            let body = json!({ "userId": arg_str(&args, "friendUserId") });
            match c.rest(Method::POST, "/conversations/direct", Some(body)).await {
                Ok(resp) => conversation_id_result(&resp),
                // 403 blocked / friends-only — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("conversations", "createGroup") => {
            let body = json!({
                "name": arg_str(&args, "name"),
                "userIds": arg_str_list(&args, "memberUserIds"),
            });
            match c.rest(Method::POST, "/conversations/group", Some(body)).await {
                Ok(resp) => conversation_id_result(&resp),
                Err(err) => human_or(err),
            }
        }
        ("conversations", "markRead") => {
            let conversation_id = arg_str(&args, "conversationId");
            if !conversation_id.is_empty() {
                // Legacy behavior: read receipts are best-effort and never
                // surface an error (non-members silently no-op'd).
                let _ = c
                    .rest(
                        Method::PATCH,
                        &format!("/conversations/{conversation_id}/read"),
                        None,
                    )
                    .await;
            }
            ok_null()
        }
        ("conversations", "readStates") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}"),
                    None,
                )
                .await?;
            let members = jlist(&resp, &["members"]);
            let mut out = Vec::with_capacity(members.len());
            for m in members {
                let mut obj = BTreeMap::new();
                obj.insert(
                    "userId".to_string(),
                    Value::String(js(m, &["user_id", "userId", "id", "user.id"])),
                );
                let display_name = js(m, &["display_name", "displayName", "user.display_name"]);
                obj.insert(
                    "displayName".to_string(),
                    Value::String(if display_name.is_empty() {
                        "Unknown".to_string()
                    } else {
                        display_name
                    }),
                );
                obj.insert(
                    "lastReadAt".to_string(),
                    Value::Float64(jnum(m, &["last_read_at", "lastReadAt"])),
                );
                out.push(Value::Object(obj));
            }
            ok(Value::Array(out))
        }
        // Degradation: no support-DM bypass endpoint on the new API.
        ("conversations", "openSupportDm") => err_msg("Not supported yet"),

        // ---------- messages ----------
        ("messages", "list") => {
            let conversation_id = arg_str(&args, "conversationId");
            if conversation_id.is_empty() {
                return Err(ApiError("messages:list missing conversationId".to_string()));
            }
            // Legacy defaults: limit 100, optional `before` ms cursor.
            let limit = arg_f64(&args, "limit").unwrap_or(100.0).clamp(1.0, 100.0) as i64;
            let mut path = format!("/conversations/{conversation_id}/messages?limit={limit}");
            if let Some(before) = arg_f64(&args, "before") {
                path.push_str(&format!("&before={}", before as i64));
            }
            let resp = c.rest(Method::GET, &path, None).await?;
            let items = jlist(&resp, &["messages", "items"]);
            remember_reactions(items);
            // REST returns newest-first; the legacy client renders
            // oldest-first (Convex reversed before returning).
            let out: Vec<Value> = items
                .iter()
                .rev()
                .map(|m| message_value(c, m))
                .collect();
            ok(Value::Array(out))
        }
        ("messages", "listPinned") => {
            let conversation_id = arg_str(&args, "conversationId");
            if conversation_id.is_empty() {
                return Err(ApiError(
                    "messages:listPinned missing conversationId".to_string(),
                ));
            }
            // No dedicated pins endpoint: scan the recent history page and
            // filter client-side (best effort — pins older than the last 100
            // messages are missed; noted in the migration report).
            let path = format!("/conversations/{conversation_id}/messages?limit=100");
            let resp = c.rest(Method::GET, &path, None).await?;
            let items = jlist(&resp, &["messages", "items"]);
            let mut out = Vec::new();
            for m in items {
                if !jb(m, &["pinned"]) || jb(m, &["deleted"]) {
                    continue;
                }
                if js(m, &["kind"]) == "call" {
                    continue;
                }
                let encrypted = jb(m, &["encrypted"]);
                let body = js(m, &["body"]);
                // Legacy snippet rule: encrypted blobs go out whole (client
                // decrypts + truncates), plaintext truncates at 80 chars.
                let snippet = if encrypted { body } else { truncate80(&body) };
                let mut obj = BTreeMap::new();
                obj.insert(
                    "id".to_string(),
                    Value::String(js(m, &["id", "message_id"])),
                );
                obj.insert(
                    "authorId".to_string(),
                    Value::String(js(m, &["author_id", "authorId", "author.id"])),
                );
                obj.insert(
                    "authorName".to_string(),
                    Value::String(js(m, &["author_name", "authorName", "author.display_name"])),
                );
                obj.insert("snippet".to_string(), Value::String(snippet));
                obj.insert("encrypted".to_string(), Value::Boolean(encrypted));
                obj.insert("pinned".to_string(), Value::Boolean(true));
                obj.insert(
                    "sentAt".to_string(),
                    Value::Float64(jnum(m, &["sent_at", "created_at", "sentAt"])),
                );
                out.push(Value::Object(obj));
            }
            if out.len() > 50 {
                out.truncate(50); // legacy MAX_PINS_PER_CONVERSATION
            }
            ok(Value::Array(out))
        }
        ("messages", "send") => {
            let conversation_id = arg_str(&args, "conversationId");
            let body_text = arg_str(&args, "body");
            // Legacy arg `attachmentStorageId` now carries the /files key and
            // maps to the REST `attachmentKey` field.
            let attachment_key = arg_opt_str(&args, "attachmentStorageId");
            let encrypted = arg_bool(&args, "encrypted").unwrap_or(false);
            // Legacy no-op: empty plaintext body without an attachment was
            // dropped client-side by Convex (`return null`).
            if !encrypted && body_text.trim().is_empty() && attachment_key.is_none() {
                return ok_null();
            }
            let mut payload = serde_json::Map::new();
            payload.insert("body".to_string(), json!(body_text));
            if let Some(key) = attachment_key {
                payload.insert("attachmentKey".to_string(), json!(key));
            }
            if let Some(reply_id) = arg_opt_str(&args, "replyToMessageId") {
                payload.insert("replyTo".to_string(), json!(reply_id));
            }
            if encrypted {
                payload.insert("encrypted".to_string(), json!(true));
            }
            let mention_ids = arg_str_list(&args, "mentionUserIds");
            if !mention_ids.is_empty() {
                payload.insert("mentionUserIds".to_string(), json!(mention_ids));
            }
            if arg_bool(&args, "mentionEveryone") == Some(true) {
                payload.insert("mentionEveryone".to_string(), json!(true));
            }
            match c
                .rest(
                    Method::POST,
                    &format!("/conversations/{conversation_id}/messages"),
                    Some(serde_json::Value::Object(payload)),
                )
                .await
            {
                // Legacy result was {stored:true} / null; call sites only
                // require success (`expect_null` accepts any value).
                Ok(_) => ok_null(),
                // "Message is too long", permission errors — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("messages", "edit") => {
            let message_id = arg_str(&args, "messageId");
            let body = json!({ "body": arg_str(&args, "body") });
            match c
                .rest(Method::PATCH, &format!("/messages/{message_id}"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                // 403 not-author / 410 deleted — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("messages", "remove") => {
            let message_id = arg_str(&args, "messageId");
            match c
                .rest(Method::DELETE, &format!("/messages/{message_id}"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("messages", "pinMessage") => {
            let message_id = arg_str(&args, "messageId");
            match c
                .rest(Method::POST, &format!("/messages/{message_id}/pin"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("messages", "unpinMessage") => {
            let message_id = arg_str(&args, "messageId");
            match c
                .rest(Method::DELETE, &format!("/messages/{message_id}/pin"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("messages", "toggleReaction") => {
            let message_id = arg_str(&args, "messageId");
            let emoji = arg_str(&args, "emoji");
            if message_id.is_empty() || emoji.is_empty() {
                return Err(ApiError(
                    "messages:toggleReaction missing messageId/emoji".to_string(),
                ));
            }
            // The new API only has idempotent add/remove; the toggle decision
            // rides on the reacted_by_me cache fed by `messages:list`.
            let key = (message_id.clone(), emoji.clone());
            let reacted = reaction_cache()
                .lock()
                .ok()
                .and_then(|cache| cache.get(&key).copied())
                .unwrap_or(false);
            let method = if reacted { Method::DELETE } else { Method::PUT };
            let path = format!("/messages/{message_id}/reactions/{}", pct_encode(&emoji));
            match c.rest(method, &path, None).await {
                Ok(_) => {
                    if let Ok(mut cache) = reaction_cache().lock() {
                        cache.insert(key, !reacted);
                    }
                    ok_null()
                }
                Err(err) => human_or(err),
            }
        }
        // ---------- degradations (per migration table) ----------
        ("messages", "search") => err_msg("Search not available yet"),
        ("messages", "purge")
        | ("messages", "clearConversation")
        | ("messages", "purgeAllHistory") => err_msg("Not supported yet"),
        // No edit-history endpoint — query degrades to an empty trail.
        ("messages", "listEditHistory") => ok(Value::Array(Vec::new())),
        // Uploads go through `ApiClient::upload_file` now; call sites no
        // longer call this path.
        ("messages", "generateAttachmentUploadUrl") => err_msg("Not supported yet"),

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

fn err_msg(msg: &str) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::ErrorMessage(msg.to_string()))
}

/// Direct/group creation returns a conversation record; the legacy result
/// was the bare conversation id string (`expect_string` at the call sites).
fn conversation_id_result(resp: &serde_json::Value) -> Result<FunctionResult, ApiError> {
    let id = js(
        resp,
        &[
            "id",
            "conversation_id",
            "conversationId",
            "conversation.id",
            "conversation.conversation_id",
        ],
    );
    if id.is_empty() {
        return Err(ApiError("Unexpected server response".to_string()));
    }
    ok(Value::String(id))
}

// ---------- message object builder ----------

/// Rebuild one REST message row (snake_case) into the camelCase object the
/// legacy `messages:list` parser expects. Deleted messages keep the new
/// API's tombstone shape (`body: ""` + `deleted: true`) — passed through
/// as-is, exactly like the old "Message deleted" masking relied on the
/// `deleted` flag.
fn message_value(c: &ApiClient, m: &serde_json::Value) -> Value {
    let mut obj = BTreeMap::new();
    obj.insert(
        "id".to_string(),
        Value::String(js(m, &["id", "message_id"])),
    );
    obj.insert(
        "authorId".to_string(),
        Value::String(js(m, &["author_id", "authorId", "author.id"])),
    );
    obj.insert(
        "authorName".to_string(),
        Value::String(js(m, &["author_name", "authorName", "author.display_name"])),
    );
    obj.insert(
        "authorAvatarColor".to_string(),
        Value::String(js(
            m,
            &["author_avatar_color", "authorAvatarColor", "author.avatar_color"],
        )),
    );
    obj.insert(
        "authorAvatarImageUrl".to_string(),
        Value::String(avatar_url(
            c,
            m,
            &["author_avatar_url", "authorAvatarImageUrl"],
            &[
                "author_avatar_storage_key",
                "author_avatar_key",
                "author.avatar_storage_key",
            ],
        )),
    );
    obj.insert(
        "authorIsBot".to_string(),
        Value::Boolean(jb(m, &["author_is_bot", "authorIsBot", "author.is_bot"])),
    );
    // Plus doesn't exist on the new API; the parser defaults to false.
    obj.insert("authorPlusActive".to_string(), Value::Boolean(false));
    obj.insert("body".to_string(), Value::String(js(m, &["body"])));
    let kind = js(m, &["kind"]);
    obj.insert(
        "kind".to_string(),
        Value::String(if kind.is_empty() {
            "text".to_string()
        } else {
            kind
        }),
    );
    obj.insert(
        "encrypted".to_string(),
        Value::Boolean(jb(m, &["encrypted"])),
    );
    obj.insert(
        "attachmentUrl".to_string(),
        Value::String(avatar_url(
            c,
            m,
            &["attachment_url", "attachmentUrl"],
            &["attachment_key", "attachmentKey", "attachment_storage_key"],
        )),
    );

    let mut reactions = Vec::new();
    if let Some(arr) = get_path(m, "reactions").and_then(|x| x.as_array()) {
        for r in arr {
            let mut robj = BTreeMap::new();
            robj.insert("emoji".to_string(), Value::String(js(r, &["emoji"])));
            robj.insert(
                "count".to_string(),
                Value::Float64(jnum(r, &["count"])),
            );
            robj.insert(
                "reactedByMe".to_string(),
                Value::Boolean(jb(r, &["reacted_by_me", "reactedByMe"])),
            );
            reactions.push(Value::Object(robj));
        }
    }
    obj.insert("reactions".to_string(), Value::Array(reactions));

    // replyTo: embedded object (author_name/snippet/encrypted) or flat
    // reply_to_* fields; omitted entirely when the server sends nothing
    // (the parser treats a missing key like the legacy `null`).
    let mut reply: Option<BTreeMap<String, Value>> = None;
    for key in ["reply_to", "replyTo"] {
        if let Some(rt) = get_path(m, key).filter(|x| x.is_object()) {
            let mut robj = BTreeMap::new();
            robj.insert(
                "authorName".to_string(),
                Value::String(js(rt, &["author_name", "authorName"])),
            );
            robj.insert(
                "snippet".to_string(),
                Value::String(js(rt, &["snippet", "body"])),
            );
            robj.insert(
                "encrypted".to_string(),
                Value::Boolean(jb(rt, &["encrypted"])),
            );
            reply = Some(robj);
            break;
        }
    }
    if reply.is_none() {
        let author = js(m, &["reply_to_author_name"]);
        let snippet = js(m, &["reply_to_snippet"]);
        if !author.is_empty() || !snippet.is_empty() {
            let mut robj = BTreeMap::new();
            robj.insert("authorName".to_string(), Value::String(author));
            robj.insert("snippet".to_string(), Value::String(snippet));
            robj.insert(
                "encrypted".to_string(),
                Value::Boolean(jb(m, &["reply_to_encrypted"])),
            );
            reply = Some(robj);
        }
    }
    if let Some(robj) = reply {
        obj.insert("replyTo".to_string(), Value::Object(robj));
    }

    obj.insert(
        "deleted".to_string(),
        Value::Boolean(jb(m, &["deleted"])),
    );
    let edited = jb(m, &["edited"])
        || get_path(m, "edited_at")
            .map(|x| !x.is_null())
            .unwrap_or(false);
    obj.insert("edited".to_string(), Value::Boolean(edited));
    obj.insert("pinned".to_string(), Value::Boolean(jb(m, &["pinned"])));
    obj.insert(
        "sentAt".to_string(),
        Value::Float64(jnum(m, &["sent_at", "created_at", "sentAt"])),
    );
    Value::Object(obj)
}

/// Resolve a display URL: a direct URL field wins; otherwise a storage key
/// is expanded through `ApiClient::file_url`. Empty = none.
fn avatar_url(
    c: &ApiClient,
    v: &serde_json::Value,
    url_keys: &[&str],
    storage_keys: &[&str],
) -> String {
    let direct = js(v, url_keys);
    if !direct.is_empty() {
        return direct;
    }
    let key = js(v, storage_keys);
    if key.is_empty() {
        String::new()
    } else {
        c.file_url(&key)
    }
}

// ---------- reaction toggle cache ----------

/// Process-local `reacted_by_me` per (messageId, emoji), populated from
/// every `messages:list` page. `toggleReaction` consults it to choose
/// between PUT (add) and DELETE (remove). A stale entry self-corrects: the
/// verbs are idempotent, so one extra click after a WS refetch converges.
fn reaction_cache() -> &'static Mutex<HashMap<(String, String), bool>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_reactions(items: &[serde_json::Value]) {
    let Ok(mut cache) = reaction_cache().lock() else {
        return;
    };
    for m in items {
        let id = js(m, &["id", "message_id"]);
        if id.is_empty() {
            continue;
        }
        if let Some(arr) = get_path(m, "reactions").and_then(|x| x.as_array()) {
            for r in arr {
                let emoji = js(r, &["emoji"]);
                if emoji.is_empty() {
                    continue;
                }
                cache.insert(
                    (id.clone(), emoji),
                    jb(r, &["reacted_by_me", "reactedByMe"]),
                );
            }
        }
    }
    // Crude bound — a long session shouldn't grow this without limit.
    if cache.len() > 10_000 {
        cache.clear();
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

fn arg_f64(args: &BTreeMap<String, Value>, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Float64(f)) => Some(*f),
        Some(Value::Int64(i)) => Some(*i as f64),
        _ => None,
    }
}

fn arg_str_list(args: &BTreeMap<String, Value>, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------- JSON helpers (string-tolerant; pg BIGINTs arrive as text) ----------

/// Dotted-path lookup (`"conversation.id"` walks nested objects).
fn get_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn js(v: &serde_json::Value, paths: &[&str]) -> String {
    for p in paths {
        if let Some(x) = get_path(v, p) {
            match x {
                serde_json::Value::String(s) => return s.clone(),
                serde_json::Value::Number(n) => return n.to_string(),
                _ => {}
            }
        }
    }
    String::new()
}

fn jnum(v: &serde_json::Value, paths: &[&str]) -> f64 {
    for p in paths {
        if let Some(x) = get_path(v, p) {
            match x {
                serde_json::Value::Number(n) => return n.as_f64().unwrap_or(0.0),
                serde_json::Value::String(s) => {
                    if let Ok(f) = s.parse::<f64>() {
                        return f;
                    }
                }
                _ => {}
            }
        }
    }
    0.0
}

fn jb(v: &serde_json::Value, paths: &[&str]) -> bool {
    for p in paths {
        if let Some(x) = get_path(v, p) {
            match x {
                serde_json::Value::Bool(b) => return *b,
                serde_json::Value::Number(n) => return n.as_i64().unwrap_or(0) != 0,
                _ => {}
            }
        }
    }
    false
}

fn jopt_bool(v: &serde_json::Value, path: &str) -> Option<bool> {
    match get_path(v, path) {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// A list response may be a bare array or wrapped (`{messages: [...]}` /
/// `{conversations: [...]}` / `{members: [...]}`); accept all forms.
fn jlist<'a>(v: &'a serde_json::Value, keys: &[&str]) -> &'a [serde_json::Value] {
    if let Some(arr) = v.as_array() {
        return arr;
    }
    for k in keys {
        if let Some(arr) = v.get(k).and_then(|x| x.as_array()) {
            return arr;
        }
    }
    &[]
}

// ---------- misc ----------

/// Legacy pinned-snippet rule: plaintext truncates at 80 chars + "..."
/// (char-boundary safe; JS sliced code units, chars are close enough for
/// display purposes and never split a codepoint).
fn truncate80(body: &str) -> String {
    if body.chars().count() > 80 {
        let cut: String = body.chars().take(80).collect();
        format!("{cut}...")
    } else {
        body.to_string()
    }
}

/// Percent-encode a path segment (emoji in the reactions route are raw
/// UTF-8 and must be encoded before hitting reqwest).
fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// mutations surfaced 4xx bodies as user-facing messages ("You can't
/// message this user", "Message is too long", ...), so downgrade those to
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
