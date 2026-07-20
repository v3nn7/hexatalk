//! HexaTalk mobile — **Rust + Slint** (egui removed).
//!
//! Android uses Slint's official android-activity backend so `TextInput`
//! goes through the system IME → long-press Paste works like a normal app.

mod convex_api;

use convex_api::{
    AuthSession, Backend, ChannelRow, ConversationRow, FriendRequestRow, FriendRow, MessageRow,
    NetEvent, ServerRow,
};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

slint::include_modules!();

fn initial_of(s: &str) -> SharedString {
    s.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into())
        .into()
}

fn fmt_time(ms: f64) -> String {
    if ms <= 0.0 {
        return String::new();
    }
    let secs = (ms / 1000.0) as i64;
    let mins = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    format!("{hours:02}:{mins:02}")
}

fn extract_invite_code(input: &str) -> String {
    let trimmed = input.trim();
    let code = match trimmed.rsplit_once("invite/") {
        Some((_, rest)) => rest,
        None => trimmed,
    };
    code.trim_matches('/').to_string()
}

/// App state. UI is driven via `ui_weak` so we never need to Clone AppWindow.
struct State {
    backend: Arc<Backend>,
    session: Option<AuthSession>,
    conversations: Vec<ConversationRow>,
    friends: Vec<FriendRow>,
    incoming: Vec<FriendRequestRow>,
    servers: Vec<ServerRow>,
    channels: Vec<ChannelRow>,
    messages: Vec<MessageRow>,
    active_conv_id: Option<String>,
    active_server_id: Option<String>,
    last_typing: Instant,
    heartbeat_acc: f32,
}

impl State {
    fn ui(ui_weak: &slint::Weak<AppWindow>) -> Option<AppWindow> {
        ui_weak.upgrade()
    }

    fn set_banner(ui: &AppWindow, msg: impl Into<SharedString>, err: bool) {
        ui.set_banner(msg.into());
        ui.set_banner_error(err);
    }

    fn clear_banner(ui: &AppWindow) {
        ui.set_banner(SharedString::default());
        ui.set_banner_error(false);
    }

    fn apply_session(&mut self, ui: &AppWindow, s: AuthSession) {
        save_session(&s);
        ui.set_my_name(s.display_name.clone().into());
        ui.set_my_username(s.username.clone().into());
        ui.set_profile_display(s.display_name.clone().into());
        self.backend.subscribe_home(s.token.clone());
        self.backend
            .register_push_token(s.token.clone(), format!("android-{}", s.user_id));
        self.session = Some(s);
        ui.set_screen(Screen::Home);
        ui.set_busy(false);
        Self::clear_banner(ui);
    }

