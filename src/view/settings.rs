//! The app-wide Settings screen (My Account / Privacy / Bots / Voice / About).

use std::sync::atomic::Ordering;

use iced::widget::{
    button, column, container, horizontal_space, image, row, slider, text, text_input, Space,
};
use iced::{Background, Border, Color, ContentFit, Element, Length, Shadow, Theme};

use crate::style::*;
use crate::*;

impl App {
    pub(crate) fn view_settings<'a>(&'a self, session: &'a Session) -> Element<'a, Message> {
        let header = row![
            button(text("← Back").size(13))
                .on_press(Message::CloseSettings)
                .padding([8, 14])
                .style(secondary_button_style),
            horizontal_space(),
            text("Settings").size(20).style(|_theme: &Theme| text::Style {
                color: Some(c_text_primary())
            }),
            horizontal_space(),
            Space::with_width(Length::Fixed(72.0)),
        ]
        .align_y(iced::Alignment::Center)
        .padding(16);

        let photo_preview: Element<'_, Message> =
            if let Some(handle) = self.avatar_image_cache.get(&session.avatar_image_url) {
                container(
                    image(handle.clone())
                        .width(Length::Fixed(72.0))
                        .height(Length::Fixed(72.0))
                        .content_fit(ContentFit::Cover),
                )
                .width(Length::Fixed(72.0))
                .height(Length::Fixed(72.0))
                .into()
            } else {
                let bg_color = parse_hex_color(&self.settings_avatar_color).unwrap_or_else(c_accent);
                container(text(
                    session
                        .display_name
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "?".to_string()),
                ))
                .width(Length::Fixed(72.0))
                .height(Length::Fixed(72.0))
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .style(move |_theme: &Theme| container::Style {
                    background: Some(Background::Color(bg_color)),
                    text_color: Some(Color::from_rgb(0.02, 0.05, 0.02)),
                    border: Border {
                        radius: r0(),
                        width: 1.0,
                        color: c_border_strong(),
                    },
                    ..Default::default()
                })
                .into()
            };

        let has_photo = !session.avatar_image_url.is_empty();
        let mut photo_actions = column![
            button(text(if self.avatar_upload_busy {
                "Uploading..."
            } else {
                "Upload photo"
            }))
            .on_press_maybe((!self.avatar_upload_busy).then_some(Message::PickAvatarImage))
            .padding([8, 12])
            .style(secondary_button_style),
        ]
        .spacing(6);
        if has_photo {
            photo_actions = photo_actions.push(
                button(text("Remove photo").size(12))
                    .on_press(Message::RemoveAvatarImage)
                    .padding([6, 10])
                    .style(link_button_style),
            );
        }
        photo_actions = photo_actions.push(
            text("PNG or JPG, under 2MB.").size(11).style(|_theme: &Theme| text::Style {
                color: Some(c_text_muted()),
            }),
        );

        let mut profile_section = column![
            section_title("Profile"),
            row![photo_preview, photo_actions]
                .spacing(16)
                .align_y(iced::Alignment::Center),
            field_label("Display name"),
            text_input("Display name", &self.settings_display_name_input)
                .on_input(Message::SettingsDisplayNameChanged)
                .padding(12)
                .style(pill_input_style),
            field_label("Status message"),
            text_input(
                "What's on your mind?",
                &self.settings_status_input
            )
            .on_input(Message::SettingsStatusChanged)
            .padding(12)
            .style(pill_input_style),
            field_label("Bio"),
            text_input("Tell people a bit about yourself", &self.settings_bio_input)
                .on_input(Message::SettingsBioChanged)
                .padding(12)
                .style(pill_input_style),
            field_label("Avatar color"),
        ]
        .spacing(14)
        .width(Length::Fill);

