//! `admin:*`, `reports:*`, `bots:*`, `plus:*` — none of these endpoints
//! exist on the new API, so every path degrades per the migration table:
//! empty-but-well-formed payloads for queries the UI polls (it should show
//! an empty state, not crash), and human-readable `ErrorMessage`s for
//! mutations so the user gets feedback instead of a silent no-op.

use std::collections::BTreeMap;

use super::client::{ApiClient, ApiError};
use super::value::{FunctionResult, Value};

fn err(msg: &str) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::ErrorMessage(msg.to_string()))
}

fn ok(value: Value) -> Result<FunctionResult, ApiError> {
    Ok(FunctionResult::Value(value))
}

pub async fn dispatch(
    _c: &ApiClient,
    module: &str,
    name: &str,
    _args: BTreeMap<String, Value>,
) -> Result<FunctionResult, ApiError> {
    match (module, name) {
        // ---------- admin ----------
        // Polled by `admin_users_subscription` via `parse_object_array`:
        // rows of {userId, username, displayName, role, banned}. Empty
        // array renders an empty staff table.
        ("admin", "listUsers") => ok(Value::Array(vec![])),
        // `parse_admin_stats` wants an object; missing numeric keys parse
        // as 0, so a zero-filled object shows the stats card with zeros.
        ("admin", "adminStats") => ok(Value::Object(BTreeMap::from([
            ("totalUsers".to_string(), Value::Float64(0.0)),
            ("online".to_string(), Value::Float64(0.0)),
            ("banned".to_string(), Value::Float64(0.0)),
            ("staff".to_string(), Value::Float64(0.0)),
            ("bots".to_string(), Value::Float64(0.0)),
            ("servers".to_string(), Value::Float64(0.0)),
        ]))),
        // `parse_admin_user_detail` maps ErrorMessage to `None`, so the
        // detail panel simply stays closed instead of showing a fake user.
        ("admin", "adminUserDetail") => err("Admin panel not available yet"),
        ("admin", "setRole")
        | ("admin", "setBanned")
        | ("admin", "adminRevokeSessions") => err("Admin panel not available yet"),

        // ---------- reports ----------
        ("reports", "reportMessage")
        | ("reports", "adminListReports")
        | ("reports", "adminResolveReport") => err("Reports not available yet"),

        // ---------- bots ----------
        // `bots:listMine` is parsed by `parse_object_array` → empty list.
        ("bots", "listMine") => ok(Value::Array(vec![])),
        ("bots", "create")
        | ("bots", "inviteToServer")
        | ("bots", "regenerateToken")
        | ("bots", "destroy") => err("Bots not available yet"),

        // ---------- plus ----------
        // update.rs (PlusRefreshStatus) reads `active` via value_as_bool and
        // `expiresAt` via obj_ms. The old Convex handler returned
        // `expiresAt: 0` when inactive, so mirror that exactly (Float64) —
        // "no Plus" in the precise shape the caller parses.
        ("plus", "getMyStatus") => ok(Value::Object(BTreeMap::from([
            ("active".to_string(), Value::Boolean(false)),
            ("expiresAt".to_string(), Value::Float64(0.0)),
        ]))),
        ("plus", "createBillingPortal") => {
            err("Billing portal not available yet — visit https://buy.vyrapp.pro")
        }

        // ---------- module catch-alls ----------
        // Degradation table covers `admin:*`, `reports:*`, `bots:*`, `plus:*`
        // wholesale; any function not enumerated above (e.g. bots:rename,
        // plus:adminGrant) gets the same human-readable message instead of a
        // hard "unmapped path" transport error.
        ("admin", _) => err("Admin panel not available yet"),
        ("reports", _) => err("Reports not available yet"),
        ("bots", _) => err("Bots not available yet"),
        ("plus", _) => err("Plus not available yet — visit https://buy.vyrapp.pro"),

        _ => Err(ApiError(format!("unmapped path {module}:{name}"))),
    }
}