    fn refresh_conv_model(&self, ui: &AppWindow) {
        let q = ui.get_search().to_string().to_lowercase();
        let rows: Vec<ConvItem> = self
            .conversations
            .iter()
            .filter(|c| q.is_empty() || c.title.to_lowercase().contains(&q))
            .map(|c| ConvItem {
                id: c.id.clone().into(),
                title: c.title.clone().into(),
                kind: c.kind.clone().into(),
                unread: c.unread,
                initial: initial_of(&c.title),
            })
            .collect();
        ui.set_conversations(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh_friends_model(&self, ui: &AppWindow) {
        let q = ui.get_search().to_string().to_lowercase();
        let rows: Vec<FriendItem> = self
            .friends
            .iter()
            .filter(|f| {
                q.is_empty()
                    || f.display_name.to_lowercase().contains(&q)
                    || f.username.to_lowercase().contains(&q)
            })
            .map(|f| {
                let label = if f.nickname.is_empty() {
                    f.display_name.clone()
                } else {
                    f.nickname.clone()
                };
                FriendItem {
                    user_id: f.user_id.clone().into(),
                    label: label.clone().into(),
                    subtitle: format!("@{}", f.username).into(),
                    online: f.online,
                    initial: initial_of(&label),
                }
            })
            .collect();
        ui.set_friends(ModelRc::new(VecModel::from(rows)));

        let inc: Vec<RequestItem> = self
            .incoming
            .iter()
            .map(|r| RequestItem {
                request_id: r.request_id.clone().into(),
                label: format!("{} (@{})", r.from_display_name, r.from_username).into(),
                note: r.note.clone().into(),
            })
            .collect();
        ui.set_incoming(ModelRc::new(VecModel::from(inc)));
    }

    fn refresh_servers_model(&self, ui: &AppWindow) {
        let q = ui.get_search().to_string().to_lowercase();
        let rows: Vec<ServerItem> = self
            .servers
            .iter()
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q))
            .map(|s| ServerItem {
                server_id: s.server_id.clone().into(),
                name: s.name.clone().into(),
                is_owner: s.is_owner,
                invite: s.invite_code.clone().into(),
                initial: initial_of(&s.name),
            })
            .collect();
        ui.set_servers(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh_channels_model(&self, ui: &AppWindow) {
        let rows: Vec<ChannelItem> = self
            .channels
            .iter()
            .map(|c| ChannelItem {
                conversation_id: c.conversation_id.clone().into(),
                name: c.name.clone().into(),
                is_voice: c.channel_type == "voice",
            })
            .collect();
        ui.set_channels(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh_messages_model(&self, ui: &AppWindow) {
        let my_id = self
            .session
            .as_ref()
            .map(|s| s.user_id.as_str())
            .unwrap_or("");
        let rows: Vec<MsgItem> = self
            .messages
            .iter()
            .map(|m| {
                let body = if m.deleted {
                    "Message deleted".into()
                } else if m.encrypted {
                    "Encrypted — open on desktop".into()
                } else {
                    m.body.clone()
                };
                MsgItem {
                    id: m.id.clone().into(),
                    author: m.author_name.clone().into(),
                    body: body.into(),
                    meta: fmt_time(m.sent_at).into(),
                    mine: !my_id.is_empty() && m.author_id == my_id,
                    plus: m.author_plus_active,
                }
            })
            .collect();
        ui.set_messages(ModelRc::new(VecModel::from(rows)));
    }

    fn open_chat(&mut self, ui: &AppWindow, id: String, title: String) {
        self.active_conv_id = Some(id.clone());
        self.messages.clear();
        self.refresh_messages_model(ui);
        ui.set_chat_title(title.into());
        ui.set_draft(SharedString::default());
        ui.set_screen(Screen::Chat);
        if let Some(s) = &self.session {
            self.backend.subscribe_chat(s.token.clone(), id);
        }
    }

    fn go_back(&mut self, ui: &AppWindow) {
        match ui.get_screen() {
            Screen::Chat => {
                self.active_conv_id = None;
                self.messages.clear();
                if let Some(sid) = self.active_server_id.clone() {
                    ui.set_screen(Screen::Server);
                    if let Some(s) = &self.session {
                        self.backend.subscribe_channels(s.token.clone(), sid);
                    }
                } else {
                    ui.set_screen(Screen::Home);
                    if let Some(s) = &self.session {
                        self.backend.subscribe_home(s.token.clone());
                    }
                }
            }
            Screen::Server => {
                self.active_server_id = None;
                self.channels.clear();
                ui.set_screen(Screen::Home);
                if let Some(s) = &self.session {
                    self.backend.subscribe_home(s.token.clone());
                }
            }
            Screen::Profile => ui.set_screen(Screen::Home),
            _ => {}
        }
    }

    fn pump(&mut self, ui: &AppWindow) {
        for ev in self.backend.poll() {
            match ev {
                NetEvent::AuthOk(s) => {
                    log::info!("auth ok user={}", s.username);
                    self.apply_session(ui, s);
                }
                NetEvent::AuthErr(e) => {
                    ui.set_busy(false);
                    let msg = convex_api::clean_error(&e);
                    log::warn!("auth err: {msg}");
                    Self::set_banner(ui, msg, true);
                }
                NetEvent::PasswordResetCodeSent => {
                    ui.set_busy(false);
                    ui.set_password_reset_code_sent(true);
                    Self::set_banner(
                        ui,
                        "If that email is registered, a code is on its way",
                        false,
                    );
                }
                NetEvent::PasswordResetOk => {
                    ui.set_busy(false);
                    ui.set_forgot_password(false);
                    ui.set_password_reset_code_sent(false);
                    ui.set_password("".into());
                    ui.set_password_confirm("".into());
                    ui.set_reset_code("".into());
                    Self::set_banner(ui, "Password updated — sign in", false);
                }
                NetEvent::Conversations(v) => {
                    self.conversations = v;
                    self.refresh_conv_model(ui);
                }
                NetEvent::Friends(v) => {
                    self.friends = v;
                    self.refresh_friends_model(ui);
                }
                NetEvent::FriendRequests(v) => {
                    self.incoming = v;
                    self.refresh_friends_model(ui);
                }
                NetEvent::OutgoingRequests(_) | NetEvent::SocialStats(_) | NetEvent::Suggestions(_) => {}
                NetEvent::Servers(v) => {
                    self.servers = v;
                    self.refresh_servers_model(ui);
                }
                NetEvent::Channels(v) => {
                    self.channels = v;
                    self.refresh_channels_model(ui);
                }
                NetEvent::Messages(v) => {
                    self.messages = v;
                    self.refresh_messages_model(ui);
                }
                NetEvent::Typing(_) => {}
                NetEvent::Status(s) => {
                    ui.set_busy(false);
                    if let Some(id) = s.strip_prefix("OPEN_DM:") {
                        let title = ui.get_chat_title().to_string();
                        let title = if title.is_empty() { "DM".into() } else { title };
                        self.open_chat(ui, id.to_string(), title);
                    } else {
                        if s.contains("Server created") || s.contains("Joined server") {
                            ui.set_new_server(SharedString::default());
                            ui.set_join_code(SharedString::default());
                            if let Some(sess) = &self.session {
                                self.backend.subscribe_home(sess.token.clone());
                            }
                        }
                        if s.contains("Profile saved") {
                            ui.set_screen(Screen::Home);
                        }
                        Self::set_banner(ui, s, false);
                    }
                }
                NetEvent::Error(e) => {
                    ui.set_busy(false);
                    let msg = convex_api::clean_error(&e);
                    if msg.to_lowercase().contains("session expired")
                        || msg.to_lowercase().contains("please log in")
                    {
                        clear_session();
                        self.session = None;
                        ui.set_screen(Screen::Auth);
                    }
                    Self::set_banner(ui, msg, true);
                }
                NetEvent::SentOk => {
                    ui.set_busy(false);
                    ui.set_draft(SharedString::default());
                }
                NetEvent::Profile(p) => {
                    ui.set_profile_display(p.display_name.into());
                    ui.set_profile_status(p.status_message.into());
                    ui.set_profile_bio(p.bio.into());
                }
                NetEvent::MyServerPermissions(_) | NetEvent::SearchResults(_) => {}
            }
        }

        self.heartbeat_acc += 0.12;
        if self.heartbeat_acc > 20.0 {
            self.heartbeat_acc = 0.0;
            if let Some(s) = &self.session {
                self.backend.heartbeat(s.token.clone());
            }
        }
    }
}

fn session_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("hexatalk-mobile").join("session.json")
}

fn save_session(s: &AuthSession) {
    let path = session_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let json = serde_json::json!({
        "token": s.token,
        "user_id": s.user_id,
        "username": s.username,
        "display_name": s.display_name,
        "role": s.role,
    });
    let _ = std::fs::write(path, json.to_string());
}

fn load_session() -> Option<AuthSession> {
    let data = std::fs::read_to_string(session_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    Some(AuthSession {
        token: v.get("token")?.as_str()?.to_string(),
        user_id: v.get("user_id")?.as_str()?.to_string(),
        username: v.get("username")?.as_str()?.to_string(),
        display_name: v.get("display_name")?.as_str()?.to_string(),
        role: v
            .get("role")
            .and_then(|x| x.as_str())
            .unwrap_or("user")
            .to_string(),
    })
}

fn clear_session() {
    let _ = std::fs::remove_file(session_path());
}

fn wire(ui: &AppWindow, ui_weak: slint::Weak<AppWindow>, st: Rc<RefCell<State>>) {
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_submit_auth(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            if ui.get_busy() {
                return;
            }

            // Forgot password flow
            if ui.get_forgot_password() {
                let email = ui.get_email().to_string().trim().to_lowercase();
                if email.is_empty() || !email.contains('@') {
                    State::set_banner(&ui, "Enter a valid email address", true);
                    return;
                }
                ui.set_busy(true);
                State::clear_banner(&ui);
                if !ui.get_password_reset_code_sent() {
                    g.backend.request_password_reset(email);
                    return;
                }
                let code = ui.get_reset_code().to_string().trim().to_string();
                let password = ui.get_password().to_string();
                let confirm = ui.get_password_confirm().to_string();
                if code.len() != 6 {
                    ui.set_busy(false);
                    State::set_banner(&ui, "Enter the 6-digit code", true);
                    return;
                }
                if password.len() < 6 {
                    ui.set_busy(false);
                    State::set_banner(&ui, "Password must be at least 6 characters", true);
                    return;
                }
                if password != confirm {
                    ui.set_busy(false);
                    State::set_banner(&ui, "Passwords don't match", true);
                    return;
                }
                g.backend.reset_password_with_code(email, code, password);
                return;
            }

            let username = ui.get_username().to_string();
            let password = ui.get_password().to_string();
            if username.trim().is_empty() || password.is_empty() {
                State::set_banner(&ui, "Enter username and password", true);
                return;
            }
            ui.set_busy(true);
            State::clear_banner(&ui);
            log::info!("auth submit sign_up={}", ui.get_sign_up());
            if ui.get_sign_up() {
                let dn = ui.get_display_name().to_string();
                let email = ui.get_email().to_string();
                if email.trim().is_empty() {
                    ui.set_busy(false);
                    State::set_banner(&ui, "Email is required to register", true);
                    return;
                }
                g.backend.sign_up(
                    username.trim().to_lowercase(),
                    password,
                    if dn.trim().is_empty() {
                        username.trim().to_string()
                    } else {
                        dn.trim().to_string()
                    },
                    email.trim().to_lowercase(),
                );
            } else {
                g.backend
                    .sign_in(username.trim().to_lowercase(), password);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_toggle_sign_up(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_forgot_password(false);
                ui.set_password_reset_code_sent(false);
                ui.set_sign_up(!ui.get_sign_up());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_toggle_forgot_password(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let next = !ui.get_forgot_password();
                ui.set_forgot_password(next);
                ui.set_sign_up(false);
                ui.set_password_reset_code_sent(false);
                ui.set_reset_code("".into());
                ui.set_password_confirm("".into());
                State::clear_banner(&ui);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        ui.on_set_home_tab(move |tab| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_home_tab(tab);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_open_chat(move |id, title| {
            let Some(ui) = ui_weak.upgrade() else { return };
            st.borrow_mut()
                .open_chat(&ui, id.to_string(), title.to_string());
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_open_dm(move |uid, label| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let g = st.borrow();
            if let Some(s) = &g.session {
                ui.set_chat_title(label);
                State::set_banner(&ui, "Opening DM…", false);
                g.backend.open_dm(s.token.clone(), uid.to_string());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_open_server(move |sid, name| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            g.active_server_id = Some(sid.to_string());
            g.channels.clear();
            g.refresh_channels_model(&ui);
            ui.set_server_name(name);
            ui.set_screen(Screen::Server);
            if let Some(s) = &g.session {
                g.backend
                    .subscribe_channels(s.token.clone(), sid.to_string());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_open_channel(move |cid, name| {
            let Some(ui) = ui_weak.upgrade() else { return };
            st.borrow_mut()
                .open_chat(&ui, cid.to_string(), name.to_string());
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_go_back(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            st.borrow_mut().go_back(&ui);
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_send_message(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            let body = ui.get_draft().to_string();
            if body.trim().is_empty() {
                return;
            }
            let Some(s) = g.session.clone() else { return };
            let Some(id) = g.active_conv_id.clone() else {
                return;
            };
            ui.set_busy(true);
            g.backend
                .send_message(s.token.clone(), id.clone(), body.trim().to_string());
            g.backend.set_typing(s.token, id, false);
        });
    }
    {
        let st = st.clone();
        ui.on_draft_changed(move |text| {
            let mut g = st.borrow_mut();
            if g.last_typing.elapsed() < Duration::from_millis(800) {
                return;
            }
            g.last_typing = Instant::now();
            if let (Some(s), Some(id)) = (g.session.clone(), g.active_conv_id.clone()) {
                g.backend
                    .set_typing(s.token, id, !text.to_string().is_empty());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_send_friend_request(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let g = st.borrow();
            let u = ui.get_add_friend().to_string();
            let note = ui.get_friend_note().to_string();
            if let Some(s) = &g.session {
                let u = u.trim().trim_start_matches('@').to_string();
                if !u.is_empty() {
                    g.backend.send_friend_request(
                        s.token.clone(),
                        u,
                        if note.trim().is_empty() {
                            None
                        } else {
                            Some(note)
                        },
                    );
                    ui.set_add_friend(SharedString::default());
                    ui.set_friend_note(SharedString::default());
                }
            }
        });
    }
    {
        let st = st.clone();
        ui.on_accept_request(move |id| {
            if let Some(s) = &st.borrow().session {
                st.borrow().backend.respond_friend_request(
                    s.token.clone(),
                    id.to_string(),
                    true,
                );
            }
        });
    }
    {
        let st = st.clone();
        ui.on_decline_request(move |id| {
            if let Some(s) = &st.borrow().session {
                st.borrow().backend.respond_friend_request(
                    s.token.clone(),
                    id.to_string(),
                    false,
                );
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_create_server(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            let name = ui.get_new_server().to_string();
            if name.trim().is_empty() {
                State::set_banner(&ui, "Enter a server name", true);
                return;
            }
            if let Some(s) = &g.session {
                ui.set_busy(true);
                g.backend
                    .create_server(s.token.clone(), name.trim().to_string());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_join_server(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            let code = extract_invite_code(&ui.get_join_code());
            if code.is_empty() {
                State::set_banner(&ui, "Enter an invite code", true);
                return;
            }
            if let Some(s) = &g.session {
                ui.set_busy(true);
                g.backend.join_server(s.token.clone(), code);
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_open_profile(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_screen(Screen::Profile);
            if let Some(s) = &st.borrow().session {
                st.borrow()
                    .backend
                    .fetch_profile(s.token.clone(), s.user_id.clone());
            }
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_save_profile(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            let Some(s) = g.session.clone() else { return };
            ui.set_busy(true);
            g.backend.update_profile(
                s.token,
                ui.get_profile_display().to_string(),
                ui.get_profile_status().to_string(),
                ui.get_profile_bio().to_string(),
                "#3FB36B".into(),
            );
        });
    }
    {
        let ui_weak = ui_weak.clone();
        let st = st.clone();
        ui.on_sign_out(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut g = st.borrow_mut();
            clear_session();
            g.session = None;
            g.conversations.clear();
            g.friends.clear();
            g.servers.clear();
            ui.set_screen(Screen::Auth);
            State::clear_banner(&ui);
        });
    }
}

/// Desktop example + Android entry.
pub fn start() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();
    let backend = Arc::new(Backend::new().expect("tokio runtime"));
    backend.ensure_connected();

    let st = Rc::new(RefCell::new(State {
        backend,
        session: None,
        conversations: vec![],
        friends: vec![],
        incoming: vec![],
        servers: vec![],
        channels: vec![],
        messages: vec![],
        active_conv_id: None,
        active_server_id: None,
        last_typing: Instant::now() - Duration::from_secs(10),
        heartbeat_acc: 0.0,
    }));

    wire(&ui, ui_weak.clone(), st.clone());

    if let Some(s) = load_session() {
        if let Some(ui) = ui_weak.upgrade() {
            st.borrow_mut().apply_session(&ui, s);
        }
    }

    let timer = Timer::default();
    {
        let st = st.clone();
        let ui_weak = ui_weak.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(120), move || {
            if let Some(ui) = ui_weak.upgrade() {
                st.borrow_mut().pump(&ui);
            }
        });
    }
    std::mem::forget(timer);

    ui.run()
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    slint::android::init(app).unwrap();
    if let Err(e) = start() {
        log::error!("HexaTalk exit: {e:?}");
    }
}
