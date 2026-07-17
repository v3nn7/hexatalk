//! Talkyss mobile UI — egui immediate mode (dark green, angular).

use crate::convex_api::{
    AuthSession, Backend, ChannelRow, ConversationRow, FriendRow, MessageRow, NetEvent, ServerRow,
};
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin,
    RichText, Stroke, TextStyle, Vec2,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const BG: Color32 = Color32::from_rgb(0x0B, 0x0F, 0x0C);
const PANEL: Color32 = Color32::from_rgb(0x14, 0x1C, 0x16);
const ELEVATED: Color32 = Color32::from_rgb(0x1C, 0x26, 0x1E);
const GREEN: Color32 = Color32::from_rgb(0x3D, 0xFF, 0x7A);
const GREEN_DIM: Color32 = Color32::from_rgb(0x1A, 0x4A, 0x2C);
const GREEN_SOFT: Color32 = Color32::from_rgb(0x14, 0x28, 0x1A);
const TEXT: Color32 = Color32::from_rgb(0xEE, 0xF5, 0xF0);
const MUTED: Color32 = Color32::from_rgb(0x7A, 0x8C, 0x80);
const BORDER: Color32 = Color32::from_rgb(0x2A, 0x38, 0x2E);
const BORDER_FOCUS: Color32 = Color32::from_rgb(0x3A, 0x6B, 0x4A);
const DANGER: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x5C);
const DANGER_BG: Color32 = Color32::from_rgb(0x2A, 0x14, 0x14);
const ONLINE: Color32 = Color32::from_rgb(0x3D, 0xFF, 0x7A);
const BUBBLE_ME: Color32 = Color32::from_rgb(0x16, 0x32, 0x20);
const BUBBLE_THEM: Color32 = Color32::from_rgb(0x18, 0x20, 0x1A);

/// Design width (logical points) everything is tuned against.
const REF_WIDTH: f32 = 390.0;
/// Keep scale close to 1 — readability first; width only nudges slightly.
const MIN_UI_SCALE: f32 = 0.92;
const MAX_UI_SCALE: f32 = 1.08;
/// Extra top inset so content clears status bar / notch (system UI).
const SYS_TOP_PT: f32 = 28.0;
/// Extra bottom inset for gesture / 3-button nav bar.
const SYS_BOTTOM_PT: f32 = 26.0;

/// Pulls the invite code out of a pasted `talkyss://invite/<code>` (or
/// `https://.../invite/<code>`) link, falling back to treating the whole
/// trimmed input as a bare code if it doesn't look like a link. Matches the
/// desktop client's `extract_invite_code`.
fn extract_invite_code(input: &str) -> String {
    let trimmed = input.trim();
    let code = match trimmed.rsplit_once("invite/") {
        Some((_, rest)) => rest,
        None => trimmed,
    };
    code.trim_matches('/').to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Chats,
    Friends,
    Servers,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Auth,
    Home,
    Chat,
    Server,
    Profile,
}

/// Must match `PERM_MANAGE_CHANNELS` in desktop's src/types.rs / convex/roles.ts.
const PERM_MANAGE_CHANNELS: u32 = 1 << 2;

pub struct TalkyssApp {
    backend: Arc<Backend>,
    screen: Screen,
    tab: Tab,
    // auth
    username: String,
    password: String,
    display_name: String,
    /// When false, password is masked (still pasteable via button / events).
    show_password: bool,
    sign_up_mode: bool,
    busy: bool,
    error: Option<String>,
    status: Option<String>,
    // session
    session: Option<AuthSession>,
    // data
    conversations: Vec<ConversationRow>,
    friends: Vec<FriendRow>,
    friend_requests: Vec<crate::convex_api::FriendRequestRow>,
    outgoing_requests: Vec<crate::convex_api::OutgoingRequestRow>,
    suggestions: Vec<crate::convex_api::SuggestionRow>,
    social_stats: crate::convex_api::SocialStats,
    add_friend_user: String,
    add_friend_note: String,
    servers: Vec<ServerRow>,
    messages: Vec<MessageRow>,
    channels: Vec<ChannelRow>,
    typing: Vec<String>,
    // nav
    active_conv_id: Option<String>,
    active_conv_title: String,
    active_server_id: Option<String>,
    active_server_name: String,
    draft: String,
    search: String,
    heartbeat_acc: f32,
    /// Auto-clear transient status toasts.
    status_ttl: f32,
    /// Create / join server fields (Servers tab).
    new_server_name: String,
    join_invite_code: String,
    /// Multiplier for fonts/spacing based on screen size (ref ~390pt wide).
    ui_scale: f32,
    /// Safe outer margin (notch / gesture bar approximation).
    safe_pad: f32,
    // profile editing
    profile_display_name: String,
    profile_status: String,
    profile_bio: String,
    profile_avatar_color: String,
    profile_loaded: bool,
    // server management (gated by my_server_permissions)
    my_server_permissions: u32,
    new_channel_name: String,
    new_channel_is_voice: bool,
    renaming_channel_id: Option<String>,
    rename_channel_input: String,
}

