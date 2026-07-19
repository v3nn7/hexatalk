//! Application state and the update loop: the `App` state machine, the
//! `Message` events driving it, shared domain types, and local persistence
//! (session token, user settings, encrypted history vault).

pub(crate) mod app;
pub(crate) mod history;
pub(crate) mod message;
pub(crate) mod session_store;
pub(crate) mod settings_store;
pub(crate) mod types;
pub(crate) mod update;
