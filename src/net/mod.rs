//! Networking: the task/subscription runtime plumbing (`rt`), Convex query
//! result parsing, live Convex subscriptions, and the peerseal E2EE DM
//! bridge. Convex carries signaling/state; message bodies go peer-to-peer.

pub(crate) mod convex_parse;
pub(crate) mod peer;
pub(crate) mod rt;
pub(crate) mod subscriptions;