impl TalkyssApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_app_fonts(&cc.egui_ctx);

        let backend = Backend::new().expect("tokio runtime");
        backend.ensure_connected();

        let mut app = Self {
            backend: Arc::new(backend),
            screen: Screen::Auth,
            tab: Tab::Chats,
            username: String::new(),
            password: String::new(),
            display_name: String::new(),
            show_password: false,
            sign_up_mode: false,
            busy: false,
            error: None,
            status: None,
            session: None,
            conversations: vec![],
            friends: vec![],
            friend_requests: vec![],
            outgoing_requests: vec![],
            suggestions: vec![],
            social_stats: crate::convex_api::SocialStats::default(),
            add_friend_user: String::new(),
            add_friend_note: String::new(),
            servers: vec![],
            messages: vec![],
            channels: vec![],
            typing: vec![],
            active_conv_id: None,
            active_conv_title: String::new(),
            active_server_id: None,
            active_server_name: String::new(),
            draft: String::new(),
            search: String::new(),
            heartbeat_acc: 0.0,
            status_ttl: 0.0,
            new_server_name: String::new(),
            join_invite_code: String::new(),
            ui_scale: 1.0,
            safe_pad: 8.0,
            profile_display_name: String::new(),
            profile_status: String::new(),
            profile_bio: String::new(),
            profile_avatar_color: crate::convex_api::AVATAR_PALETTE[0].to_string(),
            profile_loaded: false,
            my_server_permissions: 0,
            new_channel_name: String::new(),
            new_channel_is_voice: false,
            renaming_channel_id: None,
            rename_channel_input: String::new(),
        };

        if let Some(s) = load_session() {
            app.session = Some(s.clone());
            app.screen = Screen::Home;
            app.backend.subscribe_home(s.token);
        }
        app
    }

    fn apply_events(&mut self) {
        for ev in self.backend.poll() {
            match ev {
                NetEvent::AuthOk(s) => {
                    save_session(&s);
                    self.session = Some(s.clone());
                    self.screen = Screen::Home;
                    self.busy = false;
                    self.error = None;
                    self.backend.subscribe_home(s.token);
                }
                NetEvent::AuthErr(e) => {
                    self.busy = false;
                    self.error = Some(crate::convex_api::clean_error(&e));
                }
                NetEvent::Conversations(v) => {
                    self.conversations = v;
                    // Successful home load — clear stale auth lockout banners.
                    if self
                        .error
                        .as_ref()
                        .is_some_and(|e| e.contains("failed attempts") || e.contains("Session expired"))
                    {
                        self.error = None;
                    }
                }
                NetEvent::Messages(v) => self.messages = v,
                NetEvent::Friends(v) => self.friends = v,
                NetEvent::FriendRequests(v) => self.friend_requests = v,
                NetEvent::OutgoingRequests(v) => self.outgoing_requests = v,
                NetEvent::SocialStats(s) => self.social_stats = s,
                NetEvent::Suggestions(v) => self.suggestions = v,
                NetEvent::Servers(v) => self.servers = v,
                NetEvent::Channels(v) => self.channels = v,
                NetEvent::Typing(v) => self.typing = v,
                NetEvent::Status(s) => {
                    self.busy = false;
                    if let Some(id) = s.strip_prefix("OPEN_DM:") {
                        self.open_chat(id.to_string(), "DM".into());
                    } else {
                        // After create/join server, stay on home and refresh.
                        if s.contains("Server created") || s.contains("Joined server") {
                            self.new_server_name.clear();
                            self.join_invite_code.clear();
                            if let Some(sess) = &self.session {
                                self.backend.subscribe_home(sess.token.clone());
                            }
                        }
                        if (s.contains("Channel created")
                            || s.contains("Channel renamed")
                            || s.contains("Channel deleted"))
                        {
                            self.new_channel_name.clear();
                            self.renaming_channel_id = None;
                            if let (Some(sess), Some(server_id)) =
                                (&self.session, &self.active_server_id)
                            {
                                self.backend
                                    .subscribe_channels(sess.token.clone(), server_id.clone());
                            }
                        }
                        if s.contains("Profile saved") {
                            self.screen = Screen::Home;
                        }
                        self.status = Some(s);
                        self.status_ttl = 4.0;
                    }
                }
                NetEvent::Error(e) => {
                    self.busy = false;
                    let msg = crate::convex_api::clean_error(&e);
                    // Dead session → force login screen (don't leave black home).
                    if msg.to_lowercase().contains("session expired")
                        || msg.to_lowercase().contains("please log in")
                        || msg.to_lowercase().contains("not authenticated")
                    {
                        clear_session();
                        self.session = None;
                        self.conversations.clear();
                        self.friends.clear();
                        self.servers.clear();
                        self.screen = Screen::Auth;
                        self.error = Some("Session expired — log in again".into());
                    } else {
                        self.error = Some(msg);
                    }
                }
                NetEvent::SentOk => {
                    self.busy = false;
                    self.draft.clear();
                }
                NetEvent::Profile(p) => {
                    self.profile_display_name = p.display_name;
                    self.profile_status = p.status_message;
                    self.profile_bio = p.bio;
                    self.profile_avatar_color = if p.avatar_color.is_empty() {
                        crate::convex_api::AVATAR_PALETTE[0].to_string()
                    } else {
                        p.avatar_color
                    };
                    self.profile_loaded = true;
                }
                NetEvent::MyServerPermissions(perms) => {
                    self.my_server_permissions = perms;
                }
            }
        }
    }

    fn open_chat(&mut self, id: String, title: String) {
        self.active_conv_id = Some(id.clone());
        self.active_conv_title = title;
        self.messages.clear();
        self.typing.clear();
        self.draft.clear();
        self.screen = Screen::Chat;
        if let Some(s) = &self.session {
            self.backend.subscribe_chat(s.token.clone(), id);
        }
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Chat => {
                self.active_conv_id = None;
                self.messages.clear();
                self.typing.clear();
                self.draft.clear();
                if let Some(server_id) = self.active_server_id.clone() {
                    // Returning to a server — re-subscribe to channels
                    // (chat watch replaced the home/channels poller).
                    self.screen = Screen::Server;
                    if let Some(s) = &self.session {
                        self.backend
                            .subscribe_channels(s.token.clone(), server_id);
                    }
                } else {
                    self.screen = Screen::Home;
                    if let Some(s) = &self.session {
                        self.backend.subscribe_home(s.token.clone());
                    }
                }
            }
            Screen::Server => {
                self.active_server_id = None;
                self.channels.clear();
                self.screen = Screen::Home;
                if let Some(s) = &self.session {
                    self.backend.subscribe_home(s.token.clone());
                }
            }
            Screen::Home | Screen::Auth => {}
        }
    }

    /// Readable fonts + short chrome; insets push content clear of system bars.
    fn apply_responsive_style(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        let w = screen.width().max(1.0);

        // Mild width scale only — do not crush text on small phones.
        let scale = (w / REF_WIDTH).clamp(MIN_UI_SCALE, MAX_UI_SCALE);
        self.ui_scale = scale;
        // Horizontal pad only (vertical system insets applied separately).
        self.safe_pad = if w > 500.0 {
            12.0
        } else if w < 340.0 {
            8.0
        } else {
            10.0
        };

        let s = scale;
        let mut style = (*ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = PANEL;
        style.visuals.window_fill = BG;
        style.visuals.extreme_bg_color = BG;
        style.visuals.faint_bg_color = ELEVATED;
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.widgets.inactive.bg_fill = ELEVATED;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x22, 0x35, 0x28);
        style.visuals.widgets.active.bg_fill = GREEN;
        style.visuals.selection.bg_fill = Color32::from_rgb(0x1F, 0xA3, 0x44);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.window_corner_radius = CornerRadius::same(2);
        style.visuals.menu_corner_radius = CornerRadius::same(2);

        // Short vertical chrome, readable hit targets horizontally.
        style.spacing.item_spacing = Vec2::new(6.0 * s, 3.0 * s);
        style.spacing.button_padding = Vec2::new(12.0 * s, 5.0 * s); // low height
        style.spacing.indent = 10.0 * s;
        style.spacing.icon_width = 16.0 * s;
        style.spacing.interact_size = Vec2::new(40.0 * s, 26.0 * s); // short rows
        style.spacing.scroll = egui::style::ScrollStyle::solid();

        // Larger type for readability (was 11/13 — too small).
        let mut text_styles = BTreeMap::new();
        text_styles.insert(
            TextStyle::Small,
            FontId::new(12.5 * s, FontFamily::Proportional),
        );
        text_styles.insert(
            TextStyle::Body,
            FontId::new(15.0 * s, FontFamily::Proportional),
        );
        text_styles.insert(
            TextStyle::Button,
            FontId::new(14.5 * s, FontFamily::Proportional),
        );
        text_styles.insert(
            TextStyle::Heading,
            FontId::new(18.0 * s, FontFamily::Proportional),
        );
        // Same family as body — no “coding” monospace look on mobile.
        text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.5 * s, FontFamily::Proportional),
        );
        style.text_styles = text_styles;
        style.wrap_mode = Some(egui::TextWrapMode::Wrap);

        ctx.set_style(style);
    }

    fn fs(&self, base: f32) -> f32 {
        base * self.ui_scale
    }

    /// Short list row — height down, text stays readable.
    fn row_h(&self) -> f32 {
        52.0 * self.ui_scale
    }

    fn card_frame(&self) -> Frame {
        Frame::new()
            .fill(ELEVATED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(10, 8))
    }

    fn panel_frame(&self) -> Frame {
        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(12, 10))
    }

    fn primary_btn(&self, label: &str, wide: f32, h: f32) -> egui::Button<'static> {
        egui::Button::new(
            RichText::new(label.to_owned())
                .strong()
                .color(Color32::from_rgb(0x08, 0x0C, 0x09))
                .size(self.fs(14.0)),
        )
        .fill(GREEN)
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(wide, h))
    }

    fn ghost_btn(&self, label: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(label.to_owned()).color(TEXT).size(self.fs(12.0)))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, BORDER_FOCUS))
            .corner_radius(CornerRadius::same(6))
    }

    fn avatar_chip(&self, ui: &mut egui::Ui, name: &str, online: Option<bool>) {
        let letter = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into());
        let size = self.fs(36.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(8), GREEN_DIM);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0, BORDER_FOCUS),
            egui::StrokeKind::Outside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            letter,
            FontId::new(self.fs(15.0), FontFamily::Proportional),
            GREEN,
        );
        if let Some(on) = online {
            let r = 4.5 * self.ui_scale;
            let c = rect.right_bottom() - Vec2::splat(r + 1.0);
            ui.painter()
                .circle_filled(c, r + 1.2, BG);
            ui.painter()
                .circle_filled(c, r, if on { ONLINE } else { MUTED });
        }
    }
}

