//! Own-backend API adapter (drop-in replacement for the `convex` crate).
//!
//! `client` exposes `ApiClient` with the same call surface the app used on
//! `ConvexClient`; `value` mirrors `Value`/`FunctionResult`; the
//! `dispatch_*` modules translate each `"module:name"` path into REST calls
//! against api.vyrapp.pro; `ws` runs the live-update socket.

pub mod client;
pub mod value;

mod dispatch_auth;
mod dispatch_conv;
mod dispatch_friends;
mod dispatch_media;
mod dispatch_misc;
mod dispatch_profile;
mod dispatch_servers;
mod ws;

pub use client::{ApiClient, ApiError, WsEvent};
pub use value::{FunctionResult, Value};