        let mut color_row = row![].spacing(10);
        for color in AVATAR_PALETTE {
            let selected = self.settings_avatar_color == color;
            let swatch = button(Space::new(Length::Fixed(28.0), Length::Fixed(28.0)))
                .on_press(Message::SettingsAvatarColorSelected(color.to_string()))
                .style(move |_theme: &Theme, _status| button::Style {
                    background: parse_hex_color(color).map(Background::Color),
                    border: Border {
                        radius: r0(),
                        width: if selected { 2.0 } else { 1.0 },
                        color: if selected {
                            c_accent()
                        } else {
                            c_border()
                        },
                    },
                    shadow: if selected {
                        light_shadow()
                    } else {
                        Shadow::default()
                    },
                    ..Default::default()
                });
            color_row = color_row.push(swatch);
        }
        profile_section = profile_section.push(color_row);
        profile_section = profile_section.push(
            button(container(text("Save profile").size(14)).center_x(Length::Fill))
                .on_press(Message::SaveProfile)
                .padding(12)
                .width(Length::Fixed(200.0))
                .style(accent_button_style),
        );
        if let Some(status) = &self.settings_profile_status {
            profile_section = profile_section.push(muted_text(status.clone(), 12));
        }

        let mut password_section = column![
            section_title("Change password"),
            text_input("Current password", &self.settings_current_password_input)
                .on_input(Message::SettingsCurrentPasswordChanged)
                .secure(true)
                .padding(12)
                .style(pill_input_style),
            text_input("New password", &self.settings_new_password_input)
                .on_input(Message::SettingsNewPasswordChanged)
                .secure(true)
                .padding(12)
                .style(pill_input_style),
            text_input("Confirm new password", &self.settings_confirm_password_input)
                .on_input(Message::SettingsConfirmPasswordChanged)
                .secure(true)
                .padding(12)
                .style(pill_input_style),
        ]
        .spacing(14)
        .width(Length::Fill);
        password_section = password_section.push(
            button(container(text("Change password").size(14)).center_x(Length::Fill))
                .on_press(Message::ChangePassword)
                .padding(12)
                .width(Length::Fixed(200.0))
                .style(accent_button_style),
        );
        if let Some(status) = &self.settings_password_status {
            password_section = password_section.push(muted_text(status.clone(), 12));
        }

        let logout_section = column![
            section_title("Session"),
            muted_text("Sign out of this device. You can sign back in anytime.", 12),
            button(container(text("Log out").size(14)).center_x(Length::Fill))
                .on_press(Message::LogOut)
                .padding(12)
                .width(Length::Fixed(200.0))
                .style(danger_button_style),
        ]
        .spacing(12)
        .width(Length::Fill);

        let mut voice_section = column![
            section_title("Voice & audio"),
            field_label("Microphone"),
        ]
        .spacing(14)
        .width(Length::Fill);

        let mut mic_list = column![].spacing(4);
        let default_label = "System default".to_string();
        let mic_active = self.settings_input_device.is_none();
        mic_list = mic_list.push(
            button(text(default_label.clone()).size(13))
                .on_press(Message::SettingsInputDeviceSelected(String::new()))
                .width(Length::Fill)
                .padding(8)
                .style(move |theme: &Theme, status| {
                    sidebar_item_style(theme, status, mic_active)
                }),
        );
        for device in &self.settings_input_devices {
            let is_selected = self.settings_input_device.as_deref() == Some(device.as_str());
            mic_list = mic_list.push(
                button(text(device.clone()).size(13))
                    .on_press(Message::SettingsInputDeviceSelected(device.clone()))
                    .width(Length::Fill)
                    .padding(8)
                    .style(move |theme: &Theme, status| {
                        sidebar_item_style(theme, status, is_selected)
                    }),
            );
        }
        voice_section = voice_section.push(mic_list);
        voice_section = voice_section.push(field_label("Speaker"));