impl eframe::App for TalkyssApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_responsive_style(ctx);
        self.apply_events();

        let dt = ctx.input(|i| i.unstable_dt);
        // Heartbeat every ~20s while logged in.
        self.heartbeat_acc += dt;
        if self.heartbeat_acc > 20.0 {
            self.heartbeat_acc = 0.0;
            if let Some(s) = &self.session {
                self.backend.heartbeat(s.token.clone());
            }
        }
        // Fade transient status banners.
        if self.status.is_some() {
            self.status_ttl -= dt;
            if self.status_ttl <= 0.0 {
                self.status = None;
                self.status_ttl = 0.0;
            }
        }

        // System back (Android back key often maps to Escape in winit).
        if ctx.input(|i| {
            i.key_pressed(egui::Key::Escape)
                || (i.key_pressed(egui::Key::Backspace) && !i.modifiers.any())
        }) {
            let typing = ctx.wants_keyboard_input();
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) || !typing {
                if self.screen != Screen::Home && self.screen != Screen::Auth {
                    self.go_back();
                }
            }
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        // Sides: small pad. Top/bottom: clear system status + nav bars.
        let side = self.safe_pad.round().clamp(6.0, 16.0) as i8;
        let pad_top = SYS_TOP_PT.round().clamp(22.0, 40.0) as i8;
        let pad_bot = SYS_BOTTOM_PT.round().clamp(20.0, 40.0) as i8;
        egui::CentralPanel::default()
            .frame(
                Frame::NONE
                    .fill(BG)
                    .inner_margin(Margin {
                        left: side,
                        right: side,
                        top: pad_top,
                        bottom: pad_bot,
                    }),
            )
            .show(ctx, |ui| {
                // Cap content width on tablets — center a phone column.
                let max_w = 480.0 * self.ui_scale;
                let avail = ui.available_width();
                if avail > max_w + 8.0 {
                    let side = (avail - max_w) * 0.5;
                    ui.horizontal(|ui| {
                        ui.add_space(side);
                        ui.allocate_ui_with_layout(
                            Vec2::new(max_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| self.ui_root(ui),
                        );
                    });
                } else {
                    self.ui_root(ui);
                }
            });
    }
}

