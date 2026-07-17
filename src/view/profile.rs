//! The read-only profile viewer (opened by clicking someone's name/avatar).

use iced::widget::{button, column, container, horizontal_space, row, text, Space};
use iced::{Background, Border, Color, Element, Length, Theme};

use crate::style::*;
use crate::*;

impl App {
    pub(crate) fn view_profile(&self) -> Element<'_, Message> {
        let header = row![
            button(text("← Back").size(13))
                .on_press(Message::CloseProfile)
                .style(secondary_button_style)
                .padding([8, 14]),
            horizontal_space(),
            text("Profile").size(18).style(|_theme: &Theme| text::Style {
                color: Some(c_text_primary())
            }),
            horizontal_space(),
            Space::with_width(Length::Fixed(72.0)),
        ]
        .align_y(iced::Alignment::Center)
        .padding(16);

        let body: Element<'_, Message> = if let Some(profile) = &self.viewing_profile {
            let online = is_online(profile.last_seen_at);
            let photo = self
                .avatar_image_cache
                .get(&profile.avatar_image_url)
                .cloned();
            let mut card = column![
                avatar(&profile.display_name, Some(online), &profile.avatar_color, photo),
                Space::with_height(Length::Fixed(8.0)),
                text(profile.display_name.clone()).size(22).style(
                    |_theme: &Theme| text::Style {
                        color: Some(c_text_primary()),
                    }
                ),
                text(format!("@{}", profile.username)).size(13).style(
                    |_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    }
                ),
                container(
                    text(if online { "● Online" } else { "○ Offline" })
                        .size(11)
                        .style(move |_theme: &Theme| text::Style {
                            color: Some(if online { c_online() } else { c_text_muted() }),
                        }),
                )
                .padding([4, 10])
                .style(move |_theme: &Theme| container::Style {
                    background: Some(Background::Color(if online {
                        c_accent_soft()
                    } else {
                        Color::from_rgba(0.30, 0.72, 0.52, 0.04)
                    })),
                    border: Border {
                        radius: r0(),
                        width: 1.0,
                        color: if online { c_border_strong() } else { c_border() },
                    },
                    ..Default::default()
                }),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center);

            if !profile.status_message.is_empty() {
                card = card.push(
                    text(profile.status_message.clone())
                        .size(14)
                        .shaping(iced::widget::text::Shaping::Advanced)
                        .style(|_theme: &Theme| text::Style {
                            color: Some(c_text_primary()),
                        }),
                );
            }
            if !profile.bio.is_empty() {
                card = card.push(
                    container(
                        text(profile.bio.clone())
                            .size(13)
                            .shaping(iced::widget::text::Shaping::Advanced)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_primary()),
                            }),
                    )
                    .width(Length::Fixed(380.0))
                    .padding(16)
                    .style(panel_box_style),
                );
            }

            if profile.is_staff {
                card = card.push(badge_chip(
                    "STAFF",
                    Color::from_rgb(0.82, 0.40, 0.28),
                    Color::WHITE,
                ));
            }

            let mut actions = row![].spacing(8);
            if profile.can_support_dm
                && self
                    .session
                    .as_ref()
                    .is_some_and(|s| s.user_id != profile.user_id)
            {
                actions = actions.push(
                    button(text("Support message").size(12))
                        .on_press(Message::OpenSupportDm(profile.user_id.clone()))
                        .padding([10, 14])
                        .style(accent_button_style),
                );
            }
            if profile.is_friend {
                actions = actions.push(
                    button(text("Message").size(12))
                        .on_press(Message::OpenConversationWithFriend(Friend {
                            user_id: profile.user_id.clone(),
                            username: profile.username.clone(),
                            display_name: profile.display_name.clone(),
                            last_seen_at: profile.last_seen_at,
                            presence: profile.presence.clone(),
                            avatar_color: profile.avatar_color.clone(),
                            avatar_image_url: profile.avatar_image_url.clone(),
                            public_key: String::new(),
                            status_message: profile.status_message.clone(),
                            nickname: profile.nickname.clone(),
                            favorite: profile.favorite,
                            private_note: profile.private_note.clone(),
                            friends_since: 0.0,
                            mutual_servers: profile.mutual_servers.clone(),
                            is_staff: profile.is_staff,
                        }))
                        .padding([10, 14])
                        .style(secondary_button_style),
                );
                actions = actions.push(
                    button(text(if profile.favorite { "★ Fav" } else { "☆ Fav" }).size(12))
                        .on_press(Message::ToggleFavorite(profile.user_id.clone()))
                        .padding([10, 14])
                        .style(secondary_button_style),
                );
            } else if profile.relation == "incoming" && !profile.request_id.is_empty() {
                actions = actions.push(
                    button(text("Accept request").size(12))
                        .on_press(Message::RespondRequest(profile.request_id.clone(), true))
                        .padding([10, 14])
                        .style(success_button_style),
                );
            } else if profile.relation == "outgoing" {
                actions = actions.push(muted_text("Friend request pending…", 12));
            } else if profile.relation != "self" {
                actions = actions.push(
                    button(
                        text(if self.friend_request_busy {
                            "Sending…"
                        } else {
                            "Add friend"
                        })
                        .size(12),
                    )
                    .on_press_maybe(
                        (!self.friend_request_busy)
                            .then_some(Message::SendFriendRequestToUser(profile.username.clone())),
                    )
                    .padding([10, 14])
                    .style(accent_button_style),
                );
            }

            if profile.relation != "self"
                && self.session.as_ref().is_some_and(|s| s.user_id != profile.user_id)
            {
                let is_blocked = self.blocked.iter().any(|b| b.user_id == profile.user_id);
                if is_blocked {
                    actions = actions.push(
                        button(text("Unblock").size(12))
                            .on_press(Message::UnblockUser(profile.user_id.clone()))
                            .padding([10, 14])
                            .style(secondary_button_style),
                    );
                } else if self.confirm_block_user_id.as_deref() == Some(profile.user_id.as_str()) {
                    actions = actions.push(
                        row![
                            muted_text("Block this person?", 12),
                            button(text("Yes, block").size(12))
                                .on_press(Message::BlockUser(profile.user_id.clone()))
                                .padding([10, 14])
                                .style(danger_button_style),
                            button(text("Cancel").size(12))
                                .on_press(Message::CancelBlockUser)
                                .padding([10, 14])
                                .style(secondary_button_style),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    );
                } else {
                    actions = actions.push(
                        button(text("Block").size(12))
                            .on_press(Message::ConfirmBlockUser(profile.user_id.clone()))
                            .padding([10, 14])
                            .style(danger_button_style),
                    );
                }
            }
            if let Some(server) = &self.selected_server {
                if let Some(member) =
                    self.server_members.iter().find(|m| m.user_id == profile.user_id)
                {
                    let role_badges: Element<'_, Message> = if member.is_owner {
                        badge_chip("Owner", c_warning(), Color::from_rgb(0.05, 0.05, 0.02))
                    } else if !member.roles.is_empty() {
                        let mut row_el = row![].spacing(4);
                        for tag in &member.roles {
                            row_el = row_el.push(badge_chip(
                                &tag.name,
                                parse_hex_color(&tag.color).unwrap_or_else(c_accent_soft),
                                Color::WHITE,
                            ));
                        }
                        row_el.into()
                    } else {
                        muted_text("No role", 12)
                    };
                    let mut role_section = column![
                        field_label_owned(format!("Role in {}", server.name)),
                        role_badges,
                    ]
                    .spacing(6)
                    .align_x(iced::Alignment::Center);

                    let can_manage = self.my_server_permissions & PERM_MANAGE_ROLES != 0;
                    if can_manage && !member.is_owner {
                        let is_open = self.member_role_picker_open.as_deref()
                            == Some(member.user_id.as_str());
                        role_section = role_section.push(
                            button(text(if is_open { "Close" } else { "+ Add role" }).size(11))
                                .on_press(Message::ToggleMemberRolePicker(member.user_id.clone()))
                                .padding([6, 10])
                                .style(if is_open {
                                    accent_button_style
                                } else {
                                    secondary_button_style
                                }),
                        );
                        if is_open {
                            role_section = role_section.push(self.member_role_picker_flyout(member));
                        }
                    }
                    card = card.push(role_section);
                }
            }
            if !profile.mutual_servers.is_empty() {
                card = card.push(muted_text(
                    format!("Servers in common: {}", profile.mutual_servers.join(", ")),
                    12,
                ));
            }
            card = card.push(actions);

            container(
                container(card)
                    .padding(32)
                    .style(section_card_style),
            )
            .width(Length::Fill)
            .padding(32)
            .center_x(Length::Fill)
            .into()
        } else if let Some(err) = &self.profile_error {
            container(
                container(text(err.clone()).size(13))
                    .padding(16)
                    .style(error_box_style),
            )
            .width(Length::Fill)
            .padding(32)
            .center_x(Length::Fill)
            .into()
        } else {
            container(muted_text("Loading profile…", 13))
                .width(Length::Fill)
                .padding(32)
                .center_x(Length::Fill)
                .into()
        };

        container(column![header, body])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(chat_area_style)
            .into()
    }

}