        let mut speaker_list = column![].spacing(4);
        let speaker_active = self.settings_output_device.is_none();
        speaker_list = speaker_list.push(
            button(text(default_label).size(13))
                .on_press(Message::SettingsOutputDeviceSelected(String::new()))
                .width(Length::Fill)
                .padding(8)
                .style(move |theme: &Theme, status| {
                    sidebar_item_style(theme, status, speaker_active)
                }),
        );
        for device in &self.settings_output_devices {
            let is_selected = self.settings_output_device.as_deref() == Some(device.as_str());
            speaker_list = speaker_list.push(
                button(text(device.clone()).size(13))
                    .on_press(Message::SettingsOutputDeviceSelected(device.clone()))
                    .width(Length::Fill)
                    .padding(8)
                    .style(move |theme: &Theme, status| {
                        sidebar_item_style(theme, status, is_selected)
                    }),
            );
        }
        voice_section = voice_section.push(speaker_list);
        voice_section = voice_section.push(
            text("Device choice applies the next time you start or accept a call.")
                .size(11)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
        );

        let current_gate = f32::from_bits(self.noise_gate.load(Ordering::Relaxed));
        let gate_label = if current_gate <= 0.0005 {
            "Off".to_string()
        } else {
            format!("{:.3}", current_gate)
        };
        voice_section = voice_section.push(field_label("Noise gate sensitivity"));
        voice_section = voice_section.push(
            slider(0.0..=0.03, current_gate, Message::NoiseGateChanged).step(0.0005),
        );
        voice_section = voice_section.push(
            row![
                text("Off").size(10).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted())
                }),
                horizontal_space(),
                text(gate_label).size(11),
                horizontal_space(),
                text("Aggressive").size(10).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted())
                }),
            ]
            .align_y(iced::Alignment::Center),
        );
        voice_section = voice_section.push(
            text("Cuts background hiss between words. Applies live, even mid-call.")
                .size(11)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
        );

        let _ = session;

        let vault_hint = history::vault_root_display(&session.user_id);
        let mut about_section = column![
            section_title("About"),
            text(format!("Talkyss v{CURRENT_APP_VERSION}")).size(14).font(mono()).style(
                |_theme: &Theme| text::Style {
                    color: Some(c_accent()),
                }
            ),
            text("DM history lives on Convex (shared). Live transport is peerseal.")
                .size(12)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
            text(format!("Local data dir: {vault_hint}"))
                .size(10)
                .font(mono())
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
            button(text("Check for updates").size(12))
                .on_press(Message::CheckForUpdate)
                .padding([8, 12])
                .style(secondary_button_style),
        ]
        .spacing(10);
        if let Some(status) = &self.update_check_status {
            about_section = about_section.push(muted_text(status.clone(), 12));
        }
        about_section = about_section.push(
            button(text("Ping server").size(12))
                .on_press(Message::MeasurePing)
                .padding([8, 12])
                .style(secondary_button_style),
        );
        if let Some(status) = &self.ping_status {
            about_section = about_section.push(
                text(format!("ping: {status}"))
                    .size(12)
                    .font(mono())
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_accent()),
                    }),
            );
        }

        let mut privacy_section = column![
            section_title("Privacy & Security"),
            muted_text(
                "Control how others see you, what is stored, and who can reach you.",
                12,
            ),
            // Staff badges preview
            row![
                text("Your badges:").size(12).style(|_theme: &Theme| {
                    text::Style {
                        color: Some(c_text_muted()),
                    }
                }),
                if session.platform_role == "owner" {
                    badge_chip(
                        "OWNER",
                        Color::from_rgb(0.88, 0.70, 0.28),
                        Color::from_rgb(0.05, 0.04, 0.0),
                    )
                } else if session.is_admin {
                    badge_chip("STAFF", Color::from_rgb(0.82, 0.40, 0.28), Color::WHITE)
                } else if session.is_moderator {
                    badge_chip("MOD", Color::from_rgb(0.25, 0.55, 0.95), Color::WHITE)
                } else {
                    muted_text("none", 11)
                },
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            privacy_toggle_row(
                "Chat history storage",
                "Off = live-only (peerseal); nothing saved on Convex for chats you're in.",
                session.store_chat_history,
                Message::ToggleStoreHistoryGlobal,
            ),
            privacy_toggle_row(
                "Hide online status",
                "Others see you as offline (you still receive messages).",
                session.hide_online_status,
                Message::ToggleHideOnline,
            ),
            privacy_toggle_row(
                "Friends-only DMs",
                "Only people on your friends list can start a direct chat.",
                session.friends_only_dms,
                Message::ToggleFriendsOnlyDms,
            ),
            privacy_toggle_row(
                "Discoverable profile",
                "When off, you are harder to find (exact username still works).",
                session.discoverable,
                Message::ToggleDiscoverable,
            ),
            field_label("Friend requests"),
            button(
                text(format!(
                    "Who can request: {}",
                    friend_request_privacy_label(&session.friend_request_privacy)
                ))
                .size(13),
            )
            .on_press(Message::CycleFriendRequestPrivacy)
            .padding([10, 14])
            .style(secondary_button_style),
            muted_text(
                "Everyone · Shared servers only · Nobody. Click to cycle. Decline has a 24h re-request cooldown.",
                11,
            ),
            field_label("My presence"),
            button(
                text(format!(
                    "Status: {}",
                    presence_label(&session.presence_status)
                ))
                .size(13),
            )
            .on_press(Message::CyclePresenceStatus)
            .padding([10, 14])
            .style(secondary_button_style),
            muted_text(
                "Online · Idle · Do not disturb · Invisible. Invisible hides you like offline.",
                11,
            ),
            field_label("Sessions"),
            button(text("Sign out other devices").size(13))
                .on_press(Message::SignOutOtherSessions)
                .padding([10, 14])
                .style(secondary_button_style),
            muted_text(
                "Keeps this session; revokes every other login token on this account.",
                11,
            ),
            muted_text(
                "Per-chat Store ON/OFF is in the DM header (not on server channels).",
                11,
            ),
        ]
        .spacing(14)
        .width(Length::Fill);

        if session.is_admin {
            privacy_section = privacy_section.push(
                container(
                    column![
                        text("STAFF PRIVILEGES").size(11).style(|_theme: &Theme| {
                            text::Style {
                                color: Some(c_warning()),
                            }
                        }),
                        muted_text(
                            "Admins cannot be blocked or kicked from servers. Friend requests still require acceptance. Support DMs work without friendship. Moderators can ban users but not staff.",
                            11,
                        ),
                    ]
                    .spacing(4),
                )
                .padding(10)
                .style(panel_box_style),
            );
        }

        let mut bots_section = column![
            section_title("Talkyss bots"),
            muted_text(
                "Headless bots: create here, invite to a server, run with crates/talkyss-bot.",
                12,
            ),
            row![
                text_input("Bot display name", &self.new_bot_name_input)
                    .on_input(Message::NewBotNameChanged)
                    .on_submit(Message::CreateBot)
                    .padding(10)
                    .width(Length::Fill)
                    .style(pill_input_style),
                button(text("Create").size(12))
                    .on_press(Message::CreateBot)
                    .padding([10, 14])
                    .style(accent_button_style),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            button(text("Refresh list").size(11))
                .on_press(Message::RefreshMyBots)
                .style(link_button_style),
        ]
        .spacing(14)
        .width(Length::Fill);

        if let Some(token) = &self.bot_token_reveal {
            bots_section = bots_section.push(
                container(
                    column![
                        text("COPY TOKEN NOW (shown once)").size(11).style(
                            |_theme: &Theme| text::Style {
                                color: Some(c_warning()),
                            }
                        ),
                        text(token.clone()).size(12).style(|_theme: &Theme| {
                            text::Style {
                                color: Some(c_accent()),
                            }
                        }),
                        row![
                            button(text("Copy").size(11))
                                .on_press(Message::CopyMessage(token.clone()))
                                .padding([6, 10])
                                .style(accent_button_style),
                            button(text("Dismiss").size(11))
                                .on_press(Message::DismissBotToken)
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(8),
                    ]
                    .spacing(8),
                )
                .padding(12)
                .style(panel_box_style),
            );
        }

        for bot in &self.my_bots {
            bots_section = bots_section.push(
                container(
                    column![
                        row![
                            text(format!("{}  *bot", bot.display_name)).size(13).style(
                                |_theme: &Theme| text::Style {
                                    color: Some(c_text_primary()),
                                }
                            ),
                            horizontal_space(),
                            text(format!("@{}", bot.username)).size(11).style(
                                |_theme: &Theme| text::Style {
                                    color: Some(c_text_muted()),
                                }
                            ),
                        ]
                        .align_y(iced::Alignment::Center),
                        row![
                            button(text("New token").size(11))
                                .on_press(Message::RegenerateBotToken(bot.bot_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                            button(text("Delete").size(11))
                                .on_press(Message::DeleteBot(bot.bot_id.clone()))
                                .padding([6, 10])
                                .style(danger_button_style),
                        ]
                        .spacing(6),
                    ]
                    .spacing(6),
                )
                .padding(10)
                .style(panel_box_style),
            );
        }

        bots_section = bots_section.push(
            column![
                field_label("Invite bot to current server"),
                row![
                    text_input("bot_username", &self.bot_invite_username_input)
                        .on_input(Message::BotInviteUsernameChanged)
                        .on_submit(Message::InviteBotToServer)
                        .padding(10)
                        .width(Length::Fill)
                        .style(pill_input_style),
                    button(text("Invite").size(12))
                        .on_press(Message::InviteBotToServer)
                        .padding([10, 14])
                        .style(accent_button_style),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
                muted_text("Select a server on the rail first. Owner only.", 10),
            ]
            .spacing(6),
        );

        if let Some(status) = &self.bot_status {
            bots_section = bots_section.push(muted_text(status.clone(), 12));
        }

        let categories: [(SettingsCategory, &str); 5] = [
            (SettingsCategory::Account, "My Account"),
            (SettingsCategory::Privacy, "Privacy"),
            (SettingsCategory::Bots, "Bots"),
            (SettingsCategory::Voice, "Voice & Video"),
            (SettingsCategory::About, "About"),
        ];
        let mut sidebar = column![
            text("SETTINGS")
                .size(11)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
            Space::with_height(Length::Fixed(4.0)),
        ]
        .spacing(2);
        for (category, label) in categories {
            let is_active = self.settings_category == category;
            sidebar = sidebar.push(
                button(text(label).size(13))
                    .on_press(Message::SettingsCategoryChanged(category))
                    .width(Length::Fill)
                    .padding([12, 12])
                    .style(move |theme: &Theme, status| {
                        sidebar_item_style(theme, status, is_active)
                    }),
            );
        }

        let content = match self.settings_category {
            SettingsCategory::Account => column![
                settings_card(profile_section),
                settings_card(password_section),
                settings_card(logout_section),
            ]
            .spacing(20),
            SettingsCategory::Privacy => column![settings_card(privacy_section)].spacing(20),
            SettingsCategory::Bots => column![settings_card(bots_section)].spacing(20),
            SettingsCategory::Voice => column![settings_card(voice_section)].spacing(20),
            SettingsCategory::About => column![settings_card(about_section)].spacing(20),
        };

        let body = row![
            container(sidebar)
                .width(Length::Fixed(220.0))
                .height(Length::Fill)
                .padding(16)
                .style(sidebar_style),
            settings_pane(content),
        ]
        .height(Length::Fill);

        column![
            container(header).style(chat_header_style),
            body
        ]
        .height(Length::Fill)
        .into()
    }

}