impl TalkyssApp {
    fn ui_root(&mut self, ui: &mut egui::Ui) {
        match self.screen {
            Screen::Auth => self.ui_auth(ui),
            Screen::Home => self.ui_home(ui),
            Screen::Chat => self.ui_chat(ui),
            Screen::Server => self.ui_server(ui),
            Screen::Profile => self.ui_profile(ui),
        }
    }

    fn banner(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.error.clone() {
            Frame::new()
                .fill(DANGER_BG)
                .stroke(Stroke::new(1.0, DANGER.gamma_multiply(0.55)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(err).color(DANGER).size(self.fs(12.5)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                self.error = None;
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
        if let Some(st) = self.status.clone() {
            Frame::new()
                .fill(GREEN_SOFT)
                .stroke(Stroke::new(1.0, BORDER_FOCUS))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(st).color(GREEN).size(self.fs(12.5)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                self.status = None;
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
    }

    fn field_label(&self, ui: &mut egui::Ui, text: &str) {
        ui.add_space(2.0);
        ui.label(
            RichText::new(text)
                .color(MUTED)
                .size(self.fs(11.5))
                .strong(),
        );
        ui.add_space(2.0);
    }

    fn ui_auth(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(self.fs(18.0));
                    // Logo mark
                    let size = self.fs(52.0);
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(12), GREEN_DIM);
                    ui.painter().rect_stroke(
                        rect,
                        CornerRadius::same(12),
                        Stroke::new(1.5, GREEN.gamma_multiply(0.5)),
                        egui::StrokeKind::Outside,
                    );
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "T",
                        FontId::new(self.fs(26.0), FontFamily::Proportional),
                        GREEN,
                    );
                    ui.add_space(self.fs(10.0));
                    ui.label(
                        RichText::new("Talkyss")
                            .color(TEXT)
                            .size(self.fs(24.0))
                            .strong(),
                    );
                    ui.label(
                        RichText::new("Secure chat · mobile")
                            .color(MUTED)
                            .size(self.fs(12.5)),
                    );
                    ui.add_space(self.fs(14.0));
                });

                let user_font = self.fs(15.0);
                self.panel_frame().show(ui, |ui| {
                    self.banner(ui);
                    ui.label(
                        RichText::new(if self.sign_up_mode {
                            "Create account"
                        } else {
                            "Welcome back"
                        })
                        .color(TEXT)
                        .size(self.fs(16.0))
                        .strong(),
                    );
                    ui.label(
                        RichText::new(if self.sign_up_mode {
                            "Pick a username and password"
                        } else {
                            "Sign in to continue"
                        })
                        .color(MUTED)
                        .size(self.fs(12.0)),
                    );
                    ui.add_space(self.fs(10.0));

                    self.field_label(ui, "USERNAME");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.username)
                            .desired_width(f32::INFINITY)
                            .hint_text("yourname")
                            .font(FontId::new(user_font, FontFamily::Proportional))
                            .margin(Margin::symmetric(10, 8)),
                    );
                    if self.sign_up_mode {
                        self.field_label(ui, "DISPLAY NAME");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.display_name)
                                .desired_width(f32::INFINITY)
                                .hint_text("How others see you")
                                .margin(Margin::symmetric(10, 8)),
                        );
                    }
                    self.field_label(ui, "PASSWORD");
                    for ev in ui.input(|i| i.events.clone()) {
                        if let egui::Event::Paste(text) = ev {
                            self.password = text;
                        }
                    }
                    let mut pwd_edit = egui::TextEdit::singleline(&mut self.password)
                        .desired_width(f32::INFINITY)
                        .hint_text("••••••••")
                        .margin(Margin::symmetric(10, 8));
                    if !self.show_password {
                        pwd_edit = pwd_edit.password(true);
                    }
                    ui.add(pwd_edit);
                    ui.horizontal(|ui| {
                        if ui.add(self.ghost_btn("Paste")).clicked() {
                            if let Some(text) = crate::clipboard_util::get_text() {
                                self.password = text.trim().to_string();
                            } else {
                                self.show_password = true;
                                self.status =
                                    Some("Password visible — long-press field to paste".into());
                            }
                        }
                        let eye = if self.show_password {
                            "Hide"
                        } else {
                            "Show"
                        };
                        if ui.add(self.ghost_btn(eye)).clicked() {
                            self.show_password = !self.show_password;
                        }
                    });
                    ui.add_space(self.fs(12.0));
                    let label = if self.busy {
                        "Please wait…"
                    } else if self.sign_up_mode {
                        "Create account"
                    } else {
                        "Sign in"
                    };
                    let can_submit = !self.busy
                        && !self.username.trim().is_empty()
                        && !self.password.is_empty();
                    if ui
                        .add_enabled(
                            can_submit,
                            self.primary_btn(label, ui.available_width(), self.fs(40.0)),
                        )
                        .clicked()
                    {
                        self.busy = true;
                        self.error = None;
                        if self.sign_up_mode {
                            let dn = if self.display_name.trim().is_empty() {
                                self.username.clone()
                            } else {
                                self.display_name.clone()
                            };
                            self.backend.sign_up(
                                self.username.trim().to_string(),
                                self.password.clone(),
                                dn,
                            );
                        } else {
                            self.backend.sign_in(
                                self.username.trim().to_string(),
                                self.password.clone(),
                            );
                        }
                    }
                    if self
                        .error
                        .as_ref()
                        .is_some_and(|e| e.contains("Try again in"))
                    {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("Wait for the cooldown, then try once.")
                                .color(MUTED)
                                .size(self.fs(12.0)),
                        );
                    }
                    ui.add_space(self.fs(10.0));
                    ui.vertical_centered(|ui| {
                        let switch = if self.sign_up_mode {
                            "Already have an account?  Sign in"
                        } else {
                            "New here?  Create account"
                        };
                        if ui
                            .link(RichText::new(switch).color(GREEN).size(self.fs(13.0)))
                            .clicked()
                        {
                            self.sign_up_mode = !self.sign_up_mode;
                        }
                    });
                });
            });
    }

    fn ui_home(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // mini logo
                    let sz = self.fs(28.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(sz), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(r, CornerRadius::same(7), GREEN_DIM);
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        "T",
                        FontId::new(self.fs(14.0), FontFamily::Proportional),
                        GREEN,
                    );
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Talkyss")
                                .color(TEXT)
                                .strong()
                                .size(self.fs(15.0)),
                        );
                        if let Some(sess) = &self.session {
                            ui.label(
                                RichText::new(format!("{} · @{}", sess.display_name, sess.username))
                                    .color(MUTED)
                                    .size(self.fs(11.5)),
                            );
                        }
                    });
                });
            });

        self.banner(ui);
        ui.add_space(4.0);

        let search_font = self.fs(14.0);
        Frame::new()
            .fill(ELEVATED)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .desired_width(f32::INFINITY)
                        .hint_text("Search chats, friends, servers…")
                        .frame(false)
                        .font(FontId::new(search_font, FontFamily::Proportional)),
                );
            });

        ui.add_space(6.0);
        // Pill tabs
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (t, label) in [
                (Tab::Chats, "Chats"),
                (Tab::Friends, "Friends"),
                (Tab::Servers, "Servers"),
            ] {
                let selected = self.tab == t;
                let fill = if selected { GREEN_DIM } else { Color32::TRANSPARENT };
                let stroke = if selected {
                    Stroke::new(1.0, GREEN.gamma_multiply(0.45))
                } else {
                    Stroke::new(1.0, BORDER)
                };
                let text = RichText::new(label)
                    .color(if selected { GREEN } else { MUTED })
                    .strong()
                    .size(self.fs(13.0));
                if ui
                    .add(
                        egui::Button::new(text)
                            .fill(fill)
                            .stroke(stroke)
                            .corner_radius(CornerRadius::same(16))
                            .min_size(Vec2::new(self.fs(72.0), self.fs(28.0))),
                    )
                    .clicked()
                {
                    self.tab = t;
                }
            }
        });
        ui.add_space(4.0);

        let footer_h = self.fs(34.0);
        let list_h = (ui.available_height() - footer_h).max(self.fs(100.0));
        egui::ScrollArea::vertical()
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| match self.tab {
                Tab::Chats => self.ui_chats_list(ui),
                Tab::Friends => self.ui_friends_list(ui),
                Tab::Servers => self.ui_servers_list(ui),
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if ui.add(self.ghost_btn("Edit profile")).clicked() {
                    self.profile_loaded = false;
                    self.screen = Screen::Profile;
                    if let Some(sess) = self.session.clone() {
                        self.backend
                            .fetch_profile(sess.token.clone(), sess.user_id.clone());
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(self.ghost_btn("Sign out")).clicked() {
                    clear_session();
                    self.session = None;
                    self.screen = Screen::Auth;
                    self.conversations.clear();
                    self.friends.clear();
                    self.servers.clear();
                    self.error = None;
                    self.active_conv_id = None;
                    self.active_server_id = None;
                }
            });
        });
    }

    fn empty_state(&self, ui: &mut egui::Ui, title: &str, hint: &str) {
        ui.add_space(self.fs(24.0));
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(title).color(TEXT).strong().size(self.fs(15.0)));
            ui.label(RichText::new(hint).color(MUTED).size(self.fs(12.5)));
        });
    }

    fn ui_chats_list(&mut self, ui: &mut egui::Ui) {
        let q = self.search.to_lowercase();
        let list: Vec<_> = self
            .conversations
            .iter()
            .filter(|c| q.is_empty() || c.title.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if list.is_empty() {
            self.empty_state(ui, "No chats yet", "Add a friend and start messaging");
            return;
        }
        for c in list {
            let kind = match c.kind.as_str() {
                "direct" => "DM",
                "group" => "Group",
                _ => c.kind.as_str(),
            };
            let clicked = self
                .card_frame()
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.avatar_chip(ui, &c.title, None);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&c.title)
                                        .color(TEXT)
                                        .strong()
                                        .size(self.fs(14.5)),
                                );
                                if c.unread {
                                    ui.label(RichText::new("●").color(GREEN).size(self.fs(12.0)));
                                }
                            });
                            ui.label(RichText::new(kind).color(MUTED).size(self.fs(11.5)));
                        });
                    });
                })
                .response
                .interact(egui::Sense::click())
                .clicked();
            if clicked {
                self.open_chat(c.id, c.title);
            }
            ui.add_space(4.0);
        }
    }

    fn ui_friends_list(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(format!(
                "{} friends · {} online · {} invites",
                self.social_stats.friends_total,
                self.social_stats.friends_online,
                self.social_stats.incoming_pending
            ))
            .color(MUTED)
            .size(self.fs(11.5)),
        );
        ui.add_space(4.0);

        // Add friend
        ui.label(RichText::new("Add friend").color(MUTED).size(self.fs(12.0)));
        let add_w = (ui.available_width() - self.fs(72.0)).max(80.0);
        let font_user = FontId::new(self.fs(13.0), FontFamily::Proportional);
        let font_note = FontId::new(self.fs(12.0), FontFamily::Proportional);
        let btn_w = self.fs(64.0);
        let btn_h = self.fs(30.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.add_friend_user)
                    .desired_width(add_w)
                    .hint_text("username")
                    .font(font_user),
            );
            if ui.add(self.primary_btn("Add", btn_w, btn_h)).clicked() {
                if let Some(s) = &self.session {
                    let u = self.add_friend_user.trim().trim_start_matches('@').to_string();
                    let note = self.add_friend_note.trim().to_string();
                    if !u.is_empty() {
                        self.backend.send_friend_request(
                            s.token.clone(),
                            u,
                            if note.is_empty() { None } else { Some(note) },
                        );
                        self.add_friend_user.clear();
                        self.add_friend_note.clear();
                    }
                }
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.add_friend_note)
                .desired_width(f32::INFINITY)
                .hint_text("Optional note")
                .font(font_note),
        );
        ui.add_space(8.0);

        // Incoming
        if !self.friend_requests.is_empty() {
            ui.label(
                RichText::new(format!("Incoming ({})", self.friend_requests.len()))
                    .color(GREEN)
                    .strong()
                    .size(self.fs(13.0)),
            );
            for req in self.friend_requests.clone() {
                self.card_frame().show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("{} (@{})", req.from_display_name, req.from_username))
                            .color(TEXT)
                            .strong()
                            .size(self.fs(13.5)),
                    );
                    if !req.note.is_empty() {
                        ui.label(RichText::new(&req.note).color(MUTED).size(self.fs(12.0)));
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add(self.primary_btn("Accept", self.fs(72.0), self.fs(28.0)))
                            .clicked()
                        {
                            if let Some(s) = &self.session {
                                self.backend.respond_friend_request(
                                    s.token.clone(),
                                    req.request_id.clone(),
                                    true,
                                );
                            }
                        }
                        if ui.add(self.ghost_btn("Decline")).clicked() {
                            if let Some(s) = &self.session {
                                self.backend.respond_friend_request(
                                    s.token.clone(),
                                    req.request_id.clone(),
                                    false,
                                );
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
            ui.add_space(6.0);
        }

        // Outgoing
        if !self.outgoing_requests.is_empty() {
            ui.label(
                RichText::new(format!("Outgoing ({})", self.outgoing_requests.len()))
                    .color(MUTED)
                    .strong()
                    .size(self.fs(13.0)),
            );
            for req in self.outgoing_requests.clone() {
                self.card_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} (@{})",
                                    req.to_display_name, req.to_username
                                ))
                                .color(TEXT)
                                .size(self.fs(13.0)),
                            );
                            if !req.note.is_empty() {
                                ui.label(RichText::new(&req.note).color(MUTED).size(self.fs(11.5)));
                            }
                        });
                        if ui.add(self.ghost_btn("Cancel")).clicked() {
                            if let Some(s) = &self.session {
                                self.backend
                                    .cancel_friend_request(s.token.clone(), req.request_id.clone());
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
            ui.add_space(6.0);
        }

        let q = self.search.to_lowercase();
        let list: Vec<_> = self
            .friends
            .iter()
            .filter(|f| {
                q.is_empty()
                    || f.display_name.to_lowercase().contains(&q)
                    || f.username.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        if list.is_empty() && self.friend_requests.is_empty() && self.outgoing_requests.is_empty() {
            self.empty_state(
                ui,
                "No friends yet",
                "Add someone by username above",
            );
            return;
        }
        if !self.suggestions.is_empty() {
            ui.label(
                RichText::new("Suggested")
                    .color(MUTED)
                    .strong()
                    .size(self.fs(13.0)),
            );
            for s in self.suggestions.clone().into_iter().take(5) {
                self.card_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(&s.display_name)
                                    .color(TEXT)
                                    .strong()
                                    .size(self.fs(13.0)),
                            );
                            ui.label(
                                RichText::new(if s.mutual_servers.is_empty() {
                                    format!("@{}", s.username)
                                } else {
                                    format!("@{} · {}", s.username, s.mutual_servers.join(", "))
                                })
                                .color(MUTED)
                                .size(self.fs(11.0)),
                            );
                        });
                        if ui
                            .add(self.primary_btn("Add", self.fs(56.0), self.fs(26.0)))
                            .clicked()
                        {
                            self.add_friend_user = s.username;
                        }
                    });
                });
                ui.add_space(3.0);
            }
            ui.add_space(6.0);
        }

        if !list.is_empty() {
            ui.label(
                RichText::new("Friends")
                    .color(MUTED)
                    .strong()
                    .size(self.fs(13.0)),
            );
        }
        for f in list {
            let status = if f.online { "Online" } else { "Offline" };
            let status_col = if f.online { ONLINE } else { MUTED };
            let title = if f.nickname.is_empty() {
                if f.favorite {
                    format!("★ {}", f.display_name)
                } else {
                    f.display_name.clone()
                }
            } else if f.favorite {
                format!("★ {}", f.nickname)
            } else {
                f.nickname.clone()
            };
            let clicked = self
                .card_frame()
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.avatar_chip(ui, &f.display_name, Some(f.online));
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(title)
                                    .color(TEXT)
                                    .strong()
                                    .size(self.fs(14.5)),
                            );
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("@{}", f.username))
                                        .color(MUTED)
                                        .size(self.fs(11.5)),
                                );
                                ui.label(RichText::new("·").color(MUTED));
                                ui.label(
                                    RichText::new(status).color(status_col).size(self.fs(11.5)),
                                );
                            });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new("Message ›").color(GREEN).size(self.fs(12.0)));
                        });
                    });
                })
                .response
                .interact(egui::Sense::click())
                .clicked();
            if clicked {
                if let Some(s) = &self.session {
                    self.backend.open_dm(s.token.clone(), f.user_id.clone());
                    self.active_conv_title = f.display_name.clone();
                    self.status = Some("Opening DM…".into());
                }
            }
            ui.add_space(4.0);
        }
    }

    fn ui_servers_list(&mut self, ui: &mut egui::Ui) {
        // Create server
        let label_sz = self.fs(12.0);
        let field_sz = self.fs(13.5);
        let btn_w = self.fs(80.0);
        let btn_h = self.fs(30.0);
        let create_btn = self.primary_btn("Create", btn_w, btn_h);
        let join_btn = self.primary_btn("Join", btn_w, btn_h);

        ui.label(RichText::new("Create server").color(MUTED).size(label_sz));
        ui.horizontal(|ui| {
            let w = (ui.available_width() - btn_w - 12.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.new_server_name)
                    .desired_width(w)
                    .hint_text("Server name")
                    .font(FontId::new(field_sz, FontFamily::Proportional)),
            );
            if ui.add(create_btn).clicked() {
                let name = self.new_server_name.trim().to_string();
                if name.is_empty() {
                    self.error = Some("Enter a server name".into());
                } else if let Some(s) = &self.session {
                    self.backend.create_server(s.token.clone(), name);
                    self.busy = true;
                }
            }
        });
        ui.add_space(6.0);

        // Join by invite
        ui.label(
            RichText::new("Join with invite code")
                .color(MUTED)
                .size(label_sz),
        );
        ui.horizontal(|ui| {
            let w = (ui.available_width() - btn_w - 12.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.join_invite_code)
                    .desired_width(w)
                    .hint_text("Invite code or link")
                    .font(FontId::new(field_sz, FontFamily::Proportional)),
            );
            if ui.add(join_btn).clicked() {
                let code = extract_invite_code(&self.join_invite_code);
                if code.is_empty() {
                    self.error = Some("Enter an invite code".into());
                } else if let Some(s) = &self.session {
                    self.backend.join_server(s.token.clone(), code);
                    self.busy = true;
                }
            }
        });
        ui.add_space(10.0);

        let q = self.search.to_lowercase();
        let list: Vec<_> = self
            .servers
            .iter()
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if list.is_empty() {
            self.empty_state(ui, "No servers yet", "Create one above or paste an invite code");
            return;
        }
        ui.label(
            RichText::new("Your servers")
                .color(MUTED)
                .strong()
                .size(self.fs(12.0)),
        );
        ui.add_space(4.0);
        for s in list {
            let invite = s.invite_code.clone();
            let is_owner = s.is_owner;
            let clicked = self
                .card_frame()
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.avatar_chip(ui, &s.name, None);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(&s.name)
                                    .color(TEXT)
                                    .strong()
                                    .size(self.fs(14.5)),
                            );
                            let sub = if is_owner {
                                if invite.is_empty() {
                                    "Owner".to_string()
                                } else {
                                    format!("Owner · {invite}")
                                }
                            } else {
                                "Member".to_string()
                            };
                            ui.label(RichText::new(sub).color(MUTED).size(self.fs(11.5)));
                        });
                        if is_owner && !invite.is_empty() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(self.ghost_btn("Copy link")).clicked() {
                                    ui.ctx().copy_text(format!("talkyss://invite/{invite}"));
                                    self.status = Some("Invite link copied".into());
                                    self.status_ttl = 2.5;
                                }
                            });
                        }
                    });
                })
                .response
                .interact(egui::Sense::click())
                .clicked();
            if clicked {
                self.active_server_id = Some(s.server_id.clone());
                self.active_server_name = s.name.clone();
                self.channels.clear();
                self.my_server_permissions = 0;
                self.screen = Screen::Server;
                if let Some(sess) = &self.session {
                    self.backend
                        .subscribe_channels(sess.token.clone(), s.server_id.clone());
                    self.backend
                        .fetch_my_permissions(sess.token.clone(), s.server_id);
                }
            }
            ui.add_space(4.0);
        }
    }

    fn ui_chat(&mut self, ui: &mut egui::Ui) {
        let my_id = self.session.as_ref().map(|s| s.user_id.clone()).unwrap_or_default();

        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add(self.ghost_btn("←")).clicked() {
                        self.go_back();
                    }
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&self.active_conv_title)
                                .strong()
                                .color(TEXT)
                                .size(self.fs(15.0)),
                        );
                        ui.label(
                            RichText::new("Conversation")
                                .color(MUTED)
                                .size(self.fs(11.0)),
                        );
                    });
                });
            });

        self.banner(ui);

        let bottom_h = self.fs(44.0);
        let typing_h = if self.typing.is_empty() {
            0.0
        } else {
            self.fs(16.0)
        };
        let avail = (ui.available_height() - bottom_h - typing_h - 6.0).max(self.fs(80.0));

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .max_height(avail)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.add_space(self.fs(20.0));
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("Say hello 👋")
                                .color(MUTED)
                                .size(self.fs(14.0)),
                        );
                    });
                }
                let msg_pad = 10i8;
                for m in &self.messages {
                    let mine = !my_id.is_empty() && m.author_id == my_id;
                    let fill = if mine { BUBBLE_ME } else { BUBBLE_THEM };
                    let stroke = if mine {
                        Stroke::new(1.0, BORDER_FOCUS)
                    } else {
                        Stroke::new(1.0, BORDER)
                    };
                    ui.horizontal(|ui| {
                        if mine {
                            ui.add_space(ui.available_width() * 0.12);
                        }
                        let max_w = ui.available_width() * if mine { 0.88 } else { 0.88 };
                        ui.allocate_ui_with_layout(
                            Vec2::new(max_w, 0.0),
                            if mine {
                                egui::Layout::top_down(egui::Align::Max)
                            } else {
                                egui::Layout::top_down(egui::Align::Min)
                            },
                            |ui| {
                                Frame::new()
                                    .fill(fill)
                                    .stroke(stroke)
                                    .corner_radius(CornerRadius {
                                        nw: if mine { 12 } else { 4 },
                                        ne: if mine { 4 } else { 12 },
                                        sw: 12,
                                        se: 12,
                                    })
                                    .inner_margin(msg_pad)
                                    .show(ui, |ui| {
                                        if !mine {
                                            ui.label(
                                                RichText::new(&m.author_name)
                                                    .strong()
                                                    .color(GREEN)
                                                    .size(self.fs(12.0)),
                                            );
                                        }
                                        let body = if m.deleted {
                                            "Message deleted".to_string()
                                        } else if m.encrypted {
                                            "Encrypted (open on desktop)".to_string()
                                        } else {
                                            m.body.clone()
                                        };
                                        ui.label(
                                            RichText::new(body)
                                                .color(if m.deleted { MUTED } else { TEXT })
                                                .size(self.fs(14.5)),
                                        );
                                        if !m.attachment_url.is_empty() && !m.deleted {
                                            ui.hyperlink_to(
                                                RichText::new("Open attachment")
                                                    .color(GREEN)
                                                    .size(self.fs(12.0)),
                                                &m.attachment_url,
                                            );
                                        }
                                        ui.label(
                                            RichText::new(fmt_time(m.sent_at))
                                                .color(MUTED)
                                                .size(self.fs(10.5)),
                                        );
                                    });
                            },
                        );
                    });
                    ui.add_space(5.0);
                }
            });

        if !self.typing.is_empty() {
            ui.label(
                RichText::new(format!("{} is typing…", self.typing.join(", ")))
                    .color(MUTED)
                    .italics()
                    .size(self.fs(12.0)),
            );
        }

        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let send_w = self.fs(64.0);
                    let draft_font = self.fs(15.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.draft)
                            .desired_width((ui.available_width() - send_w - 6.0).max(40.0))
                            .hint_text("Message…")
                            .frame(false)
                            .font(FontId::new(draft_font, FontFamily::Proportional)),
                    );
                    if response.changed() {
                        if let (Some(s), Some(id)) = (&self.session, &self.active_conv_id) {
                            self.backend.set_typing(
                                s.token.clone(),
                                id.clone(),
                                !self.draft.is_empty(),
                            );
                        }
                    }
                    let can_send = !self.busy && !self.draft.trim().is_empty();
                    if ui
                        .add_enabled(
                            can_send,
                            self.primary_btn("Send", send_w, self.fs(34.0)),
                        )
                        .clicked()
                        || (response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && can_send)
                    {
                        if let (Some(s), Some(id)) = (&self.session, &self.active_conv_id) {
                            self.busy = true;
                            self.backend.send_message(
                                s.token.clone(),
                                id.clone(),
                                self.draft.trim().to_string(),
                            );
                            self.backend
                                .set_typing(s.token.clone(), id.clone(), false);
                        }
                    }
                });
            });
    }

    fn ui_server(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add(self.ghost_btn("←")).clicked() {
                        self.go_back();
                    }
                    ui.add_space(4.0);
                    self.avatar_chip(ui, &self.active_server_name, None);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(&self.active_server_name)
                            .strong()
                            .color(TEXT)
                            .size(self.fs(15.0)),
                    );
                });
            });

        self.banner(ui);
        ui.add_space(4.0);
        ui.label(
            RichText::new("Channels")
                .color(MUTED)
                .strong()
                .size(self.fs(12.0)),
        );
        ui.add_space(4.0);

        let h = ui.available_height().max(self.fs(80.0));
        egui::ScrollArea::vertical()
            .max_height(h)
            .show(ui, |ui| {
                let text_ch: Vec<_> = self
                    .channels
                    .iter()
                    .filter(|c| c.channel_type != "voice")
                    .cloned()
                    .collect();
                let voice_ch: Vec<_> = self
                    .channels
                    .iter()
                    .filter(|c| c.channel_type == "voice")
                    .cloned()
                    .collect();

                if !text_ch.is_empty() {
                    ui.label(
                        RichText::new("TEXT")
                            .color(MUTED)
                            .size(self.fs(10.5))
                            .strong(),
                    );
                    ui.add_space(2.0);
                }
                for c in text_ch {
                    let clicked = self
                        .card_frame()
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("#  {}", c.name))
                                    .color(TEXT)
                                    .size(self.fs(14.5)),
                            );
                        })
                        .response
                        .interact(egui::Sense::click())
                        .clicked();
                    if clicked {
                        self.open_chat(c.conversation_id, format!("#{}", c.name));
                    }
                    ui.add_space(4.0);
                }

                if !voice_ch.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("VOICE · desktop audio")
                            .color(MUTED)
                            .size(self.fs(10.5))
                            .strong(),
                    );
                    ui.add_space(2.0);
                }
                for c in voice_ch {
                    self.card_frame().show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("v  {}", c.name))
                                .color(MUTED)
                                .size(self.fs(14.0)),
                        );
                    });
                    ui.add_space(4.0);
                }
            });
    }
}

