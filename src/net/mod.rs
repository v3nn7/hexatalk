//! Networking: the own-backend API adapter (`api` — REST + WebSocket in
//! place of the old Convex client), query result parsing, live
//! subscriptions, the task runtime plumbing (`rt`), and the peerseal E2EE
//! DM bridge.

pub mod api;
pub(crate) mod convex_parse;
pub(crate) mod peer;
pub(crate) mod rt;
pub(crate) mod subscriptions;
