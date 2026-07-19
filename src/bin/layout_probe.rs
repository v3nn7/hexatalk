//! Standalone layout/crash probe for the Slint UI. NOT part of the app.
//!
//! Instantiates the same generated `AppWindow` as the real binary, drives it
//! into a given screen state (arg: auth | welcome | chat0 | chat1 | chatN |
//! chatfull | nasty) with fake data -- no Convex, no login -- prints the
//! real window size, and captures a screenshot of the window via xcap, then
//! exits. Used to reproduce the "stack overflow on opening a chat" crash and
//! the "content doesn't fill the window" layout bug locally.

mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

fn fake_msg(i: usize, mine: bool, grouped: bool, body: &str) -> ui::ChatMessageRow {
    ui::ChatMessageRow {
        id: format!("m{i}").into(),
        author_id: if mine { "me" } else { "peer" }.into(),
        author_name: if mine { "Me" } else { "Peer" }.into(),
        author_initial: if mine { "M" } else { "P" }.into(),
        author_avatar_color: slint::Color::from_rgb_u8(61, 148, 107),
        meta: "12:34".into(),
        body: body.into(),
        mine,
        grouped,
        can_react: true,
        can_edit: mine,
        can_delete: mine,
        ..Default::default()
    }
}

/// Mirror of what `apply()` sets for a logged-in user sitting in an empty
/// direct-message conversation (peerseal channel still connecting).
fn apply_full(ui: &ui::AppWindow, nasty: bool) {
    ui.set_current_screen(ui::Screen::Chat);

    // ---- Rail ----
    ui.set_chat_home_active(true);
    ui.set_chat_unread_count(3);
    ui.set_chat_servers(
        vec![
            ui::ServerRow {
                server_id: "s1".into(),
                name: "Server One".into(),
                initial: "S".into(),
                ..Default::default()
            },
            ui::ServerRow {
                server_id: "s2".into(),
                name: "Server Two".into(),
                initial: "T".into(),
                ..Default::default()
            },
        ]
        .as_slice()
        .into(),
    );
    ui.set_chat_friends_online(2);
    ui.set_chat_incoming_count(1);
    ui.set_chat_show_admin_tab(true);

    // ---- Sidebar ----
    ui.set_chat_tab(ui::SidebarTab::Chats);
    ui.set_chat_tab_title("Direct".into());
    ui.set_chat_selected_server(false);
    ui.set_chat_sidebar_width(320.0);
    let long = "PORNHUB dupy czesty 3Rr0rExE404 test with a somewhat longer conversation title";
    ui.set_chat_conversations(
        (0..8)
            .map(|i| ui::ConversationRow {
                id: format!("c{i}").into(),
                title: if nasty && i == 0 {
                    long.into()
                } else {
                    format!("friend{i}").into()
                },
                unread: i == 2,
                active: i == 0,
            })
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    ui.set_chat_friends_summary("8 friends · 2 online · 1 in · 0 out".into());
    ui.set_chat_my_display_name("v3nn7".into());
    ui.set_chat_my_initial("V".into());
    ui.set_chat_my_badge_text("OWNER".into());
    ui.set_chat_my_badge_bg(slint::Color::from_rgb_u8(209, 188, 77));
    ui.set_chat_my_badge_fg(slint::Color::from_rgb_u8(5, 13, 5));
    ui.set_chat_is_admin(true);

    // ---- Chat area: empty direct conversation, secure channel pending ----
    ui.set_chat_has_conversation(true);
    ui.set_chat_peer_title(
        if nasty {
            "🔥💀 Ẕ̸̢a̴l̷g̶o̸ peer 🔥💀"
        } else {
            "djfranek23"
        }
        .into(),
    );
    ui.set_chat_peer_initial("D".into());
    ui.set_chat_peer_online(true);
    ui.set_chat_is_channel_icon(false);
    ui.set_chat_is_direct(true);
    ui.set_chat_peer_connected(false);
    ui.set_chat_connection_label("Connecting secure channel…".into());
    ui.set_chat_sas_label("".into());
    ui.set_chat_show_call_button(true);
    ui.set_chat_is_server_channel(false);
    ui.set_chat_store_enabled(true);
    ui.set_chat_store_allowed(true);
    ui.set_chat_can_voice(false);
    ui.set_chat_quick_emojis(
        ["👍", "❤️", "😂", "😮", "😢", "🎉"]
            .iter()
            .map(|e| slint::SharedString::from(*e))
            .collect::<Vec<_>>()
            .as_slice()
            .into(),
    );
    let msgs = if nasty {
        let megaword = "A".repeat(4000);
        vec![
            fake_msg(0, false, false, &megaword),
            fake_msg(
                1,
                true,
                false,
                "🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉",
            ),
            ui::ChatMessageRow {
                id: "d0".into(),
                is_call_log: true,
                body: "Friday, July 18".into(),
                ..Default::default()
            },
            fake_msg(
                2,
                false,
                true,
                "https://example.com/some/extremely/long/unbroken/url/path/that/never/wraps/because/it/has/no/spaces/anywhere/in/it/at/all/1234567890",
            ),
        ]
    } else {
        vec![
            ui::ChatMessageRow {
                id: "d0".into(),
                is_call_log: true,
                body: "Friday, July 18".into(),
                ..Default::default()
            },
            fake_msg(0, false, false, "hello"),
            fake_msg(1, true, false, "hi there"),
        ]
    };
    ui.set_chat_messages(msgs.as_slice().into());
    ui.set_chat_is_editing(false);
    ui.set_chat_input_placeholder("Waiting for secure channel…".into());
    ui.set_chat_send_label("Send".into());
    ui.set_chat_crypto_ready(false);
    ui.set_chat_message_input("".into());
    ui.set_chat_typing_line("".into());
    ui.set_chat_error_line("".into());
    ui.set_chat_warning_line("Connecting secure channel…".into());

    // ---- Members drawer (hidden: no server selected) ----
    ui.set_chat_members_collapsed(true);
    ui.set_chat_members_width(220.0);
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "welcome".into());
    eprintln!("probe mode: {mode}");

    let ui = ui::AppWindow::new().expect("create AppWindow");

    match mode.as_str() {
        "auth" => {
            ui.set_current_screen(ui::Screen::Auth);
        }
        "welcome" => {
            ui.set_current_screen(ui::Screen::Chat);
            ui.set_chat_has_conversation(false);
        }
        "chat0" | "chat1" | "chatN" => {
            ui.set_current_screen(ui::Screen::Chat);
            ui.set_chat_has_conversation(true);
            ui.set_chat_peer_title("Probe Peer".into());
            ui.set_chat_peer_initial("P".into());
            ui.set_chat_is_direct(true);
            ui.set_chat_input_placeholder("Type a message...".into());
            ui.set_chat_quick_emojis(
                ["👍", "❤️"]
                    .iter()
                    .map(|e| slint::SharedString::from(*e))
                    .collect::<Vec<_>>()
                    .as_slice()
                    .into(),
            );
            let msgs: Vec<ui::ChatMessageRow> = match mode.as_str() {
                "chat0" => vec![],
                "chat1" => vec![fake_msg(0, false, false, "hello")],
                _ => (0..30)
                    .map(|i| {
                        let mine = i % 3 == 0;
                        let grouped = i % 4 == 1;
                        let body = if i % 5 == 0 {
                            "a much longer message body that should word-wrap across the available width of the chat area to exercise the text wrapping logic"
                        } else {
                            "short msg"
                        };
                        fake_msg(i, mine, grouped, body)
                    })
                    .collect(),
            };
            ui.set_chat_messages(msgs.as_slice().into());
        }
        "chatfull" | "nasty" => {
            apply_full(&ui, mode == "nasty");
        }
        "friends" => {
            ui.set_current_screen(ui::Screen::Chat);
            ui.set_chat_home_active(true);
            ui.set_chat_tab(ui::SidebarTab::Friends);
            ui.set_chat_tab_title("Friends".into());
            ui.set_chat_sidebar_width(320.0);
            ui.set_chat_friends_active(true);
            ui.set_chat_friends_summary("4 friends · 2 online · 1 in · 0 out".into());
            ui.set_chat_friends(
                vec![
                    ui::FriendRow {
                        user_id: "u1".into(),
                        label: "djfranek23".into(),
                        subtitle: "Online".into(),
                        initial: "D".into(),
                        online: true,
                        ..Default::default()
                    },
                    ui::FriendRow {
                        user_id: "u2".into(),
                        label: "Xeni".into(),
                        subtitle: "Offline".into(),
                        initial: "X".into(),
                        online: false,
                        ..Default::default()
                    },
                ]
                .as_slice()
                .into(),
            );
            ui.set_chat_has_conversation(false);
        }
        "serversettings" => {
            ui.set_current_screen(ui::Screen::ServerSettings);
            ui.set_ss_category(ui::ServerSettingsCategory::Channels);
            ui.set_ss_server_name("v3nn7dev".into());
            ui.set_ss_server_initial("V".into());
            ui.set_ss_header_meta("2 channels · 4 members".into());
            ui.set_ss_is_owner(true);
            ui.set_ss_can_manage_channels(true);
            ui.set_ss_can_delete_channel(true);
            ui.set_ss_channels(
                vec![
                    ui::SSChannelRow {
                        conversation_id: "c1".into(),
                        name: "general".into(),
                        is_voice: false,
                        is_renaming: false,
                        is_editing_perms: false,
                        is_system: false,
                        is_announcement: false,
                        can_move_up: false,
                        can_move_down: true,
                    },
                    ui::SSChannelRow {
                        conversation_id: "c2".into(),
                        name: "general".into(),
                        is_voice: true,
                        is_renaming: false,
                        is_editing_perms: false,
                        is_system: false,
                        is_announcement: false,
                        can_move_up: false,
                        can_move_down: false,
                    },
                ]
                .as_slice()
                .into(),
            );
        }
        // Full snapshot but sitting on the welcome screen (no open chat):
        // isolates whether the `if !has_conversation` branch itself breaks
        // the layout, or whether it's one of the props bare `welcome` skips.
        "welcome2" => {
            apply_full(&ui, false);
            ui.set_chat_has_conversation(false);
            ui.set_chat_messages(Vec::<ui::ChatMessageRow>::new().as_slice().into());
        }
        // Bare welcome + ONLY a non-empty conversations list: isolates the
        // "No chats yet" word-wrap Text in the sidebar.
        "welcome3" => {
            ui.set_current_screen(ui::Screen::Chat);
            ui.set_chat_has_conversation(false);
            ui.set_chat_conversations(
                (0..3)
                    .map(|i| ui::ConversationRow {
                        id: format!("c{i}").into(),
                        title: format!("friend{i}").into(),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>()
                    .as_slice()
                    .into(),
            );
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }

    // Re-apply the whole snapshot periodically like the real app's sync_ui
    // does (presence updates etc. re-run apply() constantly).
    let resync = slint::Timer::default();
    if mode == "chatfull" || mode == "nasty" {
        let uiw = ui.as_weak();
        let nasty = mode == "nasty";
        resync.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(250),
            move || {
                if let Some(ui) = uiw.upgrade() {
                    apply_full(&ui, nasty);
                }
            },
        );
    }

    let uiw = ui.as_weak();
    let shot_name = format!("probe_{mode}.jpg");
    slint::Timer::single_shot(std::time::Duration::from_millis(2000), move || {
        let ui = uiw.unwrap();
        eprintln!("window().size() = {:?}", ui.window().size());
        eprintln!("scale_factor    = {}", ui.window().scale_factor());
        // Screenshot our own window (matched by pid, falling back to the
        // primary monitor) so layout can be inspected.
        let my_pid = std::process::id();
        let save = |img: &xcap::image::RgbaImage, name: &str| {
            let path = std::env::temp_dir().join(name);
            // JPEG has no alpha channel -- convert RGBA -> RGB first.
            let rgb = xcap::image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            let mut out = Vec::new();
            let enc = xcap::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
            match rgb.write_with_encoder(enc) {
                Ok(()) => {
                    let _ = std::fs::write(&path, &out);
                    eprintln!("screenshot -> {}", path.display());
                }
                Err(e) => eprintln!("jpeg encode failed: {e}"),
            }
        };
        let mut captured = false;
        if let Ok(windows) = xcap::Window::all() {
            for w in windows {
                let pid = w.pid().unwrap_or(0);
                let title = w.title().unwrap_or_default();
                if pid == my_pid && !title.is_empty() {
                    eprintln!("own window: pid={pid} title={title:?}");
                    match w.capture_image() {
                        Ok(img) => {
                            save(&img, &shot_name);
                            captured = true;
                        }
                        Err(e) => eprintln!("window capture failed: {e}"),
                    }
                }
            }
        }
        if !captured {
            eprintln!("no own window matched; capturing primary monitor");
            if let Ok(monitors) = xcap::Monitor::all() {
                if let Some(m) = monitors.first() {
                    match m.capture_image() {
                        Ok(img) => save(&img, &shot_name),
                        Err(e) => eprintln!("monitor capture failed: {e}"),
                    }
                }
            }
        }
        let _ = slint::quit_event_loop();
    });

    ui.run().expect("run event loop");
    eprintln!("clean exit");
}