fn fmt_time(ms: f64) -> String {
    if ms <= 0.0 {
        return String::new();
    }
    let secs = (ms / 1000.0) as i64;
    let mins = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    // crude UTC clock — good enough for mobile list
    format!("{hours:02}:{mins:02}")
}

fn session_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("talkyss-mobile").join("session.json")
}

fn save_session(s: &AuthSession) {
    let path = session_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
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
        role: v.get("role").and_then(|x| x.as_str()).unwrap_or("user").to_string(),
    })
}

fn clear_session() {
    let _ = std::fs::remove_file(session_path());
}

/// Install Roboto + emoji fallback (normal Android look, not egui default).
fn install_app_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let regular = include_bytes!("../assets/Roboto-Regular.ttf");
    fonts.font_data.insert(
        "Roboto".to_owned(),
        std::sync::Arc::new(FontData::from_static(regular)),
    );

    let medium = include_bytes!("../assets/Roboto-Medium.ttf");
    fonts.font_data.insert(
        "RobotoMedium".to_owned(),
        std::sync::Arc::new(FontData::from_static(medium)),
    );

    // Bundled monochrome emoji so reactions / symbols paint without system fonts.
    let emoji = include_bytes!("../assets/NotoEmoji.ttf");
    fonts.font_data.insert(
        "NotoEmoji".to_owned(),
        std::sync::Arc::new(FontData::from_static(emoji)),
    );

    // Prefer Roboto for all proportional text (body, buttons, headings).
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        prop.insert(0, "Roboto".to_owned());
        prop.insert(1, "RobotoMedium".to_owned());
        // Emoji after primary faces — used when Roboto has no glyph.
        prop.push("NotoEmoji".to_owned());
    }
    // Map "monospace" style to Roboto too so leftover mono styles look normal.
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.insert(0, "Roboto".to_owned());
        mono.push("NotoEmoji".to_owned());
    }

    ctx.set_fonts(fonts);
}
