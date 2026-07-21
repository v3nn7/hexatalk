//! Path dispatch for the media domain: `calls:*`, `voice:*`, `peer:*`.
//!
//! Each Convex path is translated into a REST call against api.vyrapp.pro
//! (Bearer token is attached by `ApiClient::rest`; the legacy `sessionToken`
//! arg is ignored), and the snake_case JSON response is rebuilt into the
//! camelCase `Value` shape the old call sites expect:
//!
//! - `calls:startCall` → `Value::String(callId)` (src/media/call.rs ~1728)
//! - `calls:respond` / `calls:endCall` / `calls:addIceCandidate` → null
//! - `calls:listPeerIceCandidates` → `[{id, candidate}]` (call.rs ~1881)
//! - `calls:myCall` → degradation: null — call state arrives over WS
//!   (`call.*` events) and subscriptions build it locally.
//! - `voice:join` / `voice:leave` → null (update.rs discards the value and
//!   only uses the Err channel, so failures here stay `ApiError`, NOT
//!   `ErrorMessage` — otherwise a failed join would read as success).
//! - `voice:listInChannel` → `[{userId, displayName, joinedAt}]`
//!   (subscriptions.rs ~241, room_voice.rs ~277)
//! - `voice:publishOffer` → `Value::String(linkId)` (room_voice.rs ~665)
//! - `voice:publishAnswer` / `voice:endLink` / `voice:addLinkIce` → null
//! - `voice:listLinkIce` → `[{id, candidate}]` (room_voice.rs ~480)
//! - `voice:listMyLinks` → degradation: empty list — mesh link state is
//!   rebuilt from WS `voice.link.*` events by the subscriptions layer.
//! - `peer:publishInvite` → null (app.rs ~1122 goes through expect_null)
//! - `peer:getInvite` → `{hostUserId, invitePayload, expiresAt, isHost}`
//!   or null (subscriptions.rs ~1031)
//! - `peer:clearInvite` → no DELETE endpoint on the new API: no-op null.

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
        // ---------- calls (1:1 WebRTC signalling) ----------
        ("calls", "startCall") => {
            let body = json!({
                "conversationId": arg_str(&args, "conversationId"),
                "calleeId": arg_str(&args, "calleeId"),
                "offerSdp": arg_str(&args, "offerSdp"),
            });
            let resp = match c.rest(Method::POST, "/calls", Some(body)).await {
                Ok(resp) => resp,
                // "You're already in a call" / "You can't call this user" —
                // call.rs surfaces these on the call banner.
                Err(err) => return human_or(err),
            };
            // 201 returns the created call; be liberal about the envelope.
            let id = jstr(&resp, "id");
            let id = if id.is_empty() {
                jstr(&resp, "call_id")
            } else {
                id
            };
            let id = if id.is_empty() {
                resp.get("call").map(|v| jstr(v, "id")).unwrap_or_default()
            } else {
                id
            };
            if id.is_empty() {
                return Ok(FunctionResult::ErrorMessage(
                    "Could not start call".to_string(),
                ));
            }
            Ok(FunctionResult::Value(Value::String(id)))
        }
        ("calls", "respond") => {
            let call_id = arg_str(&args, "callId");
            let accept = arg_bool(&args, "accept").unwrap_or(false);
            let result = if accept {
                let body = json!({ "answerSdp": arg_str(&args, "answerSdp") });
                c.rest(Method::POST, &format!("/calls/{call_id}/answer"), Some(body))
                    .await
            } else {
                c.rest(Method::POST, &format!("/calls/{call_id}/decline"), None)
                    .await
            };
            match result {
                Ok(_) => ok_null(),
                // "Call not found" / "Missing answer" — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        ("calls", "endCall") => {
            let call_id = arg_str(&args, "callId");
            match c
                .rest(Method::POST, &format!("/calls/{call_id}/end"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("calls", "addIceCandidate") => {
            let call_id = arg_str(&args, "callId");
            let body = json!({ "candidate": arg_str(&args, "candidate") });
            match c
                .rest(Method::POST, &format!("/calls/{call_id}/ice"), Some(body))
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("calls", "listPeerIceCandidates") => {
            // The REST endpoint filters by `?from=<userId>`, but this query
            // only knows the callId — the peer's user id isn't available
            // here. Fetch without the filter (the old query returned all
            // non-self rows; the caller dedups by row id, so an occasional
            // own candidate is harmless), and degrade to an empty list on
            // any error — this is polled on a subscription tick and must
            // never push error noise.
            let call_id = arg_str(&args, "callId");
            match c
                .rest(Method::GET, &format!("/calls/{call_id}/ice"), None)
                .await
            {
                Ok(resp) => Ok(FunctionResult::Value(Value::Array(ice_rows(&resp)))),
                Err(_) => Ok(FunctionResult::Value(Value::Array(Vec::new()))),
            }
        }
        // Degradation: no "my active call" endpoint — null means "no active
        // call"; real state is rebuilt from WS call.* events.
        ("calls", "myCall") => Ok(FunctionResult::Value(Value::Null)),

        // ---------- voice (rooms + mesh links) ----------
        ("voice", "join") => {
            let conversation_id = arg_str(&args, "conversationId");
            // NOTE: no human_or here — update.rs only reads the Err channel
            // for join failures and discards the FunctionResult value, so an
            // ErrorMessage would be misread as a successful join.
            c.rest(
                Method::POST,
                &format!("/conversations/{conversation_id}/voice/join"),
                None,
            )
            .await?;
            ok_null()
        }
        ("voice", "leave") => {
            // `conversationId` is optional in the old API (None = leave every
            // room). There is no leave-all endpoint; leaving the rooms we're
            // told about is the best available mapping, and leave-all
            // degrades to a no-op (stale voice state is cleaned up by WS
            // voice.leave handling / server-side timeouts).
            if let Some(conversation_id) = arg_opt_str(&args, "conversationId") {
                if !conversation_id.is_empty() {
                    c.rest(
                        Method::POST,
                        &format!("/conversations/{conversation_id}/voice/leave"),
                        None,
                    )
                    .await?;
                }
            }
            ok_null()
        }
        ("voice", "listInChannel") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}/voice"),
                    None,
                )
                .await?;
            let participants = jarr(&resp, "participants");
            let rows = participants
                .iter()
                .map(|p| {
                    let mut obj = BTreeMap::new();
                    obj.insert("userId".to_string(), Value::String(jstr(p, "user_id")));
                    obj.insert(
                        "displayName".to_string(),
                        Value::String(jstr(p, "display_name")),
                    );
                    obj.insert(
                        "joinedAt".to_string(),
                        Value::Float64(jf64(p, "joined_at")),
                    );
                    Value::Object(obj)
                })
                .collect();
            Ok(FunctionResult::Value(Value::Array(rows)))
        }
        ("voice", "publishOffer") => {
            // The old API derived offerer/answerer from the lexicographic
            // order of the two user ids; room_voice.rs only calls this when
            // we're the offerer, so the peer is always the answerer.
            let body = json!({
                "conversationId": arg_str(&args, "conversationId"),
                "answererId": arg_str(&args, "peerId"),
                "offerSdp": arg_str(&args, "offerSdp"),
            });
            let resp = match c.rest(Method::POST, "/voice-links", Some(body)).await {
                Ok(resp) => resp,
                // "Both users must be in the voice room" etc. — surfaced as a
                // room voice status line.
                Err(err) => return human_or(err),
            };
            let id = jstr(&resp, "id");
            let id = if id.is_empty() {
                jstr(&resp, "link_id")
            } else {
                id
            };
            let id = if id.is_empty() {
                resp.get("link").map(|v| jstr(v, "id")).unwrap_or_default()
            } else {
                id
            };
            if id.is_empty() {
                return Ok(FunctionResult::ErrorMessage(
                    "Could not publish voice offer".to_string(),
                ));
            }
            Ok(FunctionResult::Value(Value::String(id)))
        }
        ("voice", "publishAnswer") => {
            let link_id = arg_str(&args, "linkId");
            let body = json!({ "answerSdp": arg_str(&args, "answerSdp") });
            match c
                .rest(
                    Method::POST,
                    &format!("/voice-links/{link_id}/answer"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("voice", "endLink") => {
            let link_id = arg_str(&args, "linkId");
            match c
                .rest(Method::POST, &format!("/voice-links/{link_id}/end"), None)
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        ("voice", "addLinkIce") => {
            let link_id = arg_str(&args, "linkId");
            let body = json!({ "candidate": arg_str(&args, "candidate") });
            match c
                .rest(
                    Method::POST,
                    &format!("/voice-links/{link_id}/ice"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                Err(err) => human_or(err),
            }
        }
        // Degradation: no "list my links" endpoint — empty list; the
        // subscriptions layer rebuilds mesh link state from WS
        // voice.link.offer/answer/ice/end events.
        ("voice", "listMyLinks") => Ok(FunctionResult::Value(Value::Array(Vec::new()))),
        ("voice", "listLinkIce") => {
            // Polled every 400 ms per active link — degrade errors to an
            // empty list instead of pushing error noise.
            let link_id = arg_str(&args, "linkId");
            match c
                .rest(Method::GET, &format!("/voice-links/{link_id}/ice"), None)
                .await
            {
                Ok(resp) => Ok(FunctionResult::Value(Value::Array(ice_rows(&resp)))),
                Err(_) => Ok(FunctionResult::Value(Value::Array(Vec::new()))),
            }
        }

        // ---------- peer (peerseal invites) ----------
        ("peer", "publishInvite") => {
            let conversation_id = arg_str(&args, "conversationId");
            let body = json!({
                "invitePayload": arg_str(&args, "invitePayload"),
                "expiresAt": arg_f64(&args, "expiresAt") as i64,
            });
            match c
                .rest(
                    Method::POST,
                    &format!("/conversations/{conversation_id}/peer-invites"),
                    Some(body),
                )
                .await
            {
                Ok(_) => ok_null(),
                // "Invalid invite payload" etc. — user-facing copy.
                Err(err) => human_or(err),
            }
        }
        // Degradation: no DELETE for peer invites — the invite simply
        // expires server-side. No-op so the UI flow can continue.
        ("peer", "clearInvite") => ok_null(),
        ("peer", "getInvite") => {
            let conversation_id = arg_str(&args, "conversationId");
            let resp = c
                .rest(
                    Method::GET,
                    &format!("/conversations/{conversation_id}/peer-invites"),
                    None,
                )
                .await?;
            // Non-expired invites only; at most one invite exists per
            // conversation (a republish replaces the previous row).
            let invites = jarr(&resp, "invites");
            let Some(invite) = invites.first() else {
                return Ok(FunctionResult::Value(Value::Null));
            };
            let host_user_id = jstr(invite, "host_user_id");
            // `isHost` needs our own user id, which this query doesn't
            // carry — fetch it. Only happens when an invite actually
            // exists, so the common polling path stays a single request.
            let my_id = match c.rest(Method::GET, "/users/me", None).await {
                Ok(me) => me
                    .get("user")
                    .map(|u| jstr(u, "id"))
                    .unwrap_or_else(|| jstr(&me, "id")),
                Err(_) => String::new(),
            };
            let mut obj = BTreeMap::new();
            obj.insert(
                "hostUserId".to_string(),
                Value::String(host_user_id.clone()),
            );
            obj.insert(
                "invitePayload".to_string(),
                Value::String(jstr(invite, "invite_payload")),
            );
            obj.insert(
                "expiresAt".to_string(),
                Value::Float64(jf64(invite, "expires_at")),
            );
            obj.insert(
                "isHost".to_string(),
                Value::Boolean(!my_id.is_empty() && my_id == host_user_id),
            );
            Ok(FunctionResult::Value(Value::Object(obj)))
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

fn arg_f64(args: &BTreeMap<String, Value>, key: &str) -> f64 {
    match args.get(key) {
        Some(Value::Float64(f)) => *f,
        Some(Value::Int64(i)) => *i as f64,
        _ => 0.0,
    }
}

// ---------- JSON helpers ----------

fn jstr(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn jf64(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// Extract an array from a response that may be `{<key>: [...]}` or a bare
/// `[...]` — the reference documents envelopes, but be liberal.
fn jarr<'a>(v: &'a serde_json::Value, key: &str) -> &'a [serde_json::Value] {
    if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
        return arr;
    }
    if let Some(arr) = v.as_array() {
        return arr;
    }
    &[]
}

/// Map ICE candidate rows (`{id, from_user_id, candidate, ...}`) into the
/// `[{id, candidate}]` shape `call.rs` / `room_voice.rs` consume.
fn ice_rows(resp: &serde_json::Value) -> Vec<Value> {
    jarr(resp, "candidates")
        .iter()
        .map(|row| {
            let mut obj = BTreeMap::new();
            obj.insert("id".to_string(), Value::String(jstr(row, "id")));
            obj.insert(
                "candidate".to_string(),
                Value::String(jstr(row, "candidate")),
            );
            Value::Object(obj)
        })
        .collect()
}

/// `rest()` reports non-2xx as `"{status}: {error}"`. The old Convex
/// mutations surfaced 4xx bodies as user-facing messages (e.g. "You're
/// already in a call"), so downgrade those to `ErrorMessage` for the UI;
/// anything else stays a hard error.
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
