//! The state machine: `App::update`, one giant `match` over every
//! `Message` variant. This is the single largest function in the codebase
//! by a wide margin -- it's kept as one function (rather than split further)
//! because Rust can't split a single `match` expression's arms across
//! files, and restructuring the dispatch into per-feature helper methods
//! is a real behavioral refactor, not just a file move.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use convex::{FunctionResult, Value};
use maplit::btreemap;

use crate::net::rt::write_clipboard_text;
use crate::net::rt::{Task, WindowAction};

use crate::{AVATAR_PALETTE, PEER_CLEAR_HISTORY_CTRL, scroll_chat_to_bottom};

use crate::crypto;
use crate::media::call;
use crate::media::notify::{notify_desktop, ringtone_start, ringtone_stop};
use crate::media::screenshare;
use crate::net::convex_parse::{
    expect_null, expect_string, humanize_error, obj_f64, obj_str, obj_str_list, parse_admin_stats,
    parse_admin_user_detail, parse_clear_conversation_result, parse_me, parse_message_reports,
    parse_object_array, parse_profile_view, parse_server_stats, parse_session, value_as_bool,
};
use crate::net::peer;
use crate::net::subscriptions::{mark_read_task, typing_ping_task};
use crate::state::app::App;
use crate::state::message::Message;
use crate::state::session_store::{
    clear_session_file, connect_task, save_panel_prefs, save_session_to_disk, hexatalk_data_dir,
};
use crate::state::types::{
    AttachmentPick, AuthMode, AvatarPick, BotSummary, CallRole, PendingAttachment, PeopleHit,
    ResizePanel, ServerSettingsCategory, SettingsCategory, SidebarTab,
};
use crate::tray;
use crate::ui::mentions;
use crate::ui::utils::{next_friend_request_privacy, next_presence_status};
use crate::update_check::{UpdateOutcome, check_for_update_task, stage_exe_swap};

/// `hexatalk://invite/<code>` -- shareable alongside (or instead of) the bare
/// invite code; `extract_invite_code` accepts either form pasted back in.
fn build_invite_link(code: &str) -> String {
    format!("hexatalk://invite/{code}")
}

fn hostname_best_effort() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "device".into())
}

/// Pulls the invite code out of a pasted `hexatalk://invite/<code>` (or
/// `https://.../invite/<code>`) link, falling back to treating the whole
/// trimmed input as a bare code if it doesn't look like a link.
fn extract_invite_code(input: &str) -> String {
    let trimmed = input.trim();
    let code = match trimmed.rsplit_once("invite/") {
        Some((_, rest)) => rest,
        None => trimmed,
    };
    code.trim_matches('/').to_string()
}

impl App {
    fn move_channel_task(&self, conversation_id: String, direction: &'static str) -> Task<Message> {
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        let Some(session) = self.session.clone() else {
            return Task::none();
        };
        let mut client = client;
        Task::perform(
            async move {
                client
                    .mutation(
                        "channels:moveChannel",
                        btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                            "conversationId".to_string() => Value::String(conversation_id),
                            "direction".to_string() => Value::String(direction.into()),
                        },
                    )
                    .await
                    .map_err(|e| humanize_error(&e.to_string()))
                    .and_then(expect_null)
            },
            Message::MoveChannelFinished,
        )
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Connected(client) => {
                self.connect_status = "Connected to Convex".to_string();
                self.client = Some(client.clone());

                if let Some(token) = self.pending_restore_token.take() {
                    let mut client = client;
                    let token_for_result = token.clone();
                    return Task::perform(
                        async move {
                            client
                                .query(
                                    "auth:me",
                                    btreemap! {
                                        "sessionToken".to_string() => Value::String(token),
                                    },
                                )
                                .await
                                .map_err(|err| err.to_string())
                                .and_then(|result| parse_me(result, token_for_result))
                        },
                        Message::RestoreFinished,
                    );
                }
                Task::none()
            }
            Message::ConnectFailed(err) => {
                self.connect_status = format!("Connection error: {err}");
                self.auth_error = Some(format!(
                    "Can't reach the server. Check your network and try again.\n({err})"
                ));
                Task::none()
            }
            Message::RetryConnect => {
                if self.client.is_some() {
                    self.connect_status = "Already connected".to_string();
                    return Task::none();
                }
                self.connect_status = "Reconnecting…".to_string();
                self.auth_error = None;
                connect_task(self.deployment_url.clone())
            }

            Message::SwitchAuthMode(mode) => {
                self.auth_mode = mode;
                self.auth_error = None;
                Task::none()
            }
            Message::UsernameInputChanged(value) => {
                // Keep local typing free, but strip accidental leading spaces.
                self.username_input = value.trim_start().to_string();
                if self.auth_error.is_some() {
                    self.auth_error = None;
                }
                Task::none()
            }
            Message::PasswordInputChanged(value) => {
                self.password_input = value;
                if self.auth_error.is_some() {
                    self.auth_error = None;
                }
                Task::none()
            }
            Message::DisplayNameInputChanged(value) => {
                self.display_name_input = value;
                Task::none()
            }
            Message::EmailInputChanged(value) => {
                self.email_input = value;
                if self.auth_error.is_some() {
                    self.auth_error = None;
                }
                Task::none()
            }
            Message::SubmitAuth => {
                let Some(client) = self.client.clone() else {
                    self.auth_error =
                        Some("Still connecting… wait a second, or press Retry.".to_string());
                    return Task::none();
                };
                if self.auth_busy {
                    return Task::none();
                }
                let username = self.username_input.trim().to_lowercase();
                let password = self.password_input.clone();
                if username.is_empty() || password.is_empty() {
                    self.auth_error = Some("Enter a username and password".to_string());
                    return Task::none();
                }
                if self.auth_mode == AuthMode::Register {
                    if username.len() < 3 {
                        self.auth_error =
                            Some("Username must be at least 3 characters".to_string());
                        return Task::none();
                    }
                    if !username
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    {
                        self.auth_error =
                            Some("Username can only use letters, numbers, _ and -".to_string());
                        return Task::none();
                    }
                    if password.len() < 6 {
                        self.auth_error =
                            Some("Password must be at least 6 characters".to_string());
                        return Task::none();
                    }
                    let email = self.email_input.trim();
                    if email.is_empty() || !email.contains('@') || !email.contains('.') {
                        self.auth_error = Some("Enter a valid email address".to_string());
                        return Task::none();
                    }
                }
                self.auth_busy = true;
                self.auth_error = None;
                self.username_input = username.clone();

                let mut client = client;
                let display_name = {
                    let dn = self.display_name_input.trim();
                    if dn.is_empty() {
                        username.clone()
                    } else {
                        dn.to_string()
                    }
                };
                let function_name = match self.auth_mode {
                    AuthMode::Login => "auth:signIn",
                    AuthMode::Register => "auth:signUp",
                };
                let mut args = btreemap! {
                    "username".to_string() => Value::String(username),
                    "password".to_string() => Value::String(password),
                };
                if self.auth_mode == AuthMode::Register {
                    args.insert("displayName".to_string(), Value::String(display_name));
                    args.insert(
                        "email".to_string(),
                        Value::String(self.email_input.trim().to_lowercase()),
                    );
                }

                Task::perform(
                    async move {
                        client
                            .action(function_name, args)
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(parse_session)
                    },
                    Message::AuthFinished,
                )
            }
            Message::AuthFinished(Ok(session)) => {
                self.auth_busy = false;
                self.password_input.clear();
                self.email_input.clear();
                self.auth_error = None;
                save_session_to_disk(&session);
                let avatar_url = session.avatar_image_url.clone();
                let touch_token = session.token.clone();
                self.email_verify_input = session.email.clone();
                self.email_verify_code_sent = !session.email.is_empty();
                self.session = Some(session);
                self.show_toast("Signed in");
                let touch = if let Some(client) = self.client.clone() {
                    let mut client = client;
                    let device = format!("{} · {}", std::env::consts::OS, hostname_best_effort());
                    Task::perform(
                        async move {
                            let _ = client
                                .mutation(
                                    "prefs:touchSession",
                                    btreemap! {
                                        "sessionToken".to_string() => Value::String(touch_token),
                                        "deviceName".to_string() => Value::String(device),
                                        "platform".to_string() => Value::String("desktop".into()),
                                    },
                                )
                                .await;
                        },
                        |_| Message::CallActionFinished(Ok(())),
                    )
                } else {
                    Task::none()
                };
                Task::batch([
                    self.fetch_missing_avatars(std::iter::once(avatar_url)),
                    self.ensure_identity_key(),
                    touch,
                ])
            }
            Message::AuthFinished(Err(err)) => {
                self.auth_busy = false;
                self.auth_error = Some(humanize_error(&err));
                Task::none()
            }
            Message::ChatFilterChanged(value) => {
                self.chat_filter_input = value;
                Task::none()
            }
            Message::FriendsFilterChanged(value) => {
                self.friends_filter_input = value;
                Task::none()
            }
            Message::ClearToast => {
                self.toast = None;
                Task::none()
            }
            Message::RestoreFinished(Ok(session)) => {
                let avatar_url = session.avatar_image_url.clone();
                self.email_verify_input = session.email.clone();
                self.email_verify_code_sent = !session.email.is_empty();
                self.session = Some(session);
                Task::batch([
                    self.fetch_missing_avatars(std::iter::once(avatar_url)),
                    self.ensure_identity_key(),
                ])
            }
            Message::RestoreFinished(Err(_)) => {
                clear_session_file();
                Task::none()
            }
            // Best-effort background sync; a failure here just means we'll
            // retry on the next login. Direct chats stay locked until the
            // public key is on the server and the peer has one too.
            Message::PublicKeyUploaded => Task::none(),

            Message::EmailVerifyInputChanged(value) => {
                self.email_verify_input = value;
                if self.email_verify_error.is_some() {
                    self.email_verify_error = None;
                }
                Task::none()
            }
            Message::EmailVerifyCodeInputChanged(value) => {
                self.email_verify_code_input = value;
                if self.email_verify_error.is_some() {
                    self.email_verify_error = None;
                }
                Task::none()
            }
            Message::RequestEmailVerification => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.email_verify_busy {
                    return Task::none();
                }
                let email = self.email_verify_input.trim().to_lowercase();
                if email.is_empty() || !email.contains('@') || !email.contains('.') {
                    self.email_verify_error = Some("Enter a valid email address".to_string());
                    return Task::none();
                }
                self.email_verify_busy = true;
                self.email_verify_error = None;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .action(
                                "email:requestEmailVerification",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "email".to_string() => Value::String(email),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RequestEmailVerificationFinished,
                )
            }
            Message::RequestEmailVerificationFinished(Ok(())) => {
                self.email_verify_busy = false;
                self.email_verify_code_sent = true;
                self.show_toast("Verification code sent");
                Task::none()
            }
            Message::RequestEmailVerificationFinished(Err(err)) => {
                self.email_verify_busy = false;
                self.email_verify_error = Some(err);
                Task::none()
            }
            Message::SubmitEmailVerificationCode => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.email_verify_busy {
                    return Task::none();
                }
                let code = self.email_verify_code_input.trim().to_string();
                if code.is_empty() {
                    self.email_verify_error = Some("Enter the code from your email".to_string());
                    return Task::none();
                }
                self.email_verify_busy = true;
                self.email_verify_error = None;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "email:verifyEmailCode",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "code".to_string() => Value::String(code),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::VerifyEmailCodeFinished,
                )
            }
            Message::VerifyEmailCodeFinished(Ok(())) => {
                self.email_verify_busy = false;
                self.email_verify_code_input.clear();
                if let Some(session) = &mut self.session {
                    session.email = self.email_verify_input.clone();
                    session.email_verified = true;
                }
                self.show_toast("Email verified");
                Task::none()
            }
            Message::VerifyEmailCodeFinished(Err(err)) => {
                self.email_verify_busy = false;
                self.email_verify_error = Some(err);
                Task::none()
            }
            Message::ChangeEmailVerifyAddress => {
                self.email_verify_code_sent = false;
                self.email_verify_code_input.clear();
                self.email_verify_error = None;
                Task::none()
            }

            Message::CheckForUpdate => {
                if self.pending_update_path.is_some() {
                    // Already downloaded -- no point re-downloading on
                    // every periodic tick; just remind the user.
                    self.update_check_status =
                        Some("Update ready -- press Restart & install.".to_string());
                    return Task::none();
                }
                self.update_check_status = Some("Checking...".to_string());
                check_for_update_task()
            }
            Message::RestartAndUpdate => {
                // Explicit "Restart & install": swap the exe and relaunch
                // it (unlike tray-quit, which stays off).
                if let Some(path) = &self.pending_update_path {
                    stage_exe_swap(path, true);
                    self.pending_window_action = Some(WindowAction::Exit);
                }
                Task::none()
            }
            Message::UpdateCheckFinished(outcome) => {
                match outcome {
                    UpdateOutcome::UpToDate => {
                        self.update_check_status = Some("You're up to date.".to_string());
                    }
                    UpdateOutcome::Downloaded { path, version } => {
                        self.update_check_status = Some(format!(
                            "Update to v{version} downloaded — installs next time HexaTalk restarts."
                        ));
                        self.pending_update_path = Some(path);
                        self.show_toast(format!(
                            "Update ready (v{version}) — installs on next restart"
                        ));
                    }
                    UpdateOutcome::Failed(err) => {
                        self.update_check_status = Some(format!("Update check failed: {err}"));
                    }
                }
                Task::none()
            }

            Message::MeasurePing => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.ping_status = Some("Measuring...".to_string());
                let mut client = client;
                Task::perform(
                    async move {
                        let started = Instant::now();
                        let ok = client
                            .mutation(
                                "presence:heartbeat",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .is_ok();
                        ok.then(|| started.elapsed().as_millis() as u64)
                    },
                    Message::PingMeasured,
                )
            }
            Message::PingMeasured(Some(ms)) => {
                self.ping_status = Some(format!("{ms}ms"));
                Task::none()
            }
            Message::PingMeasured(None) => {
                self.ping_status = Some("Ping failed".to_string());
                Task::none()
            }
            Message::WindowCloseRequested => {
                if self.tray_ready {
                    self.pending_window_action = Some(WindowAction::HideToTray);
                    // Hidden window cannot be focused -- without this,
                    // DM notifications stay suppressed while in tray.
                    self.window_focused = false;
                } else {
                    // No tray to hide into and no way back — quit for real
                    // instead of trapping the user with an invisible process.
                    if let Some(path) = &self.pending_update_path {
                        stage_exe_swap(path, false);
                    }
                    self.pending_window_action = Some(WindowAction::Exit);
                }
                Task::none()
            }
            Message::TrayEvent(tray::TrayEvent::Show) => {
                self.pending_window_action = Some(WindowAction::ShowAndFocus);
                self.window_focused = true;
                Task::none()
            }
            Message::TrayEvent(tray::TrayEvent::Quit) => {
                if let Some(path) = &self.pending_update_path {
                    stage_exe_swap(path, false);
                }
                self.pending_window_action = Some(WindowAction::Exit);
                Task::none()
            }
            Message::TrayEvent(tray::TrayEvent::Ready) => {
                self.tray_ready = true;
                Task::none()
            }
            Message::TrayEvent(tray::TrayEvent::Unavailable(reason)) => {
                self.tray_ready = false;
                self.show_toast(format!(
                    "Tray icon unavailable ({reason}) — closing will quit"
                ));
                Task::none()
            }
            Message::WindowFocusChanged(focused) => {
                self.window_focused = focused;
                Task::none()
            }

            Message::FriendsUpdated(friends) => {
                // Server already sorts favorites → online → name.
                let urls: Vec<String> =
                    friends.iter().map(|f| f.avatar_image_url.clone()).collect();
                self.friends = friends;

                let mut tasks = vec![self.fetch_missing_avatars(urls)];

                // Every online friend needs a DM conversation to
                // background-connect over (rows are created lazily, only
                // on first contact) — fire-and-forget, idempotent.
                if let (Some(client), Some(session)) = (self.client.clone(), self.session.clone()) {
                    for friend in self.friends.iter().filter(|f| f.is_online_like()) {
                        let has_conversation = self.conversations.iter().any(|c| {
                            c.kind == "direct"
                                && c.peer_user_id.as_deref() == Some(friend.user_id.as_str())
                        });
                        if has_conversation {
                            continue;
                        }
                        let mut client = client.clone();
                        let session_token = session.token.clone();
                        let friend_id = friend.user_id.clone();
                        tasks.push(Task::perform(
                            async move {
                                client
                                    .mutation(
                                        "conversations:getOrCreateDirect",
                                        btreemap! {
                                            "sessionToken".to_string() => Value::String(session_token),
                                            "friendUserId".to_string() => Value::String(friend_id),
                                        },
                                    )
                                    .await
                                    .map_err(|err| err.to_string())
                                    .and_then(expect_string)
                            },
                            Message::DirectConversationEnsured,
                        ));
                    }
                }

                // Reap background sessions for friends who went offline.
                let online_ids: std::collections::HashSet<&str> = self
                    .friends
                    .iter()
                    .filter(|f| f.is_online_like())
                    .map(|f| f.user_id.as_str())
                    .collect();
                let mut tracked: std::collections::HashSet<String> =
                    self.peer_cmd_txs.keys().cloned().collect();
                tracked.extend(self.peer_status.keys().cloned());
                tracked.extend(self.peer_connected.keys().cloned());
                let stale: Vec<String> = tracked
                    .into_iter()
                    .filter(|id| !online_ids.contains(id.as_str()))
                    .collect();
                for peer_id in stale {
                    self.stop_peer_session_for(&peer_id);
                }

                Task::batch(tasks)
            }
            Message::SocialStatsUpdated(stats) => {
                self.social_stats = stats;
                Task::none()
            }
            Message::SuggestionsUpdated(list) => {
                let urls: Vec<String> = list.iter().map(|s| s.avatar_image_url.clone()).collect();
                self.suggestions = list;
                self.fetch_missing_avatars(urls)
            }
            Message::PeopleSearchFinished(Ok(hits)) => {
                let urls: Vec<String> = hits.iter().map(|h| h.avatar_image_url.clone()).collect();
                self.people_hits = hits;
                self.fetch_missing_avatars(urls)
            }
            Message::PeopleSearchFinished(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::SetFriendsFilter(filter) => {
                self.friends_filter = filter;
                Task::none()
            }
            Message::ToggleFavorite(friend_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let id_for_result = friend_id.clone();
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "friends:toggleFavorite",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "friendUserId".to_string() => Value::String(friend_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        match result {
                            FunctionResult::Value(Value::Object(obj)) => {
                                let fav = obj.get("favorite").map(value_as_bool).unwrap_or(false);
                                Ok((id_for_result, fav))
                            }
                            FunctionResult::ErrorMessage(err) => Err(humanize_error(&err)),
                            FunctionResult::ConvexError(err) => {
                                Err(humanize_error(&format!("{err:?}")))
                            }
                            _ => Err("Unexpected server response".into()),
                        }
                    },
                    Message::FavoriteToggled,
                )
            }
            Message::FavoriteToggled(Ok((friend_id, favorite))) => {
                if let Some(f) = self.friends.iter_mut().find(|f| f.user_id == friend_id) {
                    f.favorite = favorite;
                }
                self.show_toast(if favorite {
                    "Added to favorites"
                } else {
                    "Removed from favorites"
                });
                Task::none()
            }
            Message::FavoriteToggled(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::RespondAllIncoming(accept) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "friends:respondAllIncoming",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "accept".to_string() => Value::Boolean(accept),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        match result {
                            FunctionResult::Value(Value::Object(obj)) => {
                                Ok(obj_f64(&obj, "count") as u32)
                            }
                            FunctionResult::ErrorMessage(err) => Err(humanize_error(&err)),
                            FunctionResult::ConvexError(err) => {
                                Err(humanize_error(&format!("{err:?}")))
                            }
                            _ => Err("Unexpected server response".into()),
                        }
                    },
                    Message::RespondAllFinished,
                )
            }
            Message::RespondAllFinished(Ok(count)) => {
                self.show_toast(&format!("Updated {count} request(s)"));
                Task::none()
            }
            Message::RespondAllFinished(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::CyclePresenceStatus => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let next = next_presence_status(&session.presence_status).to_string();
                if let Some(s) = &mut self.session {
                    s.presence_status = next.clone();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:setPresenceStatus",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "status".to_string() => Value::String(next),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PrivacyFlagFinished,
                )
            }

            Message::PeerEvent(peer_id, ev) => self.handle_peer_event(peer_id, ev),
            Message::PeerCmdReady(peer_id, tx) => {
                if let Some(payload) = self.pending_peer_invite.remove(&peer_id) {
                    let _ = tx.send(peer::PeerCmd::InvitePayload(payload));
                }
                if let Some(pub_res) = self.pending_invite_published.remove(&peer_id) {
                    match pub_res {
                        Ok(()) => {
                            let _ = tx.send(peer::PeerCmd::InvitePublished);
                        }
                        Err(err) => {
                            let _ = tx.send(peer::PeerCmd::InvitePublishFailed(err));
                        }
                    }
                }
                self.peer_cmd_txs.insert(peer_id, tx);
                Task::none()
            }
            Message::PeerInviteUpdated(peer_id, Some(payload)) => {
                if let Some(tx) = self.peer_cmd_txs.get(&peer_id) {
                    let _ = tx.send(peer::PeerCmd::InvitePayload(payload));
                } else {
                    self.pending_peer_invite.insert(peer_id, payload);
                }
                Task::none()
            }
            Message::PeerInviteUpdated(_, None) => Task::none(),
            Message::PeerInvitePublished(peer_id, Err(err)) => {
                self.peer_status
                    .insert(peer_id.clone(), format!("Invite publish failed: {err}"));
                if let Some(tx) = self.peer_cmd_txs.get(&peer_id) {
                    let _ = tx.send(peer::PeerCmd::InvitePublishFailed(err));
                } else {
                    self.pending_invite_published.insert(peer_id, Err(err));
                }
                Task::none()
            }
            Message::PeerInvitePublished(peer_id, Ok(())) => {
                self.peer_status
                    .insert(peer_id.clone(), "Invite saved — waiting for peer…".into());
                if let Some(tx) = self.peer_cmd_txs.get(&peer_id) {
                    let _ = tx.send(peer::PeerCmd::InvitePublished);
                } else {
                    self.pending_invite_published.insert(peer_id, Ok(()));
                }
                Task::none()
            }
            Message::DirectConversationEnsured(_) => Task::none(),
            Message::RequestsUpdated(requests) => {
                let grew = self.requests_loaded && requests.len() > self.incoming_requests.len();
                let newest = if grew {
                    requests
                        .iter()
                        .find(|r| {
                            !self
                                .incoming_requests
                                .iter()
                                .any(|old| old.request_id == r.request_id)
                        })
                        .cloned()
                } else {
                    None
                };
                self.requests_loaded = true;
                let urls: Vec<String> = requests
                    .iter()
                    .map(|r| r.from_avatar_image_url.clone())
                    .collect();
                self.incoming_requests = requests;
                if let Some(req) = newest {
                    let body = if req.note.is_empty() {
                        format!(
                            "{} (@{}) wants to add you as a friend",
                            req.from_display_name, req.from_username
                        )
                    } else {
                        format!(
                            "{} (@{}): {}",
                            req.from_display_name, req.from_username, req.note
                        )
                    };
                    self.notify_ping();
                    notify_desktop("Friend request", &body);
                }
                self.fetch_missing_avatars(urls)
            }
            Message::OutgoingRequestsUpdated(requests) => {
                let urls: Vec<String> = requests
                    .iter()
                    .map(|r| r.to_avatar_image_url.clone())
                    .collect();
                self.outgoing_requests = requests;
                self.fetch_missing_avatars(urls)
            }
            Message::BlockedUpdated(blocked) => {
                self.blocked = blocked;
                Task::none()
            }
            Message::ConversationsUpdated(list) => {
                let mut notify_title: Option<String> = None;
                if self.conversations_loaded {
                    for conv in &list {
                        if self.active_conversation.as_deref()
                            != Some(conv.conversation_id.as_str())
                        {
                            let prev = self
                                .seen_last_message_at
                                .get(&conv.conversation_id)
                                .copied()
                                .unwrap_or(0);
                            if prev > 0 && conv.last_message_at > prev && notify_title.is_none() {
                                notify_title = Some(conv.title.clone());
                            }
                        }
                    }
                }
                for conv in &list {
                    self.seen_last_message_at
                        .insert(conv.conversation_id.clone(), conv.last_message_at);
                }
                self.conversations_loaded = true;
                self.conversations = list;
                if let Some(title) = notify_title {
                    // Skip toast for muted channels (still show if not in channels list).
                    let muted = self.channels.iter().any(|c| {
                        c.muted
                            && self.conversations.iter().any(|conv| {
                                conv.title == title && conv.conversation_id == c.conversation_id
                            })
                    });
                    if !muted {
                        self.notify_ping();
                        notify_desktop("HexaTalk", &format!("New message · {title}"));
                    }
                }
                // Keep the open chat marked read as new traffic arrives --
                // but only when there is something genuinely unread. An
                // unconditional markRead writes lastReadAt on the server,
                // which re-fires this same watch and loops forever.
                if let Some(active_id) = self.active_conversation.clone() {
                    let active_row = self
                        .conversations
                        .iter()
                        .find(|c| c.conversation_id == active_id);
                    let needs_mark = active_row
                        .map(|c| {
                            let already_marked = self
                                .last_marked_read_at
                                .get(&c.conversation_id)
                                .copied()
                                .unwrap_or(0);
                            c.unread && c.last_message_at > already_marked
                        })
                        .unwrap_or(false);
                    if needs_mark {
                        let marked_at = active_row.map(|c| c.last_message_at).unwrap_or(0);
                        self.last_marked_read_at
                            .insert(active_id.clone(), marked_at);
                        return mark_read_task(&self.client, &self.session, active_id);
                    }
                }
                Task::none()
            }
            Message::AdminUsersUpdated(users) => {
                self.admin_users = users;
                Task::none()
            }
            Message::MessagesUpdated(messages) => {
                let messages = self.decrypt_incoming_messages(messages);
                // Mention ping (Discord-style): mention toast + message
                // beep when a genuinely new, recent message pings us (by
                // name, or @everyone in a channel/group). Skips our own
                // messages, live peerseal echoes (already notified on the
                // peer path), and history older than 2 min (opening a
                // conversation loads history through this same arm). When
                // the window is focused the highlighted row is signal
                // enough -- matches the peer path's focus rule.
                let mention_ping: Option<(String, String)> = if self.window_focused {
                    None
                } else {
                    let now = chrono::Utc::now().timestamp_millis();
                    let my_id = self
                        .session
                        .as_ref()
                        .map(|s| s.user_id.as_str())
                        .unwrap_or("");
                    let my_names = self.my_mention_names();
                    let everyone_ok = matches!(
                        self.active_conversation_kind.as_deref(),
                        Some("channel") | Some("group")
                    );
                    let live = self
                        .active_conversation_peer_id
                        .as_ref()
                        .and_then(|pid| self.peer_live_messages.get(pid));
                    let mut ping = None;
                    for m in &messages {
                        if m.author_id == my_id || m.kind == "call" || m.deleted {
                            continue;
                        }
                        if (now - m.sent_at).abs() > 120_000 {
                            continue;
                        }
                        if self.messages.iter().any(|old| old.id == m.id) {
                            continue;
                        }
                        if live.is_some_and(|l| {
                            l.iter().any(|e| {
                                e.author_id == m.author_id
                                    && e.body == m.body
                                    && (e.sent_at - m.sent_at).abs() < 120_000
                            })
                        }) {
                            continue;
                        }
                        if mentions::mentions_any(&m.body, &my_names)
                            || (everyone_ok && mentions::has_everyone(&m.body))
                        {
                            ping = Some((m.author_name.clone(), m.body.clone()));
                            break;
                        }
                    }
                    ping
                };
                if let Some((author, body)) = mention_ping {
                    self.notify_ping();
                    notify_desktop(
                        &format!("{author} mentioned you"),
                        &mentions::snippet(&body, 140),
                    );
                }
                let grew = messages.len() > self.messages.len();
                let image_jobs: Vec<(String, Option<String>, Option<String>)> = messages
                    .iter()
                    .flat_map(|m| {
                        [
                            (
                                m.attachment_url.clone(),
                                m.attachment_key.clone(),
                                m.attachment_nonce.clone(),
                            ),
                            (m.author_avatar_url.clone(), None, None),
                        ]
                    })
                    .collect();
                self.messages = messages;
                // Drop live peerseal echoes (for the conversation currently
                // open) once Convex history has them.
                if let Some(peer_id) = self.active_conversation_peer_id.clone() {
                    if let Some(live_msgs) = self.peer_live_messages.get_mut(&peer_id) {
                        let convex_msgs = &self.messages;
                        live_msgs.retain(|live| {
                            !convex_msgs.iter().any(|m| {
                                m.author_id == live.author_id
                                    && m.body == live.body
                                    && (m.attachment_url.is_empty()
                                        == live.attachment_url.is_empty()
                                        || !live.attachment_url.is_empty()
                                            && !m.attachment_url.is_empty())
                                    && (m.sent_at - live.sent_at).abs() < 120_000
                            })
                        });
                    }
                }
                let fetch = self.fetch_missing_images(image_jobs);
                if grew {
                    Task::batch([fetch, scroll_chat_to_bottom()])
                } else {
                    fetch
                }
            }
            Message::PinnedMessagesUpdated(pinned) => {
                // Same decrypt path as history: encrypted snippets from
                // `messages:listPinned` arrive as ciphertext and need the
                // group key (or stay as-is for plain DMs).
                self.pinned_messages = self.decrypt_incoming_messages(pinned);
                Task::none()
            }
            Message::TogglePinsPanel => {
                self.pins_panel_open = !self.pins_panel_open;
                Task::none()
            }
            Message::PinMessage(message_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "messages:pinMessage",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PinToggled,
                )
            }
            Message::UnpinMessage(message_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "messages:unpinMessage",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PinToggled,
                )
            }
            Message::PinToggled(Ok(())) => {
                self.chat_error = None;
                Task::none()
            }
            Message::PinToggled(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }

            Message::ArmReportMessage(message_id) => {
                self.reporting_message_id = Some(message_id);
                Task::none()
            }
            Message::CancelReportMessage => {
                self.reporting_message_id = None;
                Task::none()
            }
            Message::SubmitMessageReport(message_id, message_body, reason) => {
                self.reporting_message_id = None;
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "reports:reportMessage",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                    "messageBody".to_string() => Value::String(message_body),
                                    "reason".to_string() => Value::String(reason),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::MessageReportFinished,
                )
            }
            Message::MessageReportFinished(Ok(())) => {
                self.show_toast("Reported to staff");
                Task::none()
            }
            Message::MessageReportFinished(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::LoadAdminReports => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .query(
                                "reports:adminListReports",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(parse_message_reports)
                    },
                    Message::AdminReportsUpdated,
                )
            }
            Message::AdminReportsUpdated(Ok(reports)) => {
                self.admin_reports = reports;
                self.admin_reports_status = None;
                Task::none()
            }
            Message::AdminReportsUpdated(Err(err)) => {
                self.admin_reports_status = Some(err);
                Task::none()
            }
            Message::AdminResolveReport(report_id, status) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "reports:adminResolveReport",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "reportId".to_string() => Value::String(report_id),
                                    "status".to_string() => Value::String(status),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::AdminResolveReportFinished,
                )
            }
            Message::AdminResolveReportFinished(Ok(())) => Task::done(Message::LoadAdminReports),
            Message::AdminResolveReportFinished(Err(err)) => {
                self.admin_reports_status = Some(err);
                Task::none()
            }

            Message::SidebarTabChanged(tab) => {
                self.sidebar_tab = tab;
                // Don't carry search terms across tabs.
                if tab != SidebarTab::Chats {
                    self.chat_filter_input.clear();
                }
                if tab != SidebarTab::Friends {
                    self.friends_filter_input.clear();
                }
                if tab != SidebarTab::Admin {
                    self.admin_search_input.clear();
                }
                // Load platform counters + report queue when entering the Admin dashboard.
                if tab == SidebarTab::Admin {
                    return Task::batch([
                        Task::done(Message::LoadAdminStats),
                        Task::done(Message::LoadAdminReports),
                    ]);
                }
                Task::none()
            }
            Message::MessageHovered(id) => {
                self.hovered_message_id = id;
                Task::none()
            }
            Message::AdminSearchInputChanged(value) => {
                self.admin_search_input = value;
                Task::none()
            }
            Message::AddFriendInputChanged(value) => {
                self.add_friend_input = value;
                // Live people search when typing 2+ chars.
                let q = self
                    .add_friend_input
                    .trim()
                    .trim_start_matches('@')
                    .to_string();
                if q.chars().count() < 2 {
                    self.people_hits.clear();
                    return Task::none();
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .query(
                                "friends:searchPeople",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "query".to_string() => Value::String(q),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        Ok(parse_object_array(result)
                            .into_iter()
                            .map(|obj| PeopleHit {
                                user_id: obj_str(&obj, "userId"),
                                username: obj_str(&obj, "username"),
                                display_name: obj_str(&obj, "displayName"),
                                avatar_color: obj_str(&obj, "avatarColor"),
                                avatar_image_url: obj_str(&obj, "avatarImageUrl"),
                                status_message: obj_str(&obj, "statusMessage"),
                                presence: obj_str(&obj, "presence"),
                                relation: obj_str(&obj, "relation"),
                                incoming_request_id: obj_str(&obj, "incomingRequestId"),
                                mutual_servers: obj_str_list(&obj, "mutualServers"),
                                is_staff: obj.get("isStaff").map(value_as_bool).unwrap_or(false),
                            })
                            .collect())
                    },
                    Message::PeopleSearchFinished,
                )
            }
            Message::AddFriendNoteChanged(value) => {
                if value.chars().count() <= 200 {
                    self.add_friend_note = value;
                }
                Task::none()
            }
            Message::SendFriendRequest => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.friend_request_busy {
                    return Task::none();
                }
                let username = self
                    .add_friend_input
                    .trim()
                    .trim_start_matches('@')
                    .to_lowercase();
                if username.is_empty() {
                    self.add_friend_status = Some("Enter a username".to_string());
                    return Task::none();
                }
                if self
                    .friends
                    .iter()
                    .any(|f| f.username.eq_ignore_ascii_case(&username))
                {
                    self.add_friend_status = Some("You're already friends".to_string());
                    return Task::none();
                }
                if self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.username.eq_ignore_ascii_case(&username))
                {
                    self.add_friend_status = Some("You can't add yourself".to_string());
                    return Task::none();
                }
                let note = self.add_friend_note.trim().to_string();
                let mut client = client;
                self.add_friend_status = Some("Sending…".to_string());
                self.friend_request_busy = true;
                self.add_friend_input = username.clone();
                Task::perform(
                    async move {
                        let mut args = btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                            "toUsername".to_string() => Value::String(username),
                        };
                        if !note.is_empty() {
                            args.insert("note".to_string(), Value::String(note));
                        }
                        client
                            .mutation("friends:sendRequest", args)
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::FriendRequestFinished,
                )
            }
            Message::FriendRequestFinished(Ok(())) => {
                self.friend_request_busy = false;
                self.add_friend_input.clear();
                self.add_friend_note.clear();
                self.add_friend_status = Some("Request sent".to_string());
                self.show_toast("Friend request sent");
                Task::none()
            }
            Message::FriendRequestFinished(Err(err)) => {
                self.friend_request_busy = false;
                self.add_friend_status = Some(humanize_error(&err));
                Task::none()
            }
            Message::SendFriendRequestToUser(username) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.friend_request_busy {
                    return Task::none();
                }
                self.friend_request_busy = true;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:sendRequest",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "toUsername".to_string() => Value::String(username),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::ProfileFriendRequestFinished,
                )
            }
            Message::ProfileFriendRequestFinished(Ok(())) => {
                self.friend_request_busy = false;
                self.show_toast("Friend request sent");
                Task::none()
            }
            Message::ProfileFriendRequestFinished(Err(err)) => {
                self.friend_request_busy = false;
                self.show_toast(err);
                Task::none()
            }
            Message::RespondRequest(request_id, accept) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:respondRequest",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "requestId".to_string() => Value::String(request_id),
                                    "accept".to_string() => Value::Boolean(accept),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::RequestRespondFinished,
                )
            }
            Message::RequestRespondFinished(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::RequestRespondFinished(Ok(())) => {
                self.show_toast("Friend request updated");
                Task::none()
            }
            Message::CancelOutgoingRequest(request_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:cancelRequest",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "requestId".to_string() => Value::String(request_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::CancelOutgoingFinished,
                )
            }
            Message::CancelOutgoingFinished(Ok(())) => {
                self.show_toast("Request cancelled");
                Task::none()
            }
            Message::CancelOutgoingFinished(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::OpenSupportDm(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let peer_name = self
                    .viewing_profile
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| "Support".to_string());
                let peer_id_for_result = user_id.clone();
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "conversations:openSupportDm",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "peerUserId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_string)
                    },
                    move |result| {
                        Message::SupportDmOpened(
                            result.map(|id| (peer_name.clone(), peer_id_for_result.clone(), id)),
                        )
                    },
                )
            }
            Message::SupportDmOpened(Ok((title, peer_id, conversation_id))) => {
                self.viewing_profile = None;
                self.profile_error = None;
                self.sidebar_tab = SidebarTab::Chats;
                self.show_toast("Support chat opened");
                let stop = self.stop_typing_task();
                self.active_conversation = Some(conversation_id.clone());
                self.active_conversation_kind = Some("direct".to_string());
                self.active_conversation_peer_id = Some(peer_id);
                self.active_peer_name = Some(title);
                self.messages.clear();
                self.pinned_messages.clear();
                self.pins_panel_open = false;
                self.chat_error = None;
                self.editing_message_id = None;
                self.hovered_message_id = None;
                self.clear_chat_confirm = false;
                self.clear_chat_busy = false;
                self.message_input.clear();
                self.mention_suggestions.clear();
                self.pending_attachment = None;
                self.pending_reply = None;
                self.typing_names.clear();
                Task::batch([
                    stop,
                    mark_read_task(&self.client, &self.session, conversation_id),
                    self.load_conversation_store_pref(),
                ])
            }
            Message::SupportDmOpened(Err(err)) => {
                self.profile_error = Some(err);
                Task::none()
            }
            Message::CycleFriendRequestPrivacy => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let next = next_friend_request_privacy(&session.friend_request_privacy).to_string();
                if let Some(s) = &mut self.session {
                    s.friend_request_privacy = next.clone();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setPrivacyFlags",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "friendRequestPrivacy".to_string() => Value::String(next),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PrivacyFlagFinished,
                )
            }
            Message::RemoveFriend(friend_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:removeFriend",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "friendUserId".to_string() => Value::String(friend_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::RemoveFriendFinished,
                )
            }
            Message::RemoveFriendFinished(Err(err)) => {
                self.add_friend_status = Some(err);
                Task::none()
            }
            Message::RemoveFriendFinished(Ok(())) => Task::none(),
            Message::ConfirmBlockUser(user_id) => {
                self.confirm_block_user_id = Some(user_id);
                Task::none()
            }
            Message::CancelBlockUser => {
                self.confirm_block_user_id = None;
                Task::none()
            }
            Message::BlockUser(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.confirm_block_user_id = None;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:blockUser",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::BlockFinished,
                )
            }
            Message::BlockFinished(Err(err)) => {
                self.show_toast(err);
                Task::none()
            }
            Message::BlockFinished(Ok(())) => {
                self.show_toast("User blocked");
                Task::none()
            }
            Message::UnblockUser(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "friends:unblockUser",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    |_| Message::UnblockFinished,
                )
            }
            Message::UnblockFinished => Task::none(),

            Message::OpenConversationWithFriend(user_id) => {
                let Some(friend) = self.friends.iter().find(|f| f.user_id == user_id).cloned()
                else {
                    return Task::none();
                };
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                let title_for_result = friend.display_name.clone();
                let peer_id_for_result = friend.user_id.clone();
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "conversations:getOrCreateDirect",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "friendUserId".to_string() => Value::String(friend.user_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                    },
                    move |result| {
                        Message::ConversationOpened(result.map(|id| {
                            (
                                title_for_result.clone(),
                                Some(peer_id_for_result.clone()),
                                id,
                            )
                        }))
                    },
                )
            }
            Message::OpenConversationDirect(conversation_id) => {
                let Some(summary) = self
                    .conversations
                    .iter()
                    .find(|c| c.conversation_id == conversation_id)
                    .cloned()
                else {
                    return Task::none();
                };
                let stop = self.stop_typing_task();
                self.sidebar_tab = SidebarTab::Chats;
                self.selected_server = None;
                self.active_conversation = Some(summary.conversation_id.clone());
                self.active_conversation_kind = Some(summary.kind.clone());
                self.active_conversation_peer_id = summary.peer_user_id.clone();
                self.active_peer_name = Some(summary.title.clone());
                self.messages.clear();
                self.pinned_messages.clear();
                self.pins_panel_open = false;
                self.chat_error = None;
                self.editing_message_id = None;
                self.hovered_message_id = None;
                self.clear_chat_confirm = false;
                self.clear_chat_busy = false;
                self.message_input.clear();
                self.mention_suggestions.clear();
                self.pending_attachment = None;
                self.pending_reply = None;
                self.typing_names.clear();
                let group_key = if summary.kind == "group" {
                    self.ensure_group_key()
                } else {
                    Task::none()
                };
                Task::batch([
                    stop,
                    mark_read_task(&self.client, &self.session, summary.conversation_id),
                    self.load_conversation_store_pref(),
                    group_key,
                ])
            }
            Message::ConversationOpened(Ok((title, peer_id, conversation_id))) => {
                let stop = self.stop_typing_task();
                self.sidebar_tab = SidebarTab::Chats;
                self.selected_server = None;
                self.active_conversation = Some(conversation_id.clone());
                self.active_conversation_kind = Some("direct".to_string());
                self.active_conversation_peer_id = peer_id;
                self.active_peer_name = Some(title);
                self.messages.clear();
                self.pinned_messages.clear();
                self.pins_panel_open = false;
                self.chat_error = None;
                self.editing_message_id = None;
                self.hovered_message_id = None;
                self.clear_chat_confirm = false;
                self.clear_chat_busy = false;
                self.message_input.clear();
                self.mention_suggestions.clear();
                self.pending_attachment = None;
                self.pending_reply = None;
                self.typing_names.clear();
                Task::batch([
                    stop,
                    mark_read_task(&self.client, &self.session, conversation_id),
                    self.load_conversation_store_pref(),
                ])
            }
            Message::ConversationOpened(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::MarkReadFinished => Task::none(),

            Message::ToggleGroupPanel => {
                self.new_group_open = !self.new_group_open;
                self.group_create_status = None;
                Task::none()
            }
            Message::GroupNameInputChanged(value) => {
                self.new_group_name_input = value;
                Task::none()
            }
            Message::ToggleGroupMember(user_id) => {
                if self.new_group_selected.contains(&user_id) {
                    self.new_group_selected.remove(&user_id);
                } else {
                    self.new_group_selected.insert(user_id);
                }
                Task::none()
            }
            Message::CreateGroup => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let name = self.new_group_name_input.trim().to_string();
                if name.is_empty() || self.new_group_selected.is_empty() {
                    self.group_create_status =
                        Some("Enter a name and select at least one friend".to_string());
                    return Task::none();
                }
                let member_ids: Vec<Value> = self
                    .new_group_selected
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect();
                let mut client = client;
                let name_for_result = name.clone();
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "conversations:createGroup",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "name".to_string() => Value::String(name),
                                    "memberUserIds".to_string() => Value::Array(member_ids),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                    },
                    move |result| {
                        Message::GroupCreateFinished(result.map(|id| (name_for_result.clone(), id)))
                    },
                )
            }
            Message::GroupCreateFinished(Ok((title, conversation_id))) => {
                self.new_group_open = false;
                self.new_group_name_input.clear();
                self.new_group_selected.clear();
                self.group_create_status = None;
                self.active_conversation = Some(conversation_id);
                self.active_conversation_kind = Some("group".to_string());
                self.active_conversation_peer_id = None;
                self.active_peer_name = Some(title);
                self.messages.clear();
                self.pinned_messages.clear();
                self.pins_panel_open = false;
                self.sidebar_tab = SidebarTab::Chats;
                self.ensure_group_key()
            }
            Message::GroupCreateFinished(Err(err)) => {
                self.group_create_status = Some(err);
                Task::none()
            }

            Message::ServersUpdated(servers) => {
                let urls: Vec<String> = servers.iter().map(|s| s.icon_url.clone()).collect();
                if let Some(selected) = &self.selected_server {
                    if let Some(fresh) = servers.iter().find(|s| s.server_id == selected.server_id)
                    {
                        // Never clobber in-progress Overview text fields while
                        // settings are open — that was wiping rename / vanity
                        // slug mid-type every subscription tick.
                        if !self.server_settings_open {
                            self.custom_slug_input = fresh.custom_slug.clone();
                            self.rename_server_input = fresh.name.clone();
                        }
                        self.selected_server = Some(fresh.clone());
                    } else {
                        // Left or deleted server while viewing it.
                        self.selected_server = None;
                        self.server_settings_open = false;
                        self.channels.clear();
                        self.server_members.clear();
                    }
                }
                self.servers = servers;
                self.fetch_missing_avatars(urls)
            }
            Message::NewServerNameChanged(value) => {
                self.new_server_name_input = value;
                Task::none()
            }
            Message::CreateServer => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let name = self.new_server_name_input.trim().to_string();
                if name.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:createServer",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "name".to_string() => Value::String(name),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::CreateServerFinished,
                )
            }
            Message::CreateServerFinished(Ok(())) => {
                self.new_server_name_input.clear();
                self.server_status = None;
                self.server_add_menu_open = false;
                self.show_toast("Server created");
                Task::none()
            }
            Message::CreateServerFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::JoinServerCodeChanged(value) => {
                self.join_server_code_input = value;
                Task::none()
            }
            Message::JoinServer => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let invite_code = extract_invite_code(&self.join_server_code_input);
                if invite_code.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:joinByInviteCode",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "inviteCode".to_string() => Value::String(invite_code),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::JoinServerFinished,
                )
            }
            Message::JoinServerFinished(Ok(())) => {
                self.join_server_code_input.clear();
                self.server_status = None;
                self.server_add_menu_open = false;
                self.show_toast("Joined server");
                Task::none()
            }
            Message::JoinServerFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::GoHome => {
                self.selected_server = None;
                self.channels.clear();
                self.server_members.clear();
                self.server_roles.clear();
                self.server_add_menu_open = false;
                self.server_settings_open = false;
                self.sidebar_tab = SidebarTab::Chats;
                Task::none()
            }
            Message::ToggleServerAddMenu => {
                self.server_add_menu_open = !self.server_add_menu_open;
                Task::none()
            }
            Message::SelectServer(server_id) => {
                let Some(server) = self
                    .servers
                    .iter()
                    .find(|s| s.server_id == server_id)
                    .cloned()
                else {
                    return Task::none();
                };
                self.server_add_menu_open = false;
                self.rename_server_input = server.name.clone();
                self.custom_slug_input = server.custom_slug.clone();
                self.selected_server = Some(server);
                self.channels.clear();
                self.new_channel_open = false;
                self.new_channel_name_input.clear();
                self.server_status = None;
                self.server_settings_open = false;
                self.server_settings_category = ServerSettingsCategory::Overview;
                self.confirm_delete_server = false;
                self.server_members.clear();
                self.renaming_channel_id = None;
                self.rename_channel_input.clear();
                // Switch middle panel into server channel view.
                self.sidebar_tab = SidebarTab::Servers;
                Task::none()
            }
            Message::BackToServerList => {
                self.selected_server = None;
                self.channels.clear();
                self.server_settings_open = false;
                self.confirm_delete_server = false;
                self.server_members.clear();
                self.renaming_channel_id = None;
                self.rename_channel_input.clear();
                self.sidebar_tab = SidebarTab::Chats;
                Task::none()
            }
            Message::PickServerIcon => {
                if self.server_icon_busy {
                    return Task::none();
                }
                if !self.selected_server.as_ref().is_some_and(|s| s.is_owner) {
                    self.server_status = Some("Only the server owner can change the icon".into());
                    return Task::none();
                }
                Task::perform(
                    async move {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .add_filter("Images", &["png", "jpg", "jpeg"])
                            .pick_file()
                            .await
                        else {
                            return AvatarPick::Cancelled;
                        };
                        let bytes = file.read().await;
                        if bytes.len() > 2 * 1024 * 1024 {
                            return AvatarPick::TooLarge;
                        }
                        let name = file.file_name().to_lowercase();
                        let content_type = if name.ends_with(".png") {
                            "image/png"
                        } else {
                            "image/jpeg"
                        };
                        AvatarPick::Ready(bytes, content_type.to_string())
                    },
                    Message::ServerIconPicked,
                )
            }
            Message::ServerIconPicked(AvatarPick::Cancelled) => Task::none(),
            Message::ServerIconPicked(AvatarPick::TooLarge) => {
                self.server_status = Some("Icon must be under 2MB".into());
                Task::none()
            }
            Message::ServerIconPicked(AvatarPick::Ready(bytes, content_type)) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                self.server_icon_busy = true;
                let mut client = client;
                Task::perform(
                    async move {
                        let upload_url = client
                            .mutation(
                                "servers:generateIconUploadUrl",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token.clone()),
                                    "serverId".to_string() => Value::String(server.server_id.clone()),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_string)?;
                        let http = reqwest::Client::new();
                        let response = http
                            .post(&upload_url)
                            .header("Content-Type", content_type.as_str())
                            .body(bytes)
                            .send()
                            .await
                            .map_err(|e| format!("Upload failed: {e}"))?;
                        if !response.status().is_success() {
                            return Err(format!("Upload failed (HTTP {})", response.status()));
                        }
                        #[derive(serde::Deserialize)]
                        struct UploadResponse {
                            #[serde(rename = "storageId")]
                            storage_id: String,
                        }
                        let parsed: UploadResponse = response
                            .json()
                            .await
                            .map_err(|e| format!("Upload response invalid: {e}"))?;
                        client
                            .mutation(
                                "servers:setServerIcon",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "storageId".to_string() => Value::String(parsed.storage_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_string)
                    },
                    Message::ServerIconUploadFinished,
                )
            }
            Message::ServerIconUploadFinished(Ok(url)) => {
                self.server_icon_busy = false;
                self.server_status = None;
                if let Some(s) = &mut self.selected_server {
                    s.icon_url = url.clone();
                }
                if let Some(sid) = self.selected_server.as_ref().map(|s| s.server_id.clone()) {
                    if let Some(row) = self.servers.iter_mut().find(|s| s.server_id == sid) {
                        row.icon_url = url.clone();
                    }
                }
                self.show_toast("Server icon updated");
                if url.is_empty() {
                    Task::none()
                } else {
                    // Drop any stale cache entry so a re-upload of the same
                    // bytes path still refreshes (URL is unique per storage id).
                    self.avatar_image_cache.remove(&url);
                    self.fetch_missing_avatars(std::iter::once(url))
                }
            }
            Message::ServerIconUploadFinished(Err(err)) => {
                self.server_icon_busy = false;
                self.server_status = Some(err);
                Task::none()
            }
            Message::RemoveServerIcon => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    self.server_status = Some("Only the server owner can change the icon".into());
                    return Task::none();
                }
                self.server_icon_busy = true;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:removeServerIcon",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::ServerIconRemoveFinished,
                )
            }
            Message::ServerIconRemoveFinished(Ok(())) => {
                self.server_icon_busy = false;
                self.server_status = None;
                if let Some(s) = &mut self.selected_server {
                    s.icon_url.clear();
                }
                if let Some(sid) = self.selected_server.as_ref().map(|s| s.server_id.clone()) {
                    if let Some(row) = self.servers.iter_mut().find(|s| s.server_id == sid) {
                        row.icon_url.clear();
                    }
                }
                self.show_toast("Server icon removed");
                Task::none()
            }
            Message::ServerIconRemoveFinished(Err(err)) => {
                self.server_icon_busy = false;
                self.server_status = Some(err);
                Task::none()
            }
            Message::CustomSlugInputChanged(v) => {
                self.custom_slug_input = v;
                Task::none()
            }
            Message::SaveCustomSlug => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let slug = self.custom_slug_input.trim().to_string();
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:setCustomSlug",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "slug".to_string() => Value::String(slug),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_string)
                    },
                    Message::CustomSlugFinished,
                )
            }
            Message::ClearCustomSlug => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:clearCustomSlug",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)?;
                        Ok(String::new())
                    },
                    Message::CustomSlugFinished,
                )
            }
            Message::CustomSlugFinished(Ok(slug)) => {
                if let Some(s) = &mut self.selected_server {
                    s.custom_slug = slug.clone();
                }
                self.custom_slug_input = slug.clone();
                self.show_toast(if slug.is_empty() {
                    "Custom URL cleared".into()
                } else {
                    format!("URL set: /{slug}")
                });
                Task::none()
            }
            Message::CustomSlugFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::CopyInviteCode(code) => {
                self.show_toast("Invite code copied");
                write_clipboard_text(code);
                Task::none()
            }
            Message::CopyInviteLink(code) => {
                self.show_toast("Invite link copied");
                write_clipboard_text(build_invite_link(&code));
                Task::none()
            }
            Message::ChannelsUpdated(channels) => {
                self.channels = channels;
                Task::none()
            }
            Message::OpenChannel(conversation_id) => {
                let Some(channel) = self
                    .channels
                    .iter()
                    .find(|c| c.conversation_id == conversation_id)
                    .cloned()
                else {
                    return Task::none();
                };
                let stop = self.stop_typing_task();
                self.active_conversation = Some(channel.conversation_id.clone());
                self.active_conversation_kind = Some(if channel.channel_type == "voice" {
                    "voice".to_string()
                } else {
                    "channel".to_string()
                });
                self.active_conversation_peer_id = None;
                self.active_peer_name = Some(if channel.channel_type == "voice" {
                    format!("v {}", channel.name)
                } else {
                    format!("#{}", channel.name)
                });
                self.messages.clear();
                self.pinned_messages.clear();
                self.pins_panel_open = false;
                self.chat_error = None;
                self.editing_message_id = None;
                self.hovered_message_id = None;
                self.clear_chat_confirm = false;
                self.clear_chat_busy = false;
                self.message_input.clear();
                self.mention_suggestions.clear();
                self.pending_attachment = None;
                self.pending_reply = None;
                self.typing_names.clear();
                // Clicking a voice channel joins it straight away (Discord
                // behavior) instead of dropping the user into a text view
                // with a separate Join button. JoinVoiceChannel reads the
                // freshly-set active_conversation above.
                let auto_join = channel.channel_type == "voice"
                    && self.active_voice_channel.as_deref()
                        != Some(channel.conversation_id.as_str());
                let mut tasks = vec![
                    stop,
                    self.load_conversation_store_pref(),
                    mark_read_task(&self.client, &self.session, channel.conversation_id.clone()),
                    self.ensure_group_key(),
                ];
                if auto_join {
                    tasks.push(Task::done(Message::JoinVoiceChannel));
                }
                Task::batch(tasks)
            }
            Message::ToggleNewChannelInput => {
                self.new_channel_open = !self.new_channel_open;
                Task::none()
            }
            Message::NewChannelNameChanged(value) => {
                self.new_channel_name_input = value;
                Task::none()
            }
            Message::CreateChannel => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let name = self.new_channel_name_input.trim().to_string();
                if name.is_empty() {
                    return Task::none();
                }
                let channel_type = if self.new_channel_is_voice {
                    "voice"
                } else {
                    "text"
                }
                .to_string();
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:createChannel",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "name".to_string() => Value::String(name),
                                    "channelType".to_string() => Value::String(channel_type),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::CreateChannelFinished,
                )
            }
            Message::CreateChannelFinished(Ok(())) => {
                self.new_channel_open = false;
                self.new_channel_name_input.clear();
                self.new_channel_is_voice = false;
                self.server_status = None;
                Task::none()
            }
            Message::CreateChannelFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            Message::ToggleServerSettings => {
                let opening = !self.server_settings_open;
                self.server_settings_open = opening;
                self.server_settings_category = ServerSettingsCategory::Overview;
                self.confirm_delete_server = false;
                self.confirm_transfer_owner_id = None;
                self.renaming_channel_id = None;
                self.rename_channel_input.clear();
                self.server_status = None;
                self.server_stats = None;
                if opening {
                    if let Some(server) = &self.selected_server {
                        self.rename_server_input = server.name.clone();
                        self.custom_slug_input = server.custom_slug.clone();
                        self.server_description_input = server.description.clone();
                    }
                    self.new_channel_open = false;
                    self.new_channel_name_input.clear();
                    self.new_channel_is_voice = false;
                    // Pull the read-only stats card once, on open.
                    return Task::done(Message::LoadServerStats);
                }
                Task::none()
            }
            Message::RenameServerInputChanged(value) => {
                self.rename_server_input = value;
                Task::none()
            }
            Message::RenameServer => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    self.server_status = Some("Only the server owner can rename the server".into());
                    return Task::none();
                }
                let name = self.rename_server_input.trim().to_string();
                if name.is_empty() {
                    self.server_status = Some("Enter a server name".into());
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:renameServer",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "name".to_string() => Value::String(name),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RenameServerFinished,
                )
            }
            Message::RenameServerFinished(Ok(())) => {
                self.server_status = None;
                let name = self.rename_server_input.trim().to_string();
                if let Some(s) = &mut self.selected_server {
                    s.name = name.clone();
                }
                if let Some(sid) = self.selected_server.as_ref().map(|s| s.server_id.clone()) {
                    if let Some(row) = self.servers.iter_mut().find(|s| s.server_id == sid) {
                        row.name = name;
                    }
                }
                self.show_toast("Server name updated");
                Task::none()
            }
            Message::RenameServerFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            // ---- Server description ----
            Message::ServerDescriptionInputChanged(value) => {
                self.server_description_input = value;
                Task::none()
            }
            Message::SaveServerDescription => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    self.server_status =
                        Some("Only the server owner can edit the description".into());
                    return Task::none();
                }
                let description = self.server_description_input.trim().to_string();
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:setServerDescription",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "description".to_string() => Value::String(description),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::SaveServerDescriptionFinished,
                )
            }
            Message::SaveServerDescriptionFinished(Ok(())) => {
                let desc = self.server_description_input.trim().to_string();
                if let Some(s) = &mut self.selected_server {
                    s.description = desc;
                }
                self.server_status = None;
                self.show_toast("Description updated");
                Task::none()
            }
            Message::SaveServerDescriptionFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            // ---- Transfer ownership ----
            Message::ConfirmTransferOwnership(user_id) => {
                // Empty string (or re-clicking the armed member) cancels.
                self.confirm_transfer_owner_id = if user_id.is_empty()
                    || self.confirm_transfer_owner_id.as_deref() == Some(user_id.as_str())
                {
                    None
                } else {
                    Some(user_id)
                };
                Task::none()
            }
            Message::TransferOwnership(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    self.server_status = Some("Only the owner can transfer the server".into());
                    return Task::none();
                }
                self.confirm_transfer_owner_id = None;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:transferOwnership",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "newOwnerId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::TransferOwnershipFinished,
                )
            }
            Message::TransferOwnershipFinished(Ok(())) => {
                // We're no longer the owner; the servers subscription will
                // refresh is_owner, but flip it locally so the UI updates now.
                if let Some(s) = &mut self.selected_server {
                    s.is_owner = false;
                }
                self.server_status = None;
                self.show_toast("Ownership transferred");
                Task::none()
            }
            Message::TransferOwnershipFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            // ---- Defaults: welcome channel + invite pause ----
            Message::SetWelcomeChannel(channel_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    return Task::none();
                }
                // Re-clicking the current welcome channel clears it.
                let next = if server.welcome_channel_id == channel_id {
                    String::new()
                } else {
                    channel_id
                };
                if let Some(s) = &mut self.selected_server {
                    s.welcome_channel_id = next.clone();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:setWelcomeChannel",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "channelId".to_string() => Value::String(next),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::SetWelcomeChannelFinished,
                )
            }
            Message::SetWelcomeChannelFinished(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::SetWelcomeChannelFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::ToggleInvitesPaused => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                if !server.is_owner {
                    return Task::none();
                }
                let paused = !server.invites_paused;
                if let Some(s) = &mut self.selected_server {
                    s.invites_paused = paused;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:setInvitesPaused",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "paused".to_string() => Value::Boolean(paused),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::SetInvitesPausedFinished,
                )
            }
            Message::SetInvitesPausedFinished(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::SetInvitesPausedFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            // ---- Server stats (on-demand) ----
            Message::LoadServerStats => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .query(
                                "servers:serverStats",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                },
                            )
                            .await
                            .ok();
                        result.and_then(parse_server_stats)
                    },
                    Message::ServerStatsUpdated,
                )
            }
            Message::ServerStatsUpdated(stats) => {
                self.server_stats = stats;
                Task::none()
            }

            Message::RegenerateInviteCode => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:regenerateInviteCode",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::RegenerateInviteCodeFinished,
                )
            }
            Message::RegenerateInviteCodeFinished(Ok(())) => {
                self.server_status = None;
                self.show_toast("Invite code regenerated");
                Task::none()
            }
            Message::RegenerateInviteCodeFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::ToggleConfirmDeleteServer => {
                self.confirm_delete_server = !self.confirm_delete_server;
                Task::none()
            }
            Message::DeleteServer => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:deleteServer",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::DeleteServerFinished,
                )
            }
            Message::DeleteServerFinished(Ok(())) => {
                self.selected_server = None;
                self.channels.clear();
                self.server_settings_open = false;
                self.confirm_delete_server = false;
                self.server_status = None;
                self.server_members.clear();
                Task::none()
            }
            Message::DeleteServerFinished(Err(err)) => {
                self.confirm_delete_server = false;
                self.server_status = Some(err);
                Task::none()
            }
            Message::ServerSettingsCategoryChanged(category) => {
                self.server_settings_category = category;
                Task::none()
            }
            Message::MembersUpdated(members) => {
                let avatar_urls: Vec<String> =
                    members.iter().map(|m| m.avatar_image_url.clone()).collect();
                self.server_members = members;
                self.fetch_missing_avatars(avatar_urls)
            }
            Message::KickMember(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:kickMember",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::KickMemberFinished,
                )
            }
            Message::KickMemberFinished(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::KickMemberFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::StartRenameChannel(conversation_id, current_name) => {
                self.renaming_channel_id = Some(conversation_id);
                self.rename_channel_input = current_name;
                Task::none()
            }
            Message::RenameChannelInputChanged(value) => {
                self.rename_channel_input = value;
                Task::none()
            }
            Message::RenameChannel => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conversation_id) = self.renaming_channel_id.clone() else {
                    return Task::none();
                };
                let name = self.rename_channel_input.trim().to_string();
                if name.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:renameChannel",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                    "name".to_string() => Value::String(name),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::RenameChannelFinished,
                )
            }
            Message::RenameChannelFinished(Ok(())) => {
                self.show_toast("Channel renamed");
                self.renaming_channel_id = None;
                self.rename_channel_input.clear();
                self.server_status = None;
                Task::none()
            }
            Message::RenameChannelFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::CancelRenameChannel => {
                self.renaming_channel_id = None;
                self.rename_channel_input.clear();
                Task::none()
            }
            Message::DeleteChannel(conversation_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.active_conversation.as_deref() == Some(conversation_id.as_str()) {
                    self.active_conversation = None;
                    self.active_conversation_kind = None;
                    self.active_peer_name = None;
                    self.messages.clear();
                    self.pinned_messages.clear();
                    self.pins_panel_open = false;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "servers:deleteChannel",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::DeleteChannelFinished,
                )
            }
            Message::DeleteChannelFinished(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::DeleteChannelFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::MoveChannelUp(conversation_id) => {
                self.move_channel_task(conversation_id, "up")
            }
            Message::MoveChannelDown(conversation_id) => {
                self.move_channel_task(conversation_id, "down")
            }
            Message::MoveChannelFinished(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::MoveChannelFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::EditChannelPerms(conversation_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.channel_perms_channel_id = Some(conversation_id.clone());
                self.channel_perms_role_id = self
                    .server_roles
                    .iter()
                    .find(|r| r.position == 0)
                    .map(|r| r.role_id.clone())
                    .or_else(|| self.server_roles.first().map(|r| r.role_id.clone()));
                self.channel_overwrites.clear();
                let mut client = client;
                let conv_for_result = conversation_id.clone();
                Task::perform(
                    async move {
                        let result = client
                            .query(
                                "channels:listOverwrites",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        let rows = parse_object_array(result);
                        let mut role_ows = Vec::new();
                        for obj in rows {
                            let target_type = obj_str(&obj, "targetType");
                            if target_type != "role" {
                                continue;
                            }
                            let role_id = obj_str(&obj, "targetId");
                            let allow = obj_f64(&obj, "allow") as u32;
                            let deny = obj_f64(&obj, "deny") as u32;
                            role_ows.push((role_id, allow, deny));
                        }
                        Ok((conv_for_result, role_ows))
                    },
                    Message::ChannelOverwritesLoaded,
                )
            }
            Message::CloseChannelPerms => {
                self.channel_perms_channel_id = None;
                self.channel_perms_role_id = None;
                self.channel_overwrites.clear();
                Task::none()
            }
            Message::ChannelOverwritesLoaded(Ok((conv_id, rows))) => {
                if self.channel_perms_channel_id.as_deref() != Some(conv_id.as_str()) {
                    return Task::none();
                }
                self.channel_overwrites.clear();
                for (role_id, allow, deny) in rows {
                    if allow != 0 || deny != 0 {
                        self.channel_overwrites.insert(role_id, (allow, deny));
                    }
                }
                Task::none()
            }
            Message::ChannelOverwritesLoaded(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::SelectChannelPermRole(role_id) => {
                self.channel_perms_role_id = Some(role_id);
                Task::none()
            }
            Message::CycleChannelOverwritePerm(bit) => {
                let Some(channel_id) = self.channel_perms_channel_id.clone() else {
                    return Task::none();
                };
                let Some(role_id) = self.channel_perms_role_id.clone() else {
                    return Task::none();
                };
                let (mut allow, mut deny) = self
                    .channel_overwrites
                    .get(&role_id)
                    .copied()
                    .unwrap_or((0, 0));
                // Inherit (0) → Allow (1) → Deny (2) → Inherit
                let mode = if allow & bit != 0 {
                    1
                } else if deny & bit != 0 {
                    2
                } else {
                    0
                };
                allow &= !bit;
                deny &= !bit;
                match mode {
                    0 => allow |= bit,
                    1 => deny |= bit,
                    _ => {}
                }
                if allow == 0 && deny == 0 {
                    self.channel_overwrites.remove(&role_id);
                } else {
                    self.channel_overwrites
                        .insert(role_id.clone(), (allow, deny));
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "channels:setOverwrite",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(channel_id),
                                    "targetType".to_string() => Value::String("role".into()),
                                    "targetId".to_string() => Value::String(role_id),
                                    "allow".to_string() => Value::Float64(allow as f64),
                                    "deny".to_string() => Value::Float64(deny as f64),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::ChannelOverwriteSaved,
                )
            }
            Message::ChannelOverwriteSaved(Ok(())) => {
                self.server_status = None;
                Task::none()
            }
            Message::ChannelOverwriteSaved(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }

            Message::MessageInputChanged(value) => {
                let now_empty = value.trim().is_empty();
                self.message_input = value;
                self.mention_suggestions = self.compute_mention_suggestions();
                let Some(conversation_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                if now_empty {
                    // Stopped typing: clear our indicator right away.
                    if self.typing_active {
                        self.typing_active = false;
                        self.last_typing_ping = None;
                        return typing_ping_task(
                            &self.client,
                            &self.session,
                            conversation_id,
                            false,
                        );
                    }
                    return Task::none();
                }
                // Debounce: refresh the "typing" ping at most every 2s.
                let due = self
                    .last_typing_ping
                    .map(|t| t.elapsed() >= Duration::from_secs(2))
                    .unwrap_or(true);
                if due {
                    self.typing_active = true;
                    self.last_typing_ping = Some(Instant::now());
                    return typing_ping_task(&self.client, &self.session, conversation_id, true);
                }
                Task::none()
            }
            Message::MentionSuggestionPicked(name) => {
                self.message_input = mentions::complete(&self.message_input, &name);
                self.mention_suggestions.clear();
                Task::none()
            }
            Message::PickAttachmentImage => Task::perform(
                async move {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg"])
                        .pick_file()
                        .await
                    else {
                        return AttachmentPick::Cancelled;
                    };
                    let bytes = file.read().await;
                    if bytes.len() > 5 * 1024 * 1024 {
                        return AttachmentPick::TooLarge;
                    }
                    let name = file.file_name().to_lowercase();
                    let content_type = if name.ends_with(".png") {
                        "image/png"
                    } else {
                        "image/jpeg"
                    };
                    AttachmentPick::Ready(bytes, content_type.to_string())
                },
                Message::AttachmentFilePicked,
            ),
            Message::AttachmentFilePicked(AttachmentPick::Cancelled) => Task::none(),
            Message::AttachmentFilePicked(AttachmentPick::TooLarge) => {
                self.chat_error = Some("Attachment must be smaller than 5MB".to_string());
                Task::none()
            }
            Message::AttachmentFilePicked(AttachmentPick::Ready(bytes, content_type)) => {
                self.pending_attachment = Some(PendingAttachment {
                    bytes,
                    content_type,
                });
                self.chat_error = None;
                Task::none()
            }
            Message::RemovePendingAttachment => {
                self.pending_attachment = None;
                Task::none()
            }

            Message::SendMessage => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.send_busy {
                    return Task::none();
                }
                let body = self.message_input.trim().to_string();
                // Hard cap keeps UI snappy and matches typical chat limits.
                if body.chars().count() > 4000 {
                    self.chat_error = Some("Message is too long (max 4000 characters)".to_string());
                    return Task::none();
                }
                let is_direct = self.active_conversation_kind.as_deref() == Some("direct");

                // Direct chats: live peerseal (Noise E2EE) for realtime, plus
                // Convex as the durable shared history (no local vault copy).
                if is_direct {
                    if self.editing_message_id.take().is_some() {
                        self.chat_error =
                            Some("Edits aren't supported on the live secure channel yet".into());
                        return Task::none();
                    }
                    let Some(peer_id) = self.active_conversation_peer_id.clone() else {
                        self.chat_error = Some("Secure channel is not running".into());
                        return Task::none();
                    };
                    if !self.peer_connected.get(&peer_id).copied().unwrap_or(false) {
                        self.chat_error = Some(
                            self.peer_status
                                .get(&peer_id)
                                .cloned()
                                .unwrap_or_else(|| "Secure channel not connected yet".into()),
                        );
                        return Task::none();
                    }
                    let Some(tx) = self.peer_cmd_txs.get(&peer_id).cloned() else {
                        self.chat_error = Some("Secure channel is not running".into());
                        return Task::none();
                    };
                    let attachment = self.pending_attachment.take();
                    if body.is_empty() && attachment.is_none() {
                        return Task::none();
                    }
                    let Some(conversation_id) = self.active_conversation.clone() else {
                        self.pending_attachment = attachment;
                        return Task::none();
                    };
                    self.message_input.clear();
                    self.mention_suggestions.clear();
                    self.pending_reply = None;
                    self.chat_error = None;

                    if let Some(att) = attachment {
                        let _ = tx.send(peer::PeerCmd::SendPhoto {
                            bytes: att.bytes.clone(),
                            content_type: att.content_type.clone(),
                            width: 0,
                            height: 0,
                        });
                        self.push_local_peer_message(
                            &session,
                            &peer_id,
                            String::new(),
                            Some(att.bytes.clone()),
                            att.content_type.clone(),
                        );
                        if !session.store_chat_history || !self.chat_store_enabled {
                            return scroll_chat_to_bottom();
                        }
                        // Durable copy on Convex (shared history for both sides).
                        self.send_busy = true;
                        let mut client = client;
                        let session_token = session.token.clone();
                        let content_type = att.content_type.clone();
                        let bytes = att.bytes;
                        let caption = body.clone();
                        return Task::batch([
                            scroll_chat_to_bottom(),
                            Task::perform(
                                async move {
                                    let upload_url = client
                                        .mutation(
                                            "messages:generateAttachmentUploadUrl",
                                            btreemap! {
                                                "sessionToken".to_string() => Value::String(session_token.clone()),
                                            },
                                        )
                                        .await
                                        .map_err(|err| humanize_error(&err.to_string()))
                                        .and_then(expect_string)?;
                                    let http = reqwest::Client::new();
                                    let response = http
                                        .post(&upload_url)
                                        .header("Content-Type", content_type.as_str())
                                        .body(bytes)
                                        .send()
                                        .await
                                        .map_err(|err| format!("Upload failed: {err}"))?;
                                    if !response.status().is_success() {
                                        return Err(format!(
                                            "Upload failed (HTTP {})",
                                            response.status()
                                        ));
                                    }
                                    #[derive(serde::Deserialize)]
                                    struct UploadResponse {
                                        #[serde(rename = "storageId")]
                                        storage_id: String,
                                    }
                                    let parsed: UploadResponse = response
                                        .json()
                                        .await
                                        .map_err(|err| format!("Upload response invalid: {err}"))?;
                                    client
                                        .mutation(
                                            "messages:send",
                                            btreemap! {
                                                "sessionToken".to_string() => Value::String(session_token),
                                                "conversationId".to_string() => Value::String(conversation_id),
                                                "body".to_string() => Value::String(caption),
                                                "attachmentStorageId".to_string() => Value::String(parsed.storage_id),
                                            },
                                        )
                                        .await
                                        .map_err(|err| humanize_error(&err.to_string()))
                                        .and_then(expect_null)
                                },
                                Message::MessageSentFinished,
                            ),
                        ]);
                    }
                    if !body.is_empty() {
                        let _ = tx.send(peer::PeerCmd::SendText(body.clone()));
                        self.push_local_peer_message(
                            &session,
                            &peer_id,
                            body.clone(),
                            None,
                            String::new(),
                        );
                        // Durable shared history on Convex (unless storage disabled).
                        if !session.store_chat_history || !self.chat_store_enabled {
                            return scroll_chat_to_bottom();
                        }
                        self.send_busy = true;
                        let mut client = client;
                        let session_token = session.token.clone();
                        return Task::batch([
                            scroll_chat_to_bottom(),
                            Task::perform(
                                async move {
                                    client
                                        .mutation(
                                            "messages:send",
                                            btreemap! {
                                                "sessionToken".to_string() => Value::String(session_token),
                                                "conversationId".to_string() => Value::String(conversation_id),
                                                "body".to_string() => Value::String(body),
                                            },
                                        )
                                        .await
                                        .map_err(|err| humanize_error(&err.to_string()))
                                        .and_then(expect_null)
                                },
                                Message::MessageSentFinished,
                            ),
                        ]);
                    }
                    return scroll_chat_to_bottom();
                }

                if let Some(message_id) = self.editing_message_id.take() {
                    if body.is_empty() {
                        return Task::none();
                    }
                    let was_encrypted = self.editing_message_encrypted;
                    let mut body_to_send = body;
                    if was_encrypted {
                        let kind = self.active_conversation_kind.as_deref().unwrap_or("");
                        let conv = self.active_conversation.clone().unwrap_or_default();
                        if matches!(kind, "group" | "channel" | "voice") {
                            if let Some((epoch, key)) =
                                self.group_key_store.as_ref().and_then(|s| s.get(&conv))
                            {
                                let payload = crypto::MessagePayload::text_only(body_to_send);
                                match crypto::encrypt_group_message(
                                    &key,
                                    epoch,
                                    &conv,
                                    &payload.encode(),
                                ) {
                                    Some(ct) => body_to_send = ct,
                                    None => {
                                        self.chat_error =
                                            Some("Could not re-encrypt edited message".into());
                                        return Task::none();
                                    }
                                }
                            } else {
                                self.chat_error = Some(
                                    "Group key not ready — wait a moment and try again".into(),
                                );
                                return Task::none();
                            }
                        }
                    }
                    self.message_input.clear();
                    self.mention_suggestions.clear();
                    self.send_busy = true;
                    let mut client = client;
                    return Task::perform(
                        async move {
                            client
                                .mutation(
                                    "messages:edit",
                                    btreemap! {
                                        "sessionToken".to_string() => Value::String(session.token),
                                        "messageId".to_string() => Value::String(message_id),
                                        "body".to_string() => Value::String(body_to_send),
                                    },
                                )
                                .await
                                .map_err(|err| humanize_error(&err.to_string()))
                                .and_then(expect_null)
                        },
                        Message::EditFinished,
                    );
                }

                let attachment = self.pending_attachment.take();
                if body.is_empty() && attachment.is_none() {
                    return Task::none();
                }
                let reply_to = self.pending_reply.take();

                let Some(conversation_id) = self.active_conversation.clone() else {
                    self.pending_attachment = attachment;
                    self.pending_reply = reply_to;
                    return Task::none();
                };

                // Encrypt group / server channel messages with TGK1.
                let kind = self.active_conversation_kind.as_deref().unwrap_or("");
                let groupish = matches!(kind, "group" | "channel" | "voice");
                let group_key = if groupish {
                    self.group_key_store
                        .as_ref()
                        .and_then(|s| s.get(&conversation_id))
                } else {
                    None
                };
                if groupish && group_key.is_none() {
                    self.pending_attachment = attachment;
                    self.pending_reply = reply_to;
                    self.chat_error = Some(
                        "Encrypting… group key not ready yet. Wait a second or reopen the chat."
                            .into(),
                    );
                    return self.ensure_group_key();
                }

                let mut body_to_send = body;
                let mut encrypted_flag = false;
                let mut upload_bytes = attachment.as_ref().map(|a| a.bytes.clone());
                let mut upload_content_type = attachment
                    .as_ref()
                    .map(|a| a.content_type.clone())
                    .unwrap_or_else(|| "application/octet-stream".into());

                if let Some((epoch, key)) = group_key {
                    encrypted_flag = true;
                    let mut payload = crypto::MessagePayload::text_only(body_to_send.clone());
                    if let Some(bytes) = upload_bytes.take() {
                        let (ct, att_key, att_nonce) = crypto::encrypt_attachment(&bytes);
                        payload.att_key = Some(att_key);
                        payload.att_nonce = Some(att_nonce);
                        upload_bytes = Some(ct);
                        upload_content_type = "application/octet-stream".into();
                    }
                    match crypto::encrypt_group_message(
                        &key,
                        epoch,
                        &conversation_id,
                        &payload.encode(),
                    ) {
                        Some(ct) => body_to_send = ct,
                        None => {
                            self.pending_attachment = attachment;
                            self.pending_reply = reply_to;
                            self.chat_error = Some("Could not encrypt message".into());
                            return Task::none();
                        }
                    }
                }

                let reply_to_message_id = reply_to.map(|(id, _, _)| id);
                self.message_input.clear();
                self.mention_suggestions.clear();
                self.chat_error = None;
                self.send_busy = true;
                let stop_typing = if self.typing_active {
                    self.typing_active = false;
                    self.last_typing_ping = None;
                    typing_ping_task(&self.client, &self.session, conversation_id.clone(), false)
                } else {
                    Task::none()
                };
                let mut client = client;
                let send = Task::perform(
                    async move {
                        let attachment_storage_id: Option<String> = if let Some(bytes) =
                            upload_bytes
                        {
                            let upload_url = client
                                    .mutation(
                                        "messages:generateAttachmentUploadUrl",
                                        btreemap! {
                                            "sessionToken".to_string() => Value::String(session.token.clone()),
                                        },
                                    )
                                    .await
                                    .map_err(|err| humanize_error(&err.to_string()))
                                    .and_then(expect_string)?;

                            let http = reqwest::Client::new();
                            let response = http
                                .post(&upload_url)
                                .header("Content-Type", upload_content_type.as_str())
                                .body(bytes)
                                .send()
                                .await
                                .map_err(|err| format!("Upload failed: {err}"))?;

                            if !response.status().is_success() {
                                return Err(format!("Upload failed (HTTP {})", response.status()));
                            }

                            #[derive(serde::Deserialize)]
                            struct UploadResponse {
                                #[serde(rename = "storageId")]
                                storage_id: String,
                            }
                            let parsed: UploadResponse = response
                                .json()
                                .await
                                .map_err(|err| format!("Upload response invalid: {err}"))?;
                            Some(parsed.storage_id)
                        } else {
                            None
                        };

                        let mut args = btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                            "conversationId".to_string() => Value::String(conversation_id),
                            "body".to_string() => Value::String(body_to_send),
                        };
                        if encrypted_flag {
                            args.insert("encrypted".to_string(), Value::Boolean(true));
                        }
                        if let Some(storage_id) = attachment_storage_id {
                            args.insert(
                                "attachmentStorageId".to_string(),
                                Value::String(storage_id),
                            );
                        }
                        if let Some(reply_id) = reply_to_message_id {
                            args.insert("replyToMessageId".to_string(), Value::String(reply_id));
                        }

                        client
                            .mutation("messages:send", args)
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::MessageSentFinished,
                );
                Task::batch([stop_typing, send])
            }
            Message::MessageSentFinished(Err(err)) => {
                self.send_busy = false;
                self.chat_error = Some(humanize_error(&err));
                Task::none()
            }
            Message::MessageSentFinished(Ok(())) => {
                self.send_busy = false;
                self.chat_error = None;
                Task::none()
            }

            Message::EditMessage(id, body, encrypted) => {
                self.editing_message_id = Some(id);
                self.editing_message_encrypted = encrypted;
                self.message_input = body;
                Task::none()
            }
            Message::CancelEdit => {
                self.editing_message_id = None;
                self.message_input.clear();
                self.mention_suggestions.clear();
                Task::none()
            }
            Message::EditFinished(Err(err)) => {
                self.send_busy = false;
                self.chat_error = Some(humanize_error(&err));
                Task::none()
            }
            Message::EditFinished(Ok(())) => {
                self.send_busy = false;
                self.chat_error = None;
                self.show_toast("Message updated");
                Task::none()
            }
            Message::DeleteMessage(message_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "messages:remove",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::DeleteFinished,
                )
            }
            Message::DeleteFinished(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::DeleteFinished(Ok(())) => Task::none(),
            Message::PurgeMessage(message_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "messages:purge",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::PurgeFinished,
                )
            }
            Message::PurgeFinished(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::PurgeFinished(Ok(())) => Task::none(),
            Message::CopyMessage(text) => {
                self.show_toast("Copied to clipboard");
                write_clipboard_text(text);
                Task::none()
            }
            Message::ToggleReaction(message_id, emoji) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "messages:toggleReaction",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "messageId".to_string() => Value::String(message_id),
                                    "emoji".to_string() => Value::String(emoji),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::ReactionToggled,
                )
            }
            Message::ReactionToggled(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::ReactionToggled(Ok(())) => Task::none(),
            Message::ReplyToMessage(message_id, author_name, snippet) => {
                self.pending_reply = Some((message_id, author_name, snippet));
                Task::none()
            }
            Message::CancelReply => {
                self.pending_reply = None;
                Task::none()
            }

            Message::ToggleStoreHistoryGlobal => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let store = !session.store_chat_history;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setStoreChatHistory",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "store".to_string() => Value::Boolean(store),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)?;
                        Ok(store)
                    },
                    Message::StoreHistoryGlobalFinished,
                )
            }
            Message::StoreHistoryGlobalFinished(Ok(store)) => {
                if let Some(session) = &mut self.session {
                    session.store_chat_history = store;
                }
                self.show_toast(if store {
                    "Chat history storage: ON"
                } else {
                    "Chat history storage: OFF (ephemeral)"
                });
                Task::none()
            }
            Message::StoreHistoryGlobalFinished(Err(err)) => {
                self.settings_profile_status = Some(err);
                Task::none()
            }
            Message::ToggleHideOnline => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let next = !session.hide_online_status;
                if let Some(s) = &mut self.session {
                    s.hide_online_status = next;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setPrivacyFlags",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "hideOnlineStatus".to_string() => Value::Boolean(next),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PrivacyFlagFinished,
                )
            }
            Message::ToggleFriendsOnlyDms => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let next = !session.friends_only_dms;
                if let Some(s) = &mut self.session {
                    s.friends_only_dms = next;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setPrivacyFlags",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "friendsOnlyDms".to_string() => Value::Boolean(next),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PrivacyFlagFinished,
                )
            }
            Message::ToggleDiscoverable => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let next = !session.discoverable;
                if let Some(s) = &mut self.session {
                    s.discoverable = next;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setPrivacyFlags",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "discoverable".to_string() => Value::Boolean(next),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::PrivacyFlagFinished,
                )
            }
            Message::PrivacyFlagFinished(Ok(())) => {
                self.show_toast("Privacy updated");
                Task::none()
            }
            Message::PrivacyFlagFinished(Err(err)) => {
                self.settings_profile_status = Some(err);
                Task::none()
            }
            Message::SignOutOtherSessions => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "prefs:signOutOtherSessions",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        match result {
                            FunctionResult::Value(Value::Object(obj)) => {
                                Ok(obj_f64(&obj, "killed") as u32)
                            }
                            FunctionResult::ErrorMessage(e) => Err(humanize_error(&e)),
                            FunctionResult::ConvexError(e) => {
                                Err(humanize_error(&format!("{e:?}")))
                            }
                            _ => Ok(0),
                        }
                    },
                    Message::SignOutOthersFinished,
                )
            }
            Message::SignOutOthersFinished(Ok(n)) => {
                self.show_toast(format!("Signed out {n} other session(s)"));
                Task::none()
            }
            Message::SignOutOthersFinished(Err(err)) => {
                self.settings_profile_status = Some(err);
                Task::none()
            }
            Message::ToggleStoreHistoryThisChat => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conversation_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                let store = !self.chat_store_enabled;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "prefs:setConversationStore",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                    "store".to_string() => Value::Boolean(store),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)?;
                        Ok(store)
                    },
                    Message::StoreHistoryChatFinished,
                )
            }
            Message::StoreHistoryChatFinished(Ok(store)) => {
                self.chat_store_enabled = store;
                self.show_toast(if store {
                    "This chat: history ON"
                } else {
                    "This chat: history OFF"
                });
                // Refresh effective allow flag.
                self.load_conversation_store_pref()
            }
            Message::StoreHistoryChatFinished(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::ConversationStorePrefLoaded(store, allows) => {
                self.chat_store_enabled = store;
                self.chat_store_allowed = allows;
                Task::none()
            }

            Message::NewChannelIsVoice(v) => {
                self.new_channel_is_voice = v;
                Task::none()
            }
            Message::ToggleNewChannelIsVoice => {
                self.new_channel_is_voice = !self.new_channel_is_voice;
                Task::none()
            }
            Message::JoinVoiceChannel => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conversation_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                let mut client = client;
                let joined_id = conversation_id.clone();
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "voice:join",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "conversationId".to_string() => Value::String(conversation_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .map(|_| joined_id)
                    },
                    |result| match result {
                        Ok(id) => Message::VoiceActionFinished(Ok(Some(id))),
                        Err(e) => Message::VoiceActionFinished(Err(e)),
                    },
                )
            }
            Message::LeaveVoiceChannel => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let left_id = self.active_voice_channel.clone();
                self.active_voice_channel = None;
                self.room_voice_status = None;
                self.voice_users.clear();
                let mut client = client;
                Task::perform(
                    async move {
                        let mut args = btreemap! {
                            "sessionToken".to_string() => Value::String(session.token),
                        };
                        if let Some(id) = left_id {
                            args.insert("conversationId".to_string(), Value::String(id));
                        }
                        client
                            .mutation("voice:leave", args)
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .map(|_| None::<String>)
                    },
                    |result| match result {
                        Ok(_) => Message::VoiceActionFinished(Ok(None)),
                        Err(e) => Message::VoiceActionFinished(Err(e)),
                    },
                )
            }
            Message::VoiceUsersUpdated(users) => {
                self.voice_users = users;
                Task::none()
            }
            Message::VoiceVolumeChanged(user_id, volume) => {
                if let Ok(mut gains) = self.voice_gains.lock() {
                    gains.insert(user_id, volume.clamp(0.0, 5.0));
                }
                self.persist_settings();
                Task::none()
            }
            Message::VoiceActionFinished(Ok(Some(channel_id))) => {
                self.active_voice_channel = Some(channel_id);
                self.room_voice_status = Some("Connecting voice…".into());
                // Mute 1:1 call engine conflict: hang up private call if any.
                if self.call_role.is_some() {
                    self.call_role = None;
                    self.call_engine_key = None;
                }
                self.show_toast("Joined voice — mesh connecting");
                Task::none()
            }
            Message::VoiceActionFinished(Ok(None)) => {
                self.room_voice_status = None;
                self.show_toast("Left voice");
                Task::none()
            }
            Message::VoiceActionFinished(Err(err)) => {
                self.server_status = Some(err);
                self.room_voice_status = None;
                Task::none()
            }
            Message::RoomVoiceEngineEvent(ev) => {
                use crate::media::room_voice::RoomVoiceEvent;
                match ev {
                    RoomVoiceEvent::Connecting => {
                        self.room_voice_status = Some("Connecting voice…".into());
                    }
                    RoomVoiceEvent::Connected => {
                        self.room_voice_status = Some("Voice connected".into());
                    }
                    RoomVoiceEvent::Status(s) => {
                        self.room_voice_status = Some(s);
                    }
                    RoomVoiceEvent::Ended => {
                        if self.active_voice_channel.is_none() {
                            self.room_voice_status = None;
                        }
                    }
                    RoomVoiceEvent::Failed(err) => {
                        self.room_voice_status = Some(err.clone());
                        self.chat_error = Some(err);
                    }
                }
                Task::none()
            }
            Message::GroupKeyLoaded(conversation_id, epoch, key) => {
                if self.group_key_store.is_none() {
                    if let Some(session) = &self.session {
                        self.group_key_store = Some(crypto::GroupKeyStore::load(
                            &hexatalk_data_dir(),
                            &session.user_id,
                        ));
                    }
                }
                if let Some(store) = self.group_key_store.as_mut() {
                    store.put(&conversation_id, epoch, key);
                }
                // Re-run decrypt on messages already in the buffer.
                if self.active_conversation.as_deref() == Some(conversation_id.as_str()) {
                    let msgs = std::mem::take(&mut self.messages);
                    self.messages = self.decrypt_incoming_messages(msgs);
                }
                Task::none()
            }
            Message::GroupKeyReady(Err(err)) => {
                // Soft fail: chat still works in plaintext if bootstrap fails.
                self.chat_error = Some(format!("Group encryption: {err}"));
                Task::none()
            }
            Message::GroupKeyReady(Ok(())) => Task::none(),

            Message::ServerRolesUpdated(roles) => {
                self.server_roles = roles;
                Task::none()
            }
            Message::NewRoleNameChanged(v) => {
                self.new_role_name_input = v;
                Task::none()
            }
            Message::CreateRole => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let name = self.new_role_name_input.trim().to_string();
                if name.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:createRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "name".to_string() => Value::String(name),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::CreateRoleFinished,
                )
            }
            Message::CreateRoleFinished(Ok(())) => {
                self.new_role_name_input.clear();
                self.show_toast("Role created");
                Task::none()
            }
            Message::CreateRoleFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::ToggleMemberRole(user_id, role_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:toggleRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "userId".to_string() => Value::String(user_id),
                                    "roleId".to_string() => Value::String(role_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::ToggleRoleFinished,
                )
            }
            Message::ToggleRoleFinished(Ok(())) => {
                self.show_toast("Role updated");
                Task::none()
            }
            Message::ToggleRoleFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::ToggleMemberRolePicker(user_id) => {
                self.member_role_picker_open =
                    if self.member_role_picker_open.as_deref() == Some(user_id.as_str()) {
                        None
                    } else {
                        Some(user_id)
                    };
                Task::none()
            }
            Message::MyServerPermsUpdated(perms) => {
                self.my_server_permissions = perms;
                Task::none()
            }
            Message::SelectRoleForEdit(role_id) => {
                if let Some(role) = self.server_roles.iter().find(|r| r.role_id == role_id) {
                    self.role_name_edit_input = role.name.clone();
                }
                self.editing_role_id = Some(role_id);
                self.confirm_delete_role_id = None;
                Task::none()
            }
            Message::CloseRoleEditor => {
                self.editing_role_id = None;
                self.role_name_edit_input.clear();
                self.confirm_delete_role_id = None;
                Task::none()
            }
            Message::RoleNameEditChanged(v) => {
                self.role_name_edit_input = v;
                Task::none()
            }
            Message::SaveRoleName => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(role_id) = self.editing_role_id.clone() else {
                    return Task::none();
                };
                let name = self.role_name_edit_input.trim().to_string();
                if name.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:updateRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "roleId".to_string() => Value::String(role_id),
                                    "name".to_string() => Value::String(name),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RoleMutationFinished,
                )
            }
            Message::SetRoleColor(role_id, hex) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:updateRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "roleId".to_string() => Value::String(role_id),
                                    "color".to_string() => Value::String(hex),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RoleMutationFinished,
                )
            }
            Message::ToggleRolePermission(role_id, bit) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(role) = self.server_roles.iter().find(|r| r.role_id == role_id) else {
                    return Task::none();
                };
                let new_perms = role.permissions ^ bit;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:updateRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "roleId".to_string() => Value::String(role_id),
                                    "permissions".to_string() => Value::Float64(new_perms as f64),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RoleMutationFinished,
                )
            }
            Message::RoleMutationFinished(Ok(())) => Task::none(),
            Message::RoleMutationFinished(Err(err)) => {
                self.server_status = Some(err);
                Task::none()
            }
            Message::ConfirmDeleteRole(role_id) => {
                self.confirm_delete_role_id = Some(role_id);
                Task::none()
            }
            Message::CancelDeleteRole => {
                self.confirm_delete_role_id = None;
                Task::none()
            }
            Message::DeleteRole(role_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.confirm_delete_role_id = None;
                if self.editing_role_id.as_deref() == Some(role_id.as_str()) {
                    self.editing_role_id = None;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "roles:deleteRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "roleId".to_string() => Value::String(role_id),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::RoleMutationFinished,
                )
            }

            Message::ToggleClearChatConfirm => {
                if self.clear_chat_busy {
                    return Task::none();
                }
                self.clear_chat_confirm = !self.clear_chat_confirm;
                Task::none()
            }
            Message::ConfirmClearChat => {
                // Server channels keep normal Convex history — clear is DM/group only.
                if matches!(
                    self.active_conversation_kind.as_deref(),
                    Some("channel") | Some("voice")
                ) {
                    self.clear_chat_confirm = false;
                    self.show_toast("Server channels can't be wiped this way");
                    return Task::none();
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conversation_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                if self.clear_chat_busy {
                    return Task::none();
                }
                self.clear_chat_busy = true;
                self.clear_chat_confirm = false;
                self.chat_error = None;

                // Ask the live peer to wipe their local caches too (DMs only).
                if self.active_conversation_kind.as_deref() == Some("direct") {
                    if let Some(peer_id) = &self.active_conversation_peer_id {
                        if self.peer_connected.get(peer_id).copied().unwrap_or(false) {
                            if let Some(tx) = self.peer_cmd_txs.get(peer_id) {
                                let _ = tx.send(peer::PeerCmd::SendText(
                                    PEER_CLEAR_HISTORY_CTRL.to_string(),
                                ));
                            }
                        }
                    }
                }

                // Local caches / leftover vault / ratchet logs + UI right away.
                self.wipe_local_chat_history();

                let mut client = client;
                Task::perform(
                    async move {
                        // Drain Convex shared history in batches.
                        let mut total = 0u64;
                        loop {
                            let result = client
                                .mutation(
                                    "messages:clearConversation",
                                    btreemap! {
                                        "sessionToken".to_string() => Value::String(session.token.clone()),
                                        "conversationId".to_string() => Value::String(conversation_id.clone()),
                                    },
                                )
                                .await
                                .map_err(|err| humanize_error(&err.to_string()))?;
                            let (purged, done) = parse_clear_conversation_result(result)?;
                            total += purged;
                            if done {
                                break;
                            }
                        }
                        Ok(if total == 0 {
                            "Chat cleared (Convex + local caches)".to_string()
                        } else {
                            format!("Chat cleared · {total} messages removed from Convex")
                        })
                    },
                    Message::ClearChatFinished,
                )
            }
            Message::ClearChatFinished(Ok(status)) => {
                self.clear_chat_busy = false;
                // Re-clear UI in case a subscription raced and refilled messages.
                self.wipe_local_chat_history();
                self.show_toast(status);
                Task::none()
            }
            Message::ClearChatFinished(Err(err)) => {
                self.clear_chat_busy = false;
                self.chat_error = Some(format!("Server history: {err}"));
                self.show_toast("Local caches cleared");
                Task::none()
            }

            Message::AdminSetRole(user_id, make_admin) => {
                let role = if make_admin { "admin" } else { "user" }.to_string();
                Task::done(Message::AdminSetPlatformRole(user_id, role))
            }
            Message::AdminSetPlatformRole(user_id, role) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "admin:setRole",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                    "role".to_string() => Value::String(role),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(expect_null)
                    },
                    Message::AdminSetRoleFinished,
                )
            }
            Message::AdminSetRoleFinished(Err(err)) => {
                self.admin_status = Some(err);
                Task::none()
            }
            Message::AdminSetRoleFinished(Ok(())) => {
                self.show_toast("Role updated");
                Task::none()
            }
            Message::AdminSetBanned(user_id, banned) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "admin:setBanned",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                    "banned".to_string() => Value::Boolean(banned),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::AdminSetBannedFinished,
                )
            }
            Message::AdminSetBannedFinished(Err(err)) => {
                self.admin_status = Some(err);
                Task::none()
            }
            Message::AdminSetBannedFinished(Ok(())) => Task::none(),

            Message::LoadAdminStats => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .query(
                                "admin:adminStats",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .map_err(|err| humanize_error(&err.to_string()))
                            .and_then(|result| {
                                parse_admin_stats(result)
                                    .ok_or_else(|| "Could not read admin stats".to_string())
                            })
                    },
                    Message::AdminStatsUpdated,
                )
            }
            Message::AdminStatsUpdated(Ok(stats)) => {
                self.admin_stats = Some(stats);
                Task::none()
            }
            Message::AdminStatsUpdated(Err(err)) => {
                self.admin_status = Some(err);
                Task::none()
            }

            Message::ToggleMembersPanel => {
                self.members_panel_open = !self.members_panel_open;
                self.members_panel_target = if self.members_panel_open {
                    self.members_panel_preferred_width
                } else {
                    28.0
                };
                Task::none()
            }
            Message::AnimateMembersPanel => {
                let diff = self.members_panel_target - self.members_panel_width;
                if diff.abs() < 0.8 {
                    self.members_panel_width = self.members_panel_target;
                } else {
                    self.members_panel_width += diff * 0.28;
                }
                Task::none()
            }
            Message::PanelResizeStarted(kind) => {
                self.resizing_panel = Some(kind);
                self.resize_drag_anchor = None;
                Task::none()
            }
            Message::PanelResizeMoved(x) => {
                let Some(kind) = self.resizing_panel else {
                    return Task::none();
                };
                let current_width = match kind {
                    ResizePanel::ChannelList => self.channel_list_width,
                    ResizePanel::Members => self.members_panel_preferred_width,
                };
                let Some((anchor_x, anchor_width)) = self.resize_drag_anchor else {
                    self.resize_drag_anchor = Some((x, current_width));
                    return Task::none();
                };
                let delta = x - anchor_x;
                match kind {
                    ResizePanel::ChannelList => {
                        self.channel_list_width = (anchor_width + delta).clamp(180.0, 420.0);
                    }
                    ResizePanel::Members => {
                        let w = (anchor_width - delta).clamp(180.0, 360.0);
                        self.members_panel_preferred_width = w;
                        self.members_panel_width = w;
                        self.members_panel_target = w;
                    }
                }
                Task::none()
            }
            Message::PanelResizeEnded => {
                self.resizing_panel = None;
                self.resize_drag_anchor = None;
                save_panel_prefs(self.channel_list_width, self.members_panel_preferred_width);
                Task::none()
            }

            Message::NewBotNameChanged(v) => {
                self.new_bot_name_input = v;
                Task::none()
            }
            Message::BotInviteUsernameChanged(v) => {
                self.bot_invite_username_input = v;
                Task::none()
            }
            Message::DismissBotToken => {
                self.bot_token_reveal = None;
                Task::none()
            }
            Message::CreateBot => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let name = self.new_bot_name_input.trim().to_string();
                if name.len() < 2 {
                    self.bot_status = Some("Bot needs a name (min 2 chars)".into());
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "bots:create",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "name".to_string() => Value::String(name.clone()),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        match result {
                            FunctionResult::Value(Value::Object(obj)) => {
                                let token = obj_str(&obj, "token");
                                let uname = obj_str(&obj, "username");
                                Ok((format!("{name} (@{uname})"), token))
                            }
                            FunctionResult::ErrorMessage(e) => Err(humanize_error(&e)),
                            FunctionResult::ConvexError(e) => {
                                Err(humanize_error(&format!("{e:?}")))
                            }
                            _ => Err("Unexpected response".into()),
                        }
                    },
                    Message::BotCreateFinished,
                )
            }
            Message::BotCreateFinished(Ok((label, token))) => {
                self.new_bot_name_input.clear();
                self.bot_token_reveal = Some(token);
                self.bot_status = Some(format!("Created {label} — copy token now!"));
                self.show_toast("Bot created");
                // refresh list
                let client = self.client.clone();
                let session = self.session.clone();
                if let (Some(mut client), Some(session)) = (client, session) {
                    return Task::perform(
                        async move {
                            let result = client
                                .query(
                                    "bots:listMine",
                                    btreemap! {
                                        "sessionToken".to_string() => Value::String(session.token),
                                    },
                                )
                                .await
                                .ok();
                            parse_object_array(
                                result.unwrap_or(FunctionResult::Value(Value::Array(vec![]))),
                            )
                            .into_iter()
                            .map(|obj| BotSummary {
                                bot_id: obj_str(&obj, "botId"),
                                username: obj_str(&obj, "username"),
                                display_name: obj_str(&obj, "displayName"),
                                avatar_color: obj_str(&obj, "avatarColor"),
                            })
                            .collect::<Vec<_>>()
                        },
                        Message::MyBotsUpdated,
                    );
                }
                Task::none()
            }
            Message::BotCreateFinished(Err(err)) => {
                self.bot_status = Some(err);
                Task::none()
            }
            Message::RefreshMyBots => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = match client
                            .query(
                                "bots:listMine",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                        {
                            Ok(r) => r,
                            Err(_) => {
                                return Vec::<BotSummary>::new();
                            }
                        };
                        parse_object_array(result)
                            .into_iter()
                            .map(|obj| BotSummary {
                                bot_id: obj_str(&obj, "botId"),
                                username: obj_str(&obj, "username"),
                                display_name: obj_str(&obj, "displayName"),
                                avatar_color: obj_str(&obj, "avatarColor"),
                            })
                            .collect::<Vec<BotSummary>>()
                    },
                    Message::MyBotsUpdated,
                )
            }
            Message::MyBotsUpdated(bots) => {
                self.my_bots = bots;
                Task::none()
            }
            Message::InviteBotToServer => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(server) = self.selected_server.clone() else {
                    self.bot_status = Some("Open a server first".into());
                    return Task::none();
                };
                let username = self.bot_invite_username_input.trim().to_string();
                if username.is_empty() {
                    return Task::none();
                }
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "bots:inviteToServer",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "serverId".to_string() => Value::String(server.server_id),
                                    "botUsername".to_string() => Value::String(username),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_string)
                            .map(|_| ())
                    },
                    Message::InviteBotFinished,
                )
            }
            Message::InviteBotFinished(Ok(())) => {
                self.bot_invite_username_input.clear();
                self.show_toast("Bot invited to server");
                Task::none()
            }
            Message::InviteBotFinished(Err(err)) => {
                self.bot_status = Some(err);
                Task::none()
            }
            Message::RegenerateBotToken(bot_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "bots:regenerateToken",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "botId".to_string() => Value::String(bot_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        match result {
                            FunctionResult::Value(Value::Object(obj)) => Ok(obj_str(&obj, "token")),
                            FunctionResult::ErrorMessage(e) => Err(humanize_error(&e)),
                            FunctionResult::ConvexError(e) => {
                                Err(humanize_error(&format!("{e:?}")))
                            }
                            _ => Err("Unexpected response".into()),
                        }
                    },
                    Message::BotTokenFinished,
                )
            }
            Message::BotTokenFinished(Ok(token)) => {
                self.bot_token_reveal = Some(token);
                self.bot_status = Some("New token ready — copy it now".into());
                Task::none()
            }
            Message::BotTokenFinished(Err(err)) => {
                self.bot_status = Some(err);
                Task::none()
            }
            Message::DeleteBot(bot_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "bots:destroy",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "botId".to_string() => Value::String(bot_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null)
                    },
                    Message::DeleteBotFinished,
                )
            }
            Message::DeleteBotFinished(Ok(())) => {
                self.show_toast("Bot deleted");
                Task::done(Message::RefreshMyBots)
            }
            Message::DeleteBotFinished(Err(err)) => {
                self.bot_status = Some(err);
                Task::none()
            }

            Message::Tick => {
                if let Some((_, since)) = &self.toast {
                    if since.elapsed() >= Duration::from_secs(3) {
                        self.toast = None;
                    }
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "presence:heartbeat",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    |_| Message::HeartbeatFinished,
                )
            }
            Message::HeartbeatFinished => Task::none(),

            Message::MyCallUpdated(info) => {
                let was_ringing = self
                    .my_call
                    .as_ref()
                    .map(|c| c.status == "ringing" && !c.is_caller)
                    .unwrap_or(false);
                let now_ringing = info
                    .as_ref()
                    .map(|c| c.status == "ringing" && !c.is_caller)
                    .unwrap_or(false);
                if !was_ringing && now_ringing {
                    ringtone_start();
                    if let Some(call) = &info {
                        notify_desktop(
                            "Incoming call",
                            &format!("{} is calling you", call.peer_display_name),
                        );
                    }
                }
                if was_ringing && !now_ringing {
                    ringtone_stop();
                }
                if info.is_none() {
                    self.call_role = None;
                    self.call_engine_key = None;
                    self.call_status_text = None;
                    self.clear_share_ui();
                }
                self.my_call = info;
                Task::none()
            }
            Message::StartCall => {
                let Some(conversation_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                let Some(callee_id) = self.active_conversation_peer_id.clone() else {
                    return Task::none();
                };
                if self.active_conversation_kind.as_deref() != Some("direct") {
                    return Task::none();
                }
                if self.my_call.is_some() {
                    return Task::none();
                }
                self.call_muted = Arc::new(AtomicBool::new(false));
                self.call_output_muted = Arc::new(AtomicBool::new(false));
                self.call_status_text = Some("Calling...".to_string());
                self.call_role = Some(CallRole::Caller {
                    conversation_id,
                    callee_id,
                });
                self.call_engine_key =
                    Some(format!("caller-{}", chrono::Utc::now().timestamp_millis()));
                self.reset_share_state();
                Task::none()
            }
            Message::AcceptCall => {
                let Some(call) = self.my_call.clone() else {
                    return Task::none();
                };
                if call.is_caller {
                    return Task::none();
                }
                self.call_muted = Arc::new(AtomicBool::new(false));
                self.call_output_muted = Arc::new(AtomicBool::new(false));
                self.call_status_text = Some("Connecting...".to_string());
                self.call_role = Some(CallRole::Callee {
                    call_id: call.call_id.clone(),
                    offer_sdp: call.offer_sdp,
                });
                self.call_engine_key = Some(format!("callee-{}", call.call_id));
                self.reset_share_state();
                Task::none()
            }
            Message::DeclineCall => {
                let Some(call) = self.my_call.clone() else {
                    return Task::none();
                };
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "calls:respond",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "callId".to_string() => Value::String(call.call_id),
                                    "accept".to_string() => Value::Boolean(false),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::CallActionFinished,
                )
            }
            Message::HangUp => {
                self.call_role = None;
                self.call_engine_key = None;
                self.call_status_text = None;
                self.clear_share_ui();
                let Some(call) = self.my_call.clone() else {
                    return Task::none();
                };
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "calls:endCall",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "callId".to_string() => Value::String(call.call_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::CallActionFinished,
                )
            }
            Message::ToggleMute => {
                let current = self.call_muted.load(Ordering::Relaxed);
                self.call_muted.store(!current, Ordering::Relaxed);
                Task::none()
            }
            Message::ToggleMuteAll => {
                let both_muted = self.call_muted.load(Ordering::Relaxed)
                    && self.call_output_muted.load(Ordering::Relaxed);
                let next = !both_muted;
                self.call_muted.store(next, Ordering::Relaxed);
                self.call_output_muted.store(next, Ordering::Relaxed);
                Task::none()
            }
            Message::CallActionFinished(Err(err)) => {
                self.call_status_text = Some(err.clone());
                self.chat_error = Some(err);
                Task::none()
            }
            Message::CallActionFinished(Ok(())) => Task::none(),
            Message::CallEngineEvent(event) => {
                match event {
                    call::CallEvent::Created => {}
                    call::CallEvent::Connecting => {
                        self.call_status_text = Some("Connecting...".to_string());
                    }
                    call::CallEvent::Connected => {
                        self.call_status_text = Some("Connected".to_string());
                    }
                    call::CallEvent::Ended => {
                        self.call_role = None;
                        self.call_engine_key = None;
                        self.call_status_text = None;
                        self.clear_share_ui();
                    }
                    call::CallEvent::Failed(msg) => {
                        self.call_status_text = Some(format!("Call failed: {msg}"));
                        // The call banner only renders when my_call.is_some(),
                        // which is false for a caller whose startCall never went
                        // through -- surface the failure in the chat area too,
                        // or clicking Call looks completely dead.
                        self.chat_error = Some(format!("Call failed: {msg}"));
                        self.call_role = None;
                        self.call_engine_key = None;
                        self.clear_share_ui();
                        // Always tear down the server-side call row. Without this,
                        // a timed-out/failed attempt leaves status=ringing|active and
                        // blocks the next call with "You're already in a call".
                        if let (Some(call), Some(client), Some(session)) = (
                            self.my_call.clone(),
                            self.client.clone(),
                            self.session.clone(),
                        ) {
                            let mut client = client;
                            return Task::perform(
                                async move {
                                    let _ = client
                                        .mutation(
                                            "calls:endCall",
                                            btreemap! {
                                                "sessionToken".to_string() => Value::String(session.token),
                                                "callId".to_string() => Value::String(call.call_id),
                                            },
                                        )
                                        .await;
                                },
                                |_| Message::CallActionFinished(Ok(())),
                            );
                        }
                    }
                    call::CallEvent::ScreenFrame(bytes) => {
                        self.remote_share_frame = Some(Arc::from(bytes));
                    }
                    call::CallEvent::ScreenShareStopped => {
                        self.remote_share_frame = None;
                        self.share_stats_line.clear();
                    }
                    call::CallEvent::ScreenShareFailed(msg) => {
                        self.is_sharing = false;
                        self.share_stats_line.clear();
                        self.chat_error = Some(msg);
                    }
                    call::CallEvent::ShareStats {
                        fps,
                        kbps,
                        last_frame_bytes,
                        system_audio,
                    } => {
                        let audio = if system_audio { " · sys audio" } else { "" };
                        self.share_stats_line = format!(
                            "{fps:.1} fps · {kbps:.0} kbps · {:.0} KB/frame{audio}",
                            last_frame_bytes as f32 / 1024.0
                        );
                    }
                    call::CallEvent::PeerMuteStream(muted) => {
                        // Peer muted our outbound system audio.
                        if let Some(tx) = &self.share_control_tx {
                            let _ = tx.send(call::ShareCommand::SetSystemAudio(!muted));
                        }
                        self.share_system_audio = !muted;
                        if muted {
                            self.chat_error = Some("Peer muted your stream audio".into());
                        }
                    }
                }
                Task::none()
            }

            Message::ToggleSharePicker => {
                self.share_picker_open = !self.share_picker_open;
                if self.share_picker_open {
                    Task::perform(
                        async {
                            tokio::task::spawn_blocking(screenshare::list_share_targets)
                                .await
                                .unwrap_or_default()
                        },
                        Message::ShareTargetsLoaded,
                    )
                } else {
                    Task::none()
                }
            }
            Message::ShareTargetsLoaded(targets) => {
                self.share_targets = targets;
                Task::none()
            }
            Message::StartShare(encoded) => {
                let Some(target) = crate::ui::viewmodel::decode_share_target(&encoded) else {
                    return Task::none();
                };
                self.share_picker_open = false;
                if let Some(tx) = &self.share_control_tx {
                    let _ = tx.send(call::ShareCommand::Start {
                        target,
                        include_system_audio: self.share_system_audio,
                    });
                    self.is_sharing = true;
                }
                Task::none()
            }
            Message::StopShare => {
                self.is_sharing = false;
                self.share_stats_line.clear();
                if let Some(tx) = &self.share_control_tx {
                    let _ = tx.send(call::ShareCommand::Stop);
                }
                Task::none()
            }
            Message::ToggleStreamMute => {
                self.remote_stream_muted = !self.remote_stream_muted;
                if let Some(tx) = &self.share_control_tx {
                    let _ = tx.send(call::ShareCommand::SetRemoteStreamMuted(
                        self.remote_stream_muted,
                    ));
                }
                Task::none()
            }
            Message::ToggleShareSystemAudio => {
                self.share_system_audio = !self.share_system_audio;
                if self.is_sharing {
                    if let Some(tx) = &self.share_control_tx {
                        let _ =
                            tx.send(call::ShareCommand::SetSystemAudio(self.share_system_audio));
                    }
                }
                Task::none()
            }
            Message::ToggleChannelMute => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let Some(conv_id) = self.active_conversation.clone() else {
                    return Task::none();
                };
                let currently_muted = self
                    .channels
                    .iter()
                    .find(|c| c.conversation_id == conv_id)
                    .map(|c| c.muted)
                    .unwrap_or(false);
                let muted = !currently_muted;
                // Optimistic UI until listChannels subscription refreshes.
                if let Some(ch) = self
                    .channels
                    .iter_mut()
                    .find(|c| c.conversation_id == conv_id)
                {
                    ch.muted = muted;
                }
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .mutation(
                                "channels:setMute",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "scope".to_string() => Value::String("conversation".into()),
                                    "targetId".to_string() => Value::String(conv_id),
                                    "muted".to_string() => Value::Boolean(muted),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        expect_null(result).map_err(|e| humanize_error(&e))?;
                        Ok(muted)
                    },
                    |r| match r {
                        Ok(m) => Message::ChannelMuteFinished(Ok(m)),
                        Err(e) => Message::ChannelMuteFinished(Err(e)),
                    },
                )
            }
            Message::ChannelMuteFinished(Ok(muted)) => {
                self.show_toast(if muted {
                    "Channel muted"
                } else {
                    "Channel unmuted"
                });
                Task::none()
            }
            Message::ChannelMuteFinished(Err(err)) => {
                self.chat_error = Some(err);
                Task::none()
            }
            Message::ToggleShareViewSize => {
                self.share_view_expanded = !self.share_view_expanded;
                Task::none()
            }
            Message::OpenAttachmentPreview(url) => {
                self.attachment_preview_url = Some(url);
                Task::none()
            }
            Message::CloseAttachmentPreview => {
                self.attachment_preview_url = None;
                Task::none()
            }
            Message::OpenCommandPalette => {
                if self.session.is_none() {
                    return Task::none();
                }
                self.command_palette_open = true;
                self.command_palette_query.clear();
                self.command_palette_hits.clear();
                Task::none()
            }
            Message::CloseCommandPalette => {
                self.command_palette_open = false;
                self.command_palette_query.clear();
                self.command_palette_hits.clear();
                Task::none()
            }
            Message::CommandPaletteQueryChanged(q) => {
                self.command_palette_query = q.clone();
                if q.trim().len() < 2 {
                    self.command_palette_hits.clear();
                    return Task::none();
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let result = client
                            .query(
                                "messages:search",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "query".to_string() => Value::String(q),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))?;
                        let rows = parse_object_array(result);
                        let hits = rows
                            .into_iter()
                            .map(|obj| {
                                let conv = obj_str(&obj, "conversationId");
                                let name = obj_str(&obj, "conversationName");
                                let author = obj_str(&obj, "authorName");
                                let body = obj_str(&obj, "body");
                                let mid = obj_str(&obj, "messageId");
                                let line = format!("#{name} · {author}: {body}");
                                (conv, line, mid)
                            })
                            .collect::<Vec<_>>();
                        Ok(hits)
                    },
                    Message::CommandPaletteSearchFinished,
                )
            }
            Message::CommandPaletteSearchFinished(Ok(hits)) => {
                if self.command_palette_open {
                    self.command_palette_hits = hits;
                }
                Task::none()
            }
            Message::CommandPaletteSearchFinished(Err(err)) => {
                self.show_toast(err);
                Task::none()
            }
            Message::CommandPalettePick(idx) => {
                if let Some((conv_id, _, _)) = self.command_palette_hits.get(idx).cloned() {
                    self.command_palette_open = false;
                    self.command_palette_query.clear();
                    self.command_palette_hits.clear();
                    // Prefer channel open when the hit is a server channel we know;
                    // otherwise open as a DM/group conversation.
                    if self.channels.iter().any(|c| c.conversation_id == conv_id) {
                        return self.update(Message::OpenChannel(conv_id));
                    }
                    return self.update(Message::OpenConversationDirect(conv_id));
                }
                Task::none()
            }
            Message::EscapePressed => {
                if self.command_palette_open {
                    self.command_palette_open = false;
                    self.command_palette_query.clear();
                    self.command_palette_hits.clear();
                } else if !self.mention_suggestions.is_empty() {
                    self.mention_suggestions.clear();
                } else if self.attachment_preview_url.is_some() {
                    self.attachment_preview_url = None;
                } else if self.clear_chat_confirm {
                    self.clear_chat_confirm = false;
                } else if self.share_picker_open {
                    self.share_picker_open = false;
                } else if self.editing_message_id.is_some() {
                    self.editing_message_id = None;
                    self.message_input.clear();
                    self.mention_suggestions.clear();
                } else if self.pending_reply.is_some() {
                    self.pending_reply = None;
                } else if self.pending_attachment.is_some() {
                    self.pending_attachment = None;
                } else if self.new_group_open {
                    self.new_group_open = false;
                    self.group_create_status = None;
                } else if self.new_channel_open {
                    self.new_channel_open = false;
                    self.new_channel_name_input.clear();
                } else if self.viewing_profile.is_some() || self.profile_error.is_some() {
                    self.viewing_profile = None;
                    self.profile_error = None;
                } else if self.settings_open {
                    self.settings_open = false;
                } else if self.server_settings_open {
                    self.server_settings_open = false;
                    self.confirm_delete_server = false;
                } else if self.toast.is_some() {
                    self.toast = None;
                } else if !self.chat_filter_input.is_empty()
                    || !self.friends_filter_input.is_empty()
                {
                    self.chat_filter_input.clear();
                    self.friends_filter_input.clear();
                } else if self.chat_error.is_some() {
                    self.chat_error = None;
                }
                Task::none()
            }

            Message::OpenProfile(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                self.viewing_profile = None;
                self.profile_error = None;
                self.settings_open = false;
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .query(
                                "profile:getProfile",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(parse_profile_view)
                    },
                    Message::ProfileLoaded,
                )
            }
            Message::ProfileLoaded(Ok(profile)) => {
                let avatar_url = profile.avatar_image_url.clone();
                self.viewing_profile = Some(profile);
                self.fetch_missing_avatars(std::iter::once(avatar_url))
            }
            Message::ProfileLoaded(Err(err)) => {
                self.profile_error = Some(err);
                Task::none()
            }
            Message::CloseProfile => {
                self.viewing_profile = None;
                self.profile_error = None;
                Task::none()
            }

            Message::OpenSettings => {
                self.viewing_profile = None;
                if let Some(session) = &self.session {
                    self.settings_display_name_input = session.display_name.clone();
                    self.settings_status_input = session.status_message.clone();
                    self.settings_bio_input = session.bio.clone();
                    self.settings_avatar_color = if session.avatar_color.is_empty() {
                        AVATAR_PALETTE[0].to_string()
                    } else {
                        session.avatar_color.clone()
                    };
                }
                self.settings_profile_status = None;
                self.settings_password_status = None;
                self.settings_current_password_input.clear();
                self.settings_new_password_input.clear();
                self.settings_confirm_password_input.clear();
                self.settings_input_devices = call::list_input_devices();
                self.settings_output_devices = call::list_output_devices();
                self.settings_open = true;
                self.settings_category = SettingsCategory::Account;
                self.bot_status = None;
                Task::batch([Task::done(Message::RefreshMyBots)])
            }
            Message::CloseSettings => {
                self.settings_open = false;
                Task::none()
            }
            Message::SettingsCategoryChanged(category) => {
                self.settings_category = category;
                if category == SettingsCategory::Bots {
                    return Task::done(Message::RefreshMyBots);
                }
                Task::none()
            }
            Message::SettingsDisplayNameChanged(value) => {
                self.settings_display_name_input = value;
                Task::none()
            }
            Message::SettingsStatusChanged(value) => {
                self.settings_status_input = value;
                Task::none()
            }
            Message::SettingsBioChanged(value) => {
                self.settings_bio_input = value;
                Task::none()
            }
            Message::SettingsAvatarColorSelected(color) => {
                self.settings_avatar_color = color;
                Task::none()
            }
            Message::SaveProfile => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let display_name = self.settings_display_name_input.trim().to_string();
                if display_name.is_empty() {
                    self.settings_profile_status = Some("Display name can't be empty".to_string());
                    return Task::none();
                }
                let status_message = self.settings_status_input.trim().to_string();
                let bio = self.settings_bio_input.trim().to_string();
                let avatar_color = self.settings_avatar_color.clone();
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "profile:updateProfile",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "displayName".to_string() => Value::String(display_name),
                                    "statusMessage".to_string() => Value::String(status_message),
                                    "bio".to_string() => Value::String(bio),
                                    "avatarColor".to_string() => Value::String(avatar_color),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::ProfileSaveFinished,
                )
            }
            Message::ProfileSaveFinished(Ok(())) => {
                if let Some(session) = &mut self.session {
                    session.display_name = self.settings_display_name_input.trim().to_string();
                    session.status_message = self.settings_status_input.trim().to_string();
                    session.bio = self.settings_bio_input.trim().to_string();
                    session.avatar_color = self.settings_avatar_color.clone();
                }
                self.settings_profile_status = Some("Profile saved".to_string());
                Task::none()
            }
            Message::ProfileSaveFinished(Err(err)) => {
                self.settings_profile_status = Some(err);
                Task::none()
            }
            Message::SettingsCurrentPasswordChanged(value) => {
                self.settings_current_password_input = value;
                Task::none()
            }
            Message::SettingsNewPasswordChanged(value) => {
                self.settings_new_password_input = value;
                Task::none()
            }
            Message::SettingsConfirmPasswordChanged(value) => {
                self.settings_confirm_password_input = value;
                Task::none()
            }
            Message::ChangePassword => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                if self.settings_current_password_input.is_empty() {
                    self.settings_password_status = Some("Enter your current password".to_string());
                    return Task::none();
                }
                if self.settings_new_password_input.len() < 6 {
                    self.settings_password_status =
                        Some("New password must be at least 6 characters".to_string());
                    return Task::none();
                }
                if self.settings_new_password_input != self.settings_confirm_password_input {
                    self.settings_password_status = Some("Passwords don't match".to_string());
                    return Task::none();
                }
                if self.settings_new_password_input == self.settings_current_password_input {
                    self.settings_password_status =
                        Some("New password must be different from the current one".to_string());
                    return Task::none();
                }
                let current_password = self.settings_current_password_input.clone();
                let new_password = self.settings_new_password_input.clone();
                self.settings_password_status = Some("Changing password…".to_string());
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .action(
                                "auth:changePassword",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "currentPassword".to_string() => Value::String(current_password),
                                    "newPassword".to_string() => Value::String(new_password),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::PasswordChangeFinished,
                )
            }
            Message::PasswordChangeFinished(Ok(())) => {
                self.settings_current_password_input.clear();
                self.settings_new_password_input.clear();
                self.settings_confirm_password_input.clear();
                self.settings_password_status = Some("Password changed".to_string());
                self.show_toast("Password changed");
                Task::none()
            }
            Message::PasswordChangeFinished(Err(err)) => {
                self.settings_password_status = Some(humanize_error(&err));
                Task::none()
            }
            Message::SettingsInputDeviceSelected(name) => {
                self.settings_input_device = if name.is_empty() { None } else { Some(name) };
                self.persist_settings();
                Task::none()
            }
            Message::SettingsOutputDeviceSelected(name) => {
                self.settings_output_device = if name.is_empty() { None } else { Some(name) };
                self.persist_settings();
                Task::none()
            }
            Message::NoiseGateChanged(value) => {
                self.noise_gate.store(value.to_bits(), Ordering::Relaxed);
                self.persist_settings();
                Task::none()
            }

            Message::AvatarImageLoaded(url, Ok(bytes)) => {
                self.avatar_image_cache.insert(url, Arc::from(bytes));
                Task::none()
            }
            Message::AvatarImageLoaded(_, Err(_)) => Task::none(),
            Message::PickAvatarImage => {
                if self.avatar_upload_busy {
                    return Task::none();
                }
                self.avatar_upload_busy = true;
                self.settings_profile_status = None;
                Task::perform(
                    async move {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .add_filter("Images", &["png", "jpg", "jpeg"])
                            .pick_file()
                            .await
                        else {
                            return AvatarPick::Cancelled;
                        };
                        let bytes = file.read().await;
                        if bytes.len() > 2 * 1024 * 1024 {
                            return AvatarPick::TooLarge;
                        }
                        let name = file.file_name().to_lowercase();
                        let content_type = if name.ends_with(".png") {
                            "image/png"
                        } else {
                            "image/jpeg"
                        };
                        AvatarPick::Ready(bytes, content_type.to_string())
                    },
                    Message::AvatarFilePicked,
                )
            }
            Message::AvatarFilePicked(AvatarPick::Cancelled) => {
                self.avatar_upload_busy = false;
                Task::none()
            }
            Message::AvatarFilePicked(AvatarPick::TooLarge) => {
                self.avatar_upload_busy = false;
                self.settings_profile_status = Some("Image must be smaller than 2MB".to_string());
                Task::none()
            }
            Message::AvatarFilePicked(AvatarPick::Ready(bytes, content_type)) => {
                let Some(client) = self.client.clone() else {
                    self.avatar_upload_busy = false;
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    self.avatar_upload_busy = false;
                    return Task::none();
                };
                self.settings_profile_status = Some("Uploading...".to_string());
                let mut client = client;
                Task::perform(
                    async move {
                        let upload_url = client
                            .mutation(
                                "profile:generateAvatarUploadUrl",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token.clone()),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)?;

                        let http = reqwest::Client::new();
                        let response = http
                            .post(&upload_url)
                            .header("Content-Type", content_type)
                            .body(bytes)
                            .send()
                            .await
                            .map_err(|err| err.to_string())?;

                        #[derive(serde::Deserialize)]
                        struct UploadResponse {
                            #[serde(rename = "storageId")]
                            storage_id: String,
                        }
                        let parsed: UploadResponse =
                            response.json().await.map_err(|err| err.to_string())?;

                        client
                            .mutation(
                                "profile:setAvatarImage",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "storageId".to_string() => Value::String(parsed.storage_id),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_string)
                    },
                    Message::AvatarUploadFinished,
                )
            }
            Message::AvatarUploadFinished(Ok(url)) => {
                self.avatar_upload_busy = false;
                self.settings_profile_status = Some("Photo updated".to_string());
                if let Some(session) = &mut self.session {
                    session.avatar_image_url = url.clone();
                }
                self.fetch_missing_avatars(std::iter::once(url))
            }
            Message::AvatarUploadFinished(Err(err)) => {
                self.avatar_upload_busy = false;
                self.settings_profile_status = Some(err);
                Task::none()
            }
            Message::RemoveAvatarImage => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        client
                            .mutation(
                                "profile:removeAvatarImage",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(expect_null)
                    },
                    Message::AvatarRemoveFinished,
                )
            }
            Message::AvatarRemoveFinished(Ok(())) => {
                if let Some(session) = &mut self.session {
                    session.avatar_image_url.clear();
                }
                self.settings_profile_status = Some("Photo removed".to_string());
                Task::none()
            }
            Message::AvatarRemoveFinished(Err(err)) => {
                self.settings_profile_status = Some(err);
                Task::none()
            }

            Message::TypingUpdated(names) => {
                self.typing_names = names;
                Task::none()
            }
            Message::TypingPingFinished => Task::none(),

            Message::LogOut => {
                clear_session_file();
                let Some(client) = self.client.clone() else {
                    self.reset_session();
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    self.reset_session();
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let _ = client
                            .mutation(
                                "auth:signOut",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                },
                            )
                            .await;
                    },
                    |_| Message::LoggedOut,
                )
            }
            Message::LoggedOut => {
                self.reset_session();
                Task::none()
            }

            Message::SetAdminFilter(filter) => {
                self.admin_filter = filter;
                Task::none()
            }
            Message::ToggleAdminUserDetail(user_id) => {
                if self
                    .admin_user_detail
                    .as_ref()
                    .map(|d| d.user_id == user_id)
                    .unwrap_or(false)
                {
                    self.admin_user_detail = None;
                    return Task::none();
                }
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let detail = client
                            .query(
                                "admin:adminUserDetail",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .ok()
                            .and_then(parse_admin_user_detail);
                        Message::AdminUserDetailUpdated(detail)
                    },
                    std::convert::identity,
                )
            }
            Message::AdminUserDetailUpdated(detail) => {
                self.admin_user_detail = detail;
                Task::none()
            }
            Message::AdminRevokeSessions(user_id) => {
                let Some(client) = self.client.clone() else {
                    return Task::none();
                };
                let Some(session) = self.session.clone() else {
                    return Task::none();
                };
                let mut client = client;
                Task::perform(
                    async move {
                        let res = client
                            .mutation(
                                "admin:adminRevokeSessions",
                                btreemap! {
                                    "sessionToken".to_string() => Value::String(session.token),
                                    "userId".to_string() => Value::String(user_id),
                                },
                            )
                            .await
                            .map_err(|e| humanize_error(&e.to_string()))
                            .and_then(expect_null);
                        Message::AdminRevokeSessionsFinished(res)
                    },
                    std::convert::identity,
                )
            }
            Message::AdminRevokeSessionsFinished(Err(err)) => {
                self.admin_status = Some(err);
                Task::none()
            }
            Message::AdminRevokeSessionsFinished(Ok(())) => {
                self.show_toast("Sessions revoked");
                Task::none()
            }
        }
    }
}
