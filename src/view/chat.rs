//! The main chat screen: sidebar (DMs/friends/servers/admin), the message
//! history + composer, the members drawer, and the in-call banner. This is
//! the biggest single screen in the app -- channel list, message rendering,
//! reactions, replies, attachments, typing indicators, and voice/call UI all
//! live here because they all share the same header/sidebar/composer frame.

use std::sync::atomic::Ordering;

use iced::widget::{
    button, checkbox, column, container, horizontal_space, image, mouse_area, row, scrollable,
    stack, text, text_input, Space,
};
use iced::{Background, Border, Color, ContentFit, Element, Length, Theme};

use crate::style::*;
use crate::*;

/// Friends-tab header banner, baked into the binary at compile time (no
/// loose file dependency at runtime).
const FRIENDS_BANNER_PNG: &[u8] = include_bytes!("../../assets/textures/friendsbaner.png");
/// App icon, reused for the DM/home rail button.
const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/textures/talkyssicon.png");
/// Friends rail-tab icon.
const FRIENDS_ICON_PNG: &[u8] = include_bytes!("../../assets/textures/friendsicon.png");

impl App {
    pub(crate) fn view_chat<'a>(&'a self, session: &'a Session) -> Element<'a, Message> {
        let unread_count = self.conversations.iter().filter(|c| c.unread).count();
        let home_active = self.selected_server.is_none()
            && matches!(
                self.sidebar_tab,
                SidebarTab::Chats | SidebarTab::Friends | SidebarTab::Requests | SidebarTab::Admin
            );

        // --- Left rail: home + scrollable servers + fixed + menu ---
        let home_btn: Element<'_, Message> = {
            button(
                container(
                    image(image::Handle::from_bytes(APP_ICON_PNG))
                        .width(Length::Fixed(32.0))
                        .height(Length::Fixed(32.0))
                        .content_fit(ContentFit::Cover),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center),
            )
            .on_press(Message::GoHome)
            .width(Length::Fixed(44.0))
            .height(Length::Fixed(44.0))
            .padding(0)
            .style(move |theme: &Theme, status| rail_button_style(theme, status, home_active))
            .into()
        };

        let mut server_col = column![].spacing(8).align_x(iced::Alignment::Center);
        for server in &self.servers {
            let active = self
                .selected_server
                .as_ref()
                .is_some_and(|s| s.server_id == server.server_id);
            let icon: Element<'_, Message> = if let Some(handle) =
                self.avatar_image_cache.get(&server.icon_url)
            {
                container(
                    image(handle.clone())
                        .width(Length::Fixed(44.0))
                        .height(Length::Fixed(44.0))
                        .content_fit(ContentFit::Cover),
                )
                .width(Length::Fixed(44.0))
                .height(Length::Fixed(44.0))
                .into()
            } else {
                let initial = server
                    .name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "#".into());
                container(
                    text(initial).size(16).font(mono()).style(move |_theme: &Theme| {
                        text::Style {
                            color: Some(if active {
                                Color::from_rgb(0.02, 0.05, 0.02)
                            } else {
                                c_text_primary()
                            }),
                        }
                    }),
                )
                .width(Length::Fixed(44.0))
                .height(Length::Fixed(44.0))
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .into()
            };
            let srv_btn = button(icon)
                .on_press(Message::SelectServer(server.clone()))
                .width(Length::Fixed(44.0))
                .height(Length::Fixed(44.0))
                .padding(0)
                .style(move |theme: &Theme, status| rail_button_style(theme, status, active));
            server_col = server_col.push(srv_btn);
        }

        let add_active = self.server_add_menu_open;
        let add_btn = button(
            container(text("+").size(22).font(mono()).style(move |_theme: &Theme| text::Style {
                color: Some(if add_active {
                    Color::from_rgb(0.02, 0.05, 0.02)
                } else {
                    c_accent()
                }),
            }))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill),
        )
        .on_press(Message::ToggleServerAddMenu)
        .width(Length::Fixed(44.0))
        .height(Length::Fixed(44.0))
        .padding(0)
        .style(move |theme: &Theme, status| rail_button_style(theme, status, add_active));

        let mut bottom_rail = column![add_btn].spacing(8).align_x(iced::Alignment::Center);
        bottom_rail = bottom_rail.push(rail_button_image(
            FRIENDS_ICON_PNG,
            SidebarTab::Friends,
            self.sidebar_tab,
            self.social_stats.friends_online as usize,
        ));
        bottom_rail = bottom_rail.push(rail_button(
            "In",
            SidebarTab::Requests,
            self.sidebar_tab,
            self.incoming_requests.len(),
        ));
        if session.is_admin || session.is_moderator {
            bottom_rail =
                bottom_rail.push(rail_button("Ad", SidebarTab::Admin, self.sidebar_tab, 0));
        }
        // Badge on home for unread chats
        let home_slot: Element<'_, Message> = if unread_count > 0 {
            stack![
                home_btn,
                container(
                    container(text(if unread_count > 9 {
                        "9+".into()
                    } else {
                        unread_count.to_string()
                    })
                    .size(9)
                    .font(mono()))
                    .padding([1, 3])
                    .style(|_theme: &Theme| container::Style {
                        background: Some(Background::Color(c_danger())),
                        text_color: Some(Color::WHITE),
                        border: Border {
                            radius: r0(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                )
                .width(Length::Fixed(44.0))
                .height(Length::Fixed(44.0))
                .align_x(iced::alignment::Horizontal::Right)
                .align_y(iced::alignment::Vertical::Top),
            ]
            .into()
        } else {
            home_btn
        };

        let rail_body = column![
            home_slot,
            container(Space::new(Length::Fixed(28.0), Length::Fixed(1.0))).style(
                |_theme: &Theme| container::Style {
                    background: Some(Background::Color(c_border_strong())),
                    ..Default::default()
                }
            ),
            scrollable(server_col).height(Length::Fill),
            container(Space::new(Length::Fixed(28.0), Length::Fixed(1.0))).style(
                |_theme: &Theme| container::Style {
                    background: Some(Background::Color(c_border_strong())),
                    ..Default::default()
                }
            ),
            bottom_rail,
        ]
        .spacing(8)
        .padding(iced::Padding {
            top: 12.0,
            right: 6.0,
            bottom: 12.0,
            left: 6.0,
        })
        .align_x(iced::Alignment::Center)
        .height(Length::Fill);

        let rail_container = container(rail_body)
            .width(Length::Fixed(68.0))
            .height(Length::Fill)
            .style(rail_style);

        // When a server is selected, force middle panel into server channels.
        let effective_tab = if self.selected_server.is_some()
            && !matches!(self.sidebar_tab, SidebarTab::Admin)
            && !matches!(self.sidebar_tab, SidebarTab::Friends)
            && !matches!(self.sidebar_tab, SidebarTab::Requests)
        {
            SidebarTab::Servers
        } else {
            self.sidebar_tab
        };

        let tab_title = match effective_tab {
            SidebarTab::Chats => "Direct",
            SidebarTab::Friends => "Friends",
            SidebarTab::Requests => "Invites",
            SidebarTab::Servers => self
                .selected_server
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("Server"),
            SidebarTab::Admin => "Admin",
        };
        let header_content: Element<'_, Message> = if effective_tab == SidebarTab::Friends {
            image(image::Handle::from_bytes(FRIENDS_BANNER_PNG))
                .width(Length::Fill)
                .height(Length::Fixed(48.0))
                .content_fit(ContentFit::Contain)
                .into()
        } else {
            column![
                text(tab_title.to_uppercase()).size(10).font(mono()).style(
                    |_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    }
                ),
                text(tab_title).size(16).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_primary()),
                }),
            ]
            .spacing(2)
            .into()
        };
        let mut channel_list = column![container(header_content)
        .padding(iced::Padding {
            top: 14.0,
            right: 14.0,
            bottom: 8.0,
            left: 14.0,
        })];

        // Mini menu: create / join server (from rail +).
        if self.server_add_menu_open {
            channel_list = channel_list.push(
                container(
                    column![
                        text("ADD SERVER").size(10).font(mono()).style(
                            |_theme: &Theme| text::Style {
                                color: Some(c_accent()),
                            }
                        ),
                        text_input("Name", &self.new_server_name_input)
                            .on_input(Message::NewServerNameChanged)
                            .on_submit(Message::CreateServer)
                            .padding(10)
                            .style(pill_input_style),
                        button(container(text("Create").size(13)).center_x(Length::Fill))
                            .on_press(Message::CreateServer)
                            .padding(10)
                            .width(Length::Fill)
                            .style(accent_button_style),
                        container(Space::new(Length::Fill, Length::Fixed(1.0))).style(
                            |_theme: &Theme| container::Style {
                                background: Some(Background::Color(c_border())),
                                ..Default::default()
                            }
                        ),
                        text_input("Invite code or link", &self.join_server_code_input)
                            .on_input(Message::JoinServerCodeChanged)
                            .on_submit(Message::JoinServer)
                            .padding(10)
                            .style(pill_input_style),
                        button(container(text("Join").size(13)).center_x(Length::Fill))
                            .on_press(Message::JoinServer)
                            .padding(10)
                            .width(Length::Fill)
                            .style(secondary_button_style),
                        button(text("Close").size(11))
                            .on_press(Message::ToggleServerAddMenu)
                            .style(link_button_style),
                    ]
                    .spacing(8),
                )
                .padding(12)
                .style(panel_box_style),
            );
            if let Some(status) = &self.server_status {
                channel_list = channel_list.push(muted_text(status.clone(), 11));
            }
        }

        let list_content: Element<'_, Message> = match effective_tab {
            SidebarTab::Chats => {
                let mut panel = column![].spacing(8);

                let toggle_label = if self.new_group_open {
                    "Cancel"
                } else {
                    "+ New group"
                };
                panel = panel.push(
                    button(text(toggle_label).size(12))
                        .on_press(Message::ToggleGroupPanel)
                        .padding([8, 12])
                        .style(secondary_button_style),
                );

                if self.new_group_open {
                    let mut group_panel = column![text_input(
                        "Group name",
                        &self.new_group_name_input
                    )
                    .on_input(Message::GroupNameInputChanged)
                    .padding(10)
                    .style(pill_input_style)]
                    .spacing(8);

                    for friend in &self.friends {
                        let user_id = friend.user_id.clone();
                        group_panel = group_panel.push(
                            checkbox(
                                friend.display_name.clone(),
                                self.new_group_selected.contains(&friend.user_id),
                            )
                            .on_toggle(move |_| Message::ToggleGroupMember(user_id.clone()))
                            .text_size(12),
                        );
                    }

                    group_panel = group_panel.push(
                        button(text("Create group").size(12))
                            .on_press(Message::CreateGroup)
                            .padding([8, 12])
                            .style(accent_button_style),
                    );
                    if let Some(status) = &self.group_create_status {
                        group_panel = group_panel.push(muted_text(status.clone(), 11));
                    }

                    panel = panel.push(container(group_panel).padding(12).style(panel_box_style));
                }

                if self.conversations.is_empty() {
                    panel = panel.push(
                        container(muted_text("No chats yet. Message a friend to start.", 13))
                            .padding([16, 8]),
                    );
                } else {
                    let mut list = column![].spacing(2);
                    for conv in &self.conversations {
                        let is_active = self.active_conversation.as_deref()
                            == Some(conv.conversation_id.as_str());
                        let show_dot = conv.unread && !is_active;
                        let dot_slot: Element<'_, Message> = if show_dot {
                            container(Space::new(Length::Fixed(0.0), Length::Fixed(0.0)))
                                .width(Length::Fixed(8.0))
                                .height(Length::Fixed(8.0))
                                .style(|_theme: &Theme| container::Style {
                                    background: Some(Background::Color(c_accent())),
                                    border: Border {
                                        radius: r0(),
                                        ..Default::default()
                                    },
                                    ..Default::default()
                                })
                                .into()
                        } else {
                            Space::new(Length::Fixed(8.0), Length::Fixed(8.0)).into()
                        };
                        let title_style = if show_dot {
                            text(conv.title.clone()).size(14).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(c_text_primary()),
                                }
                            })
                        } else {
                            text(conv.title.clone()).size(14)
                        };
                        let item = button(
                            row![dot_slot, title_style]
                                .spacing(10)
                                .align_y(iced::Alignment::Center),
                        )
                        .on_press(Message::OpenConversationDirect(conv.clone()))
                        .width(Length::Fill)
                        .padding([10, 12])
                        .style(move |theme: &Theme, status| {
                            sidebar_item_style(theme, status, is_active)
                        });
                        list = list.push(item);
                    }
                    panel = panel.push(scrollable(list).height(Length::Fill));
                }

                panel.into()
            }
            SidebarTab::Friends => {
                let stats = &self.social_stats;
                let mut panel = column![
                    container(
                        row![
                            muted_text(
                                format!(
                                    "{} friends · {} online · {} in · {} out",
                                    stats.friends_total,
                                    stats.friends_online,
                                    stats.incoming_pending,
                                    stats.outgoing_pending
                                ),
                                12,
                            ),
                            horizontal_space(),
                        ]
                        .spacing(8),
                    )
                    .padding([4, 2]),
                    row![
                        text_input("Search people or @username", &self.add_friend_input)
                            .on_input(Message::AddFriendInputChanged)
                            .on_submit(Message::SendFriendRequest)
                            .padding(10)
                            .width(Length::Fill)
                            .style(pill_input_style),
                        button(text("Add").size(13))
                            .on_press(Message::SendFriendRequest)
                            .padding([10, 14])
                            .style(accent_button_style),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    text_input("Optional note (max 200)", &self.add_friend_note)
                        .on_input(Message::AddFriendNoteChanged)
                        .on_submit(Message::SendFriendRequest)
                        .padding(10)
                        .width(Length::Fill)
                        .style(pill_input_style),
                    row![
                        filter_chip(
                            "All",
                            self.friends_filter == FriendsFilter::All,
                            Message::SetFriendsFilter(FriendsFilter::All),
                        ),
                        filter_chip(
                            "Online",
                            self.friends_filter == FriendsFilter::Online,
                            Message::SetFriendsFilter(FriendsFilter::Online),
                        ),
                        filter_chip(
                            "★ Fav",
                            self.friends_filter == FriendsFilter::Favorites,
                            Message::SetFriendsFilter(FriendsFilter::Favorites),
                        ),
                    ]
                    .spacing(6),
                ]
                .spacing(10);

                if let Some(status) = &self.add_friend_status {
                    panel = panel.push(muted_text(status.clone(), 12));
                }

                // Live search hits
                if !self.people_hits.is_empty() {
                    panel = panel.push(
                        text("Search results")
                            .size(12)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }),
                    );
                    for hit in &self.people_hits {
                        let photo = self
                            .avatar_image_cache
                            .get(&hit.avatar_image_url)
                            .cloned();
                        let online = hit.presence != "offline";
                        let mut actions = row![].spacing(6);
                        match hit.relation.as_str() {
                            "friends" => {
                                actions = actions.push(
                                    button(text("Message").size(11))
                                        .on_press(Message::OpenConversationWithFriend(Friend {
                                            user_id: hit.user_id.clone(),
                                            username: hit.username.clone(),
                                            display_name: hit.display_name.clone(),
                                            last_seen_at: 0.0,
                                            presence: hit.presence.clone(),
                                            avatar_color: hit.avatar_color.clone(),
                                            avatar_image_url: hit.avatar_image_url.clone(),
                                            public_key: String::new(),
                                            status_message: hit.status_message.clone(),
                                            nickname: String::new(),
                                            favorite: false,
                                            private_note: String::new(),
                                            friends_since: 0.0,
                                            mutual_servers: hit.mutual_servers.clone(),
                                            is_staff: hit.is_staff,
                                        }))
                                        .padding([6, 10])
                                        .style(accent_button_style),
                                );
                            }
                            "incoming" if !hit.incoming_request_id.is_empty() => {
                                actions = actions.push(
                                    button(text("Accept").size(11))
                                        .on_press(Message::RespondRequest(
                                            hit.incoming_request_id.clone(),
                                            true,
                                        ))
                                        .padding([6, 10])
                                        .style(success_button_style),
                                );
                            }
                            "outgoing" => {
                                actions = actions.push(muted_text("Pending…", 11));
                            }
                            _ => {
                                actions = actions.push(
                                    button(
                                        text(if self.friend_request_busy {
                                            "Sending…"
                                        } else {
                                            "Add"
                                        })
                                        .size(11),
                                    )
                                    .on_press_maybe((!self.friend_request_busy).then_some(
                                        Message::SendFriendRequestToUser(hit.username.clone()),
                                    ))
                                    .padding([6, 10])
                                    .style(accent_button_style),
                                );
                            }
                        }
                        actions = actions.push(
                            button(text("Profile").size(11))
                                .on_press(Message::OpenProfile(hit.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                        );
                        if self.confirm_block_user_id.as_deref() == Some(hit.user_id.as_str()) {
                            actions = actions.push(muted_text("Block?", 11));
                            actions = actions.push(
                                button(text("Yes").size(11))
                                    .on_press(Message::BlockUser(hit.user_id.clone()))
                                    .padding([6, 10])
                                    .style(danger_button_style),
                            );
                            actions = actions.push(
                                button(text("Cancel").size(11))
                                    .on_press(Message::CancelBlockUser)
                                    .padding([6, 10])
                                    .style(secondary_button_style),
                            );
                        } else {
                            actions = actions.push(
                                button(text("Block").size(11))
                                    .on_press(Message::ConfirmBlockUser(hit.user_id.clone()))
                                    .padding([6, 10])
                                    .style(danger_button_style),
                            );
                        }

                        let mut meta = format!("@{}", hit.username);
                        if !hit.mutual_servers.is_empty() {
                            meta.push_str(" · ");
                            meta.push_str(&hit.mutual_servers.join(", "));
                        }

                        let entry = column![
                            row![
                                avatar(
                                    &hit.display_name,
                                    Some(online),
                                    &hit.avatar_color,
                                    photo,
                                ),
                                column![
                                    text(&hit.display_name).size(14).style(|_theme: &Theme| {
                                        text::Style {
                                            color: Some(c_text_primary()),
                                        }
                                    }),
                                    muted_text(meta, 11),
                                ]
                                .spacing(2),
                            ]
                            .spacing(10)
                            .align_y(iced::Alignment::Center),
                            actions,
                        ]
                        .spacing(8);
                        panel = panel.push(container(entry).padding(10).style(panel_box_style));
                    }
                }

                // Suggestions
                if self.people_hits.is_empty() && !self.suggestions.is_empty() {
                    panel = panel.push(
                        text("Suggested from servers")
                            .size(12)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }),
                    );
                    for s in self.suggestions.iter().take(6) {
                        let photo = self.avatar_image_cache.get(&s.avatar_image_url).cloned();
                        let entry = row![
                            avatar(
                                &s.display_name,
                                Some(s.presence != "offline"),
                                &s.avatar_color,
                                photo,
                            ),
                            column![
                                text(&s.display_name).size(13).style(|_theme: &Theme| {
                                    text::Style {
                                        color: Some(c_text_primary()),
                                    }
                                }),
                                muted_text(
                                    if s.mutual_servers.is_empty() {
                                        format!("@{}", s.username)
                                    } else {
                                        format!(
                                            "@{} · {}",
                                            s.username,
                                            s.mutual_servers.join(", ")
                                        )
                                    },
                                    11,
                                ),
                            ]
                            .spacing(2)
                            .width(Length::Fill),
                            button(text("Add").size(11))
                                .on_press(Message::AddFriendInputChanged(s.username.clone()))
                                .padding([6, 10])
                                .style(accent_button_style),
                            button(text("Profile").size(11))
                                .on_press(Message::OpenProfile(s.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center);
                        panel = panel.push(container(entry).padding(10).style(panel_box_style));
                    }
                }

                let q = self.friends_filter_input.to_lowercase();
                let filtered: Vec<&Friend> = self
                    .friends
                    .iter()
                    .filter(|f| match self.friends_filter {
                        FriendsFilter::All => true,
                        FriendsFilter::Online => f.is_online_like(),
                        FriendsFilter::Favorites => f.favorite,
                    })
                    .filter(|f| {
                        if q.is_empty() {
                            return true;
                        }
                        f.label().to_lowercase().contains(&q)
                            || f.username.to_lowercase().contains(&q)
                            || f.display_name.to_lowercase().contains(&q)
                    })
                    .collect();

                if filtered.is_empty() {
                    panel = panel.push(
                        container(muted_text(
                            if self.friends.is_empty() {
                                "No friends yet. Search above or accept invites.".to_string()
                            } else {
                                "No friends match this filter.".to_string()
                            },
                            13,
                        ))
                        .padding([12, 4]),
                    );
                } else {
                    for friend in filtered {
                        let online = friend.is_online_like();
                        let title = if friend.favorite {
                            format!("★ {}", friend.label())
                        } else {
                            friend.label().to_string()
                        };
                        let mut subtitle = format!("@{}", friend.username);
                        if !friend.status_message.is_empty() {
                            subtitle.push_str(" · ");
                            subtitle.push_str(&friend.status_message);
                        } else if !friend.mutual_servers.is_empty() {
                            subtitle.push_str(" · ");
                            subtitle.push_str(&friend.mutual_servers.join(", "));
                        }

                        let info = row![
                            avatar(
                                friend.label(),
                                Some(online),
                                &friend.avatar_color,
                                self.avatar_image_cache
                                    .get(&friend.avatar_image_url)
                                    .cloned(),
                            ),
                            column![
                                text(title).size(14).style(|_theme: &Theme| {
                                    text::Style {
                                        color: Some(c_text_primary()),
                                    }
                                }),
                                muted_text(subtitle, 11),
                                muted_text(
                                    format!(
                                        "{} · friends since {}",
                                        presence_label(&friend.presence),
                                        format_relative_time(friend.friends_since)
                                    ),
                                    10,
                                ),
                            ]
                            .spacing(2),
                        ]
                        .spacing(10)
                        .align_y(iced::Alignment::Center);

                        let mut actions = row![
                            button(text(if friend.favorite { "★" } else { "☆" }).size(11))
                                .on_press(Message::ToggleFavorite(friend.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                            button(text("Message").size(11))
                                .on_press(Message::OpenConversationWithFriend(friend.clone()))
                                .padding([6, 10])
                                .style(accent_button_style),
                            button(text("Profile").size(11))
                                .on_press(Message::OpenProfile(friend.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                            button(text("Remove").size(11))
                                .on_press(Message::RemoveFriend(friend.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(6);
                        if self.confirm_block_user_id.as_deref() == Some(friend.user_id.as_str()) {
                            actions = actions.push(muted_text("Block?", 11));
                            actions = actions.push(
                                button(text("Yes").size(11))
                                    .on_press(Message::BlockUser(friend.user_id.clone()))
                                    .padding([6, 10])
                                    .style(danger_button_style),
                            );
                            actions = actions.push(
                                button(text("Cancel").size(11))
                                    .on_press(Message::CancelBlockUser)
                                    .padding([6, 10])
                                    .style(secondary_button_style),
                            );
                        } else {
                            actions = actions.push(
                                button(text("Block").size(11))
                                    .on_press(Message::ConfirmBlockUser(friend.user_id.clone()))
                                    .padding([6, 10])
                                    .style(danger_button_style),
                            );
                        }

                        let entry = column![info, actions].spacing(10);
                        panel = panel.push(container(entry).padding(12).style(panel_box_style));
                    }
                }

                if !self.blocked.is_empty() {
                    panel = panel.push(
                        text("Blocked")
                            .size(12)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }),
                    );
                    for blocked in &self.blocked {
                        let row_content = row![
                            text(&blocked.display_name).size(13).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(c_text_primary()),
                                }
                            }),
                            horizontal_space(),
                            button(text("Unblock").size(11))
                                .on_press(Message::UnblockUser(blocked.user_id.clone()))
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Center);
                        panel =
                            panel.push(container(row_content).padding(10).style(panel_box_style));
                    }
                }

                scrollable(panel).height(Length::Fill).into()
            }
            SidebarTab::Requests => {
                let mut list = column![].spacing(10);

                list = list.push(
                    row![
                        text("Incoming")
                            .size(12)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }),
                        horizontal_space(),
                        if !self.incoming_requests.is_empty() {
                            row![
                                button(text("Accept all").size(11))
                                    .on_press(Message::RespondAllIncoming(true))
                                    .padding([6, 10])
                                    .style(success_button_style),
                                button(text("Decline all").size(11))
                                    .on_press(Message::RespondAllIncoming(false))
                                    .padding([6, 10])
                                    .style(secondary_button_style),
                            ]
                            .spacing(6)
                        } else {
                            row![].spacing(6)
                        },
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                );

                if self.incoming_requests.is_empty() {
                    list = list.push(
                        container(muted_text("No pending invites.", 13)).padding([8, 4]),
                    );
                } else {
                    for req in &self.incoming_requests {
                        let photo = self
                            .avatar_image_cache
                            .get(&req.from_avatar_image_url)
                            .cloned();
                        let mut sub = format!(
                            "@{} · {}",
                            req.from_username,
                            format_relative_time(req.sent_at)
                        );
                        if !req.mutual_servers.is_empty() {
                            sub.push_str(" · ");
                            sub.push_str(&req.mutual_servers.join(", "));
                        }
                        let mut entry = column![
                            row![
                                avatar(
                                    &req.from_display_name,
                                    Some(req.presence != "offline"),
                                    &req.from_avatar_color,
                                    photo,
                                ),
                                column![
                                    text(req.from_display_name.clone()).size(14).style(
                                        |_theme: &Theme| text::Style {
                                            color: Some(c_text_primary()),
                                        }
                                    ),
                                    muted_text(sub, 11),
                                    if !req.from_status_message.is_empty() {
                                        muted_text(req.from_status_message.clone(), 11)
                                    } else {
                                        muted_text(String::new(), 1)
                                    },
                                ]
                                .spacing(2),
                            ]
                            .spacing(10)
                            .align_y(iced::Alignment::Center),
                        ]
                        .spacing(10);

                        if !req.note.is_empty() {
                            entry = entry.push(
                                container(
                                    text(req.note.clone())
                                        .size(12)
                                        .shaping(iced::widget::text::Shaping::Advanced)
                                        .style(|_theme: &Theme| text::Style {
                                            color: Some(c_text_primary()),
                                        }),
                                )
                                .padding(10)
                                .style(panel_box_style),
                            );
                        }

                        let mut req_actions = row![
                            button(text("Accept").size(12))
                                .on_press(Message::RespondRequest(
                                    req.request_id.clone(),
                                    true
                                ))
                                .padding([8, 12])
                                .style(success_button_style),
                            button(text("Decline").size(12))
                                .on_press(Message::RespondRequest(
                                    req.request_id.clone(),
                                    false
                                ))
                                .padding([8, 12])
                                .style(secondary_button_style),
                            button(text("Profile").size(12))
                                .on_press(Message::OpenProfile(req.from_user_id.clone()))
                                .padding([8, 12])
                                .style(secondary_button_style),
                        ]
                        .spacing(8);
                        if self.confirm_block_user_id.as_deref() == Some(req.from_user_id.as_str())
                        {
                            req_actions = req_actions.push(muted_text("Block?", 12));
                            req_actions = req_actions.push(
                                button(text("Yes").size(12))
                                    .on_press(Message::BlockUser(req.from_user_id.clone()))
                                    .padding([8, 12])
                                    .style(danger_button_style),
                            );
                            req_actions = req_actions.push(
                                button(text("Cancel").size(12))
                                    .on_press(Message::CancelBlockUser)
                                    .padding([8, 12])
                                    .style(secondary_button_style),
                            );
                        } else {
                            req_actions = req_actions.push(
                                button(text("Block").size(12))
                                    .on_press(Message::ConfirmBlockUser(req.from_user_id.clone()))
                                    .padding([8, 12])
                                    .style(danger_button_style),
                            );
                        }
                        entry = entry.push(req_actions);
                        list = list.push(container(entry).padding(14).style(panel_box_style));
                    }
                }

                list = list.push(
                    text("Outgoing")
                        .size(12)
                        .style(|_theme: &Theme| text::Style {
                            color: Some(c_text_muted()),
                        }),
                );

                if self.outgoing_requests.is_empty() {
                    list = list.push(
                        container(muted_text("No outgoing requests.", 13)).padding([8, 4]),
                    );
                } else {
                    for req in &self.outgoing_requests {
                        let photo = self
                            .avatar_image_cache
                            .get(&req.to_avatar_image_url)
                            .cloned();
                        let mut entry = column![
                            row![
                                avatar(
                                    &req.to_display_name,
                                    None,
                                    &req.to_avatar_color,
                                    photo,
                                ),
                                column![
                                    text(req.to_display_name.clone()).size(14).style(
                                        |_theme: &Theme| text::Style {
                                            color: Some(c_text_primary()),
                                        }
                                    ),
                                    text(format!("@{}", req.to_username)).size(11).style(
                                        |_theme: &Theme| text::Style {
                                            color: Some(c_text_muted()),
                                        }
                                    ),
                                    muted_text(
                                        format!("Sent · {}", format_relative_time(req.sent_at)),
                                        11,
                                    ),
                                ]
                                .spacing(2),
                            ]
                            .spacing(10)
                            .align_y(iced::Alignment::Center),
                        ]
                        .spacing(10);

                        if !req.note.is_empty() {
                            entry = entry.push(
                                muted_text(format!("Note: {}", req.note), 12),
                            );
                        }

                        entry = entry.push(
                            row![
                                button(text("Cancel").size(12))
                                    .on_press(Message::CancelOutgoingRequest(
                                        req.request_id.clone()
                                    ))
                                    .padding([8, 12])
                                    .style(danger_button_style),
                                button(text("Profile").size(12))
                                    .on_press(Message::OpenProfile(req.to_user_id.clone()))
                                    .padding([8, 12])
                                    .style(secondary_button_style),
                            ]
                            .spacing(8),
                        );
                        list = list.push(container(entry).padding(14).style(panel_box_style));
                    }
                }

                scrollable(list).height(Length::Fill).into()
            }
            SidebarTab::Servers => {
                if let Some(server) = &self.selected_server {
                    let mut panel = column![
                        text(server.name.clone()).size(16).style(|_theme: &Theme| {
                            text::Style {
                                color: Some(c_text_primary()),
                            }
                        }),
                        muted_text(
                            if server.custom_slug.is_empty() {
                                format!("{} channels", self.channels.len())
                            } else {
                                format!("/{} · {} channels", server.custom_slug, self.channels.len())
                            },
                            11,
                        ),
                    ]
                    .spacing(6);

                    let mut tools = row![].spacing(6);
                    if server.is_owner || session.is_admin {
                        tools = tools.push(
                            button(text("Settings").size(12))
                                .on_press(Message::ToggleServerSettings)
                                .padding([8, 12])
                                .style(secondary_button_style),
                        );
                    }
                    if !server.invite_code.is_empty() {
                        tools = tools.push(
                            button(text("Copy invite link").size(12))
                                .on_press(Message::CopyInviteLink(server.invite_code.clone()))
                                .padding([8, 12])
                                .style(secondary_button_style),
                        );
                    }
                    panel = panel.push(tools);

                    let can_manage_channels = self.my_server_permissions & PERM_MANAGE_CHANNELS != 0;
                    if can_manage_channels {
                        let toggle_label =
                            if self.new_channel_open { "Cancel" } else { "+ New channel" };
                        panel = panel.push(
                            button(text(toggle_label).size(12))
                                .on_press(Message::ToggleNewChannelInput)
                                .padding([8, 12])
                                .width(Length::Fill)
                                .style(secondary_button_style),
                        );
                    }
                    if can_manage_channels && self.new_channel_open {
                        panel = panel.push(
                            column![
                                row![
                                    text_input("channel-name", &self.new_channel_name_input)
                                        .on_input(Message::NewChannelNameChanged)
                                        .on_submit(Message::CreateChannel)
                                        .padding(10)
                                        .width(Length::Fill)
                                        .style(pill_input_style),
                                    button(text("Create").size(12))
                                        .on_press(Message::CreateChannel)
                                        .padding([10, 12])
                                        .style(accent_button_style),
                                ]
                                .spacing(8)
                                .align_y(iced::Alignment::Center),
                                row![
                                    button(
                                        text(if self.new_channel_is_voice {
                                            "Type: Voice"
                                        } else {
                                            "Type: Text"
                                        })
                                        .size(11),
                                    )
                                    .on_press(Message::NewChannelIsVoice(
                                        !self.new_channel_is_voice
                                    ))
                                    .padding([6, 10])
                                    .style(secondary_button_style),
                                    muted_text("Click to switch type", 10),
                                ]
                                .spacing(8)
                                .align_y(iced::Alignment::Center),
                            ]
                            .spacing(8),
                        );
                    }

                    if let Some(status) = &self.server_status {
                        panel = panel.push(status_banner(status, false));
                    }

                    if self.channels.is_empty() {
                        panel = panel.push(muted_text("No channels yet.", 13));
                    } else {
                        let mut list = column![].spacing(2);
                        let text_chs: Vec<_> = self
                            .channels
                            .iter()
                            .filter(|c| c.channel_type != "voice")
                            .collect();
                        let voice_chs: Vec<_> = self
                            .channels
                            .iter()
                            .filter(|c| c.channel_type == "voice")
                            .collect();
                        if !text_chs.is_empty() {
                            list = list.push(
                                text("TEXT CHANNELS")
                                    .size(10)
                                    .font(mono())
                                    .style(|_theme: &Theme| text::Style {
                                        color: Some(c_text_muted()),
                                    }),
                            );
                            for channel in text_chs {
                                let is_active = self.active_conversation.as_deref()
                                    == Some(channel.conversation_id.as_str());
                                let item = button(
                                    text(format!("#  {}", channel.name)).size(13).font(mono()),
                                )
                                .on_press(Message::OpenChannel(channel.clone()))
                                .width(Length::Fill)
                                .padding([10, 12])
                                .style(move |theme: &Theme, status| {
                                    sidebar_item_style(theme, status, is_active)
                                });
                                list = list.push(item);
                            }
                        }
                        if !voice_chs.is_empty() {
                            list = list.push(Space::with_height(Length::Fixed(8.0)));
                            list = list.push(
                                text("VOICE CHANNELS")
                                    .size(10)
                                    .font(mono())
                                    .style(|_theme: &Theme| text::Style {
                                        color: Some(c_text_muted()),
                                    }),
                            );
                            for channel in voice_chs {
                                let is_active = self.active_conversation.as_deref()
                                    == Some(channel.conversation_id.as_str());
                                let item =
                                    button(text(format!("v  {}", channel.name)).size(14))
                                        .on_press(Message::OpenChannel(channel.clone()))
                                        .width(Length::Fill)
                                        .padding([10, 12])
                                        .style(move |theme: &Theme, status| {
                                            sidebar_item_style(theme, status, is_active)
                                        });
                                list = list.push(item);
                            }
                        }
                        panel = panel.push(scrollable(list).height(Length::Fill));
                    }

                    panel.into()
                } else {
                    container(
                        column![
                            muted_text("Pick a server on the left rail.", 13),
                            muted_text("Or press + to create / join.", 12),
                        ]
                        .spacing(6),
                    )
                    .padding([16, 8])
                    .into()
                }
            }
            SidebarTab::Admin => {
                let search = self.admin_search_input.trim().to_lowercase();
                let filtered: Vec<&AdminUserRow> = self
                    .admin_users
                    .iter()
                    .filter(|u| {
                        search.is_empty()
                            || u.username.to_lowercase().contains(&search)
                            || u.display_name.to_lowercase().contains(&search)
                    })
                    .collect();

                let mut panel = column![text_input("Search users...", &self.admin_search_input)
                    .on_input(Message::AdminSearchInputChanged)
                    .padding(10)
                    .style(pill_input_style)]
                .spacing(10);

                if let Some(status) = &self.admin_status {
                    panel = panel.push(muted_text(status.clone(), 11));
                }

                if filtered.is_empty() {
                    panel = panel.push(muted_text("No users found.", 13));
                } else {
                    let mut list = column![].spacing(8);
                    for admin_user in filtered {
                        let role = admin_user.role.as_str();
                        let ban_label = if admin_user.banned { "Unban" } else { "Ban" };
                        let status_line = if admin_user.banned {
                            format!("{role} · banned")
                        } else {
                            role.to_string()
                        };
                        let mut actions = row![].spacing(6);
                        let role_locked = role == "owner" || admin_user.username == "v3nn7";
                        if session.is_admin && !role_locked {
                            actions = actions.push(
                                button(text("User").size(10))
                                    .on_press(Message::AdminSetPlatformRole(
                                        admin_user.user_id.clone(),
                                        "user".into(),
                                    ))
                                    .padding([6, 8])
                                    .style(secondary_button_style),
                            );
                            actions = actions.push(
                                button(text("Mod").size(10))
                                    .on_press(Message::AdminSetPlatformRole(
                                        admin_user.user_id.clone(),
                                        "moderator".into(),
                                    ))
                                    .padding([6, 8])
                                    .style(secondary_button_style),
                            );
                            actions = actions.push(
                                button(text("Admin").size(10))
                                    .on_press(Message::AdminSetPlatformRole(
                                        admin_user.user_id.clone(),
                                        "admin".into(),
                                    ))
                                    .padding([6, 8])
                                    .style(accent_button_style),
                            );
                        }
                        if !role_locked {
                            actions = actions.push(
                                button(text(ban_label).size(10))
                                    .on_press(Message::AdminSetBanned(
                                        admin_user.user_id.clone(),
                                        !admin_user.banned,
                                    ))
                                    .padding([6, 8])
                                    .style(danger_button_style),
                            );
                        } else {
                            actions = actions.push(
                                text("rank locked").size(10).style(|_theme: &Theme| {
                                    text::Style {
                                        color: Some(c_warning()),
                                    }
                                }),
                            );
                        }
                        let entry = column![
                            row![
                                text(format!(
                                    "{} (@{})",
                                    admin_user.display_name, admin_user.username
                                ))
                                .size(13)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_text_primary()),
                                }),
                                if role == "owner" {
                                    badge_chip(
                                        "OWNER",
                                        Color::from_rgb(0.88, 0.70, 0.28),
                                        Color::from_rgb(0.05, 0.04, 0.0),
                                    )
                                } else if role == "admin" {
                                    badge_chip(
                                        "STAFF",
                                        Color::from_rgb(0.82, 0.40, 0.28),
                                        Color::WHITE,
                                    )
                                } else if role == "moderator" {
                                    badge_chip(
                                        "MOD",
                                        Color::from_rgb(0.25, 0.55, 0.95),
                                        Color::WHITE,
                                    )
                                } else {
                                    Space::new(Length::Fixed(0.0), Length::Fixed(0.0)).into()
                                },
                            ]
                            .spacing(8)
                            .align_y(iced::Alignment::Center),
                            text(status_line).size(11).style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }),
                            actions,
                        ]
                        .spacing(8);
                        list = list.push(container(entry).padding(12).style(panel_box_style));
                    }
                    panel = panel.push(scrollable(list).height(Length::Fill));
                }

                panel.into()
            }
        };

        channel_list = channel_list.push(
            container(list_content)
                .padding(iced::Padding {
                    top: 0.0,
                    right: 12.0,
                    bottom: 8.0,
                    left: 12.0,
                })
                .height(Length::Fill),
        );

        let mut identity_line = row![text(&session.display_name).size(13).style(
            |_theme: &Theme| text::Style {
                color: Some(c_text_primary()),
            }
        )]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        if session.platform_role == "owner" {
            identity_line = identity_line.push(badge_chip(
                "OWNER",
                Color::from_rgb(0.88, 0.70, 0.28),
                Color::from_rgb(0.05, 0.04, 0.0),
            ));
        } else if session.is_admin {
            identity_line =
                identity_line.push(badge_chip("STAFF", Color::from_rgb(0.82, 0.40, 0.28), Color::WHITE));
        } else if session.is_moderator {
            identity_line =
                identity_line.push(badge_chip("MOD", Color::from_rgb(0.25, 0.55, 0.95), Color::WHITE));
        }
        let account_panel = container(
            row![
                avatar(
                    &session.display_name,
                    Some(true),
                    &session.avatar_color,
                    self.avatar_image_cache.get(&session.avatar_image_url).cloned(),
                ),
                column![
                    identity_line,
                    text("Online").size(10).style(|_theme: &Theme| text::Style {
                        color: Some(c_online()),
                    })
                ]
                .spacing(2),
                horizontal_space(),
                button(text("Settings").size(11))
                    .on_press(Message::OpenSettings)
                    .padding([8, 10])
                    .style(secondary_button_style),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .padding(12),
        )
        .style(account_panel_style);
        channel_list = channel_list.push(account_panel);

        let channel_list_container = container(channel_list)
            .width(Length::Fixed(self.channel_list_width))
            .height(Length::Fill)
            .style(sidebar_style);

        let chat_panel: Element<'_, Message> = match &self.active_conversation {
            None => {
                let welcome_icon = container(
                    text(">_")
                        .size(26)
                        .style(|_theme: &Theme| text::Style {
                            color: Some(Color::from_rgb(0.02, 0.05, 0.02)),
                        }),
                )
                .width(Length::Fixed(72.0))
                .height(Length::Fixed(72.0))
                .align_x(iced::alignment::Horizontal::Center)
                .align_y(iced::alignment::Vertical::Center)
                .style(|_theme: &Theme| container::Style {
                    background: Some(Background::Color(c_accent())),
                    border: Border {
                        radius: r0(),
                        width: 1.0,
                        color: c_accent(),
                    },
                    shadow: soft_shadow(),
                    ..Default::default()
                });

                container(
                    column![
                        welcome_icon,
                        Space::with_height(Length::Fixed(8.0)),
                        text("Welcome to Talkyss")
                            .size(28)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_primary())
                            }),
                        text("Pick a chat on the left, or add a friend to get started.")
                            .size(14)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_text_muted())
                            }),
                        Space::with_height(Length::Fixed(4.0)),
                        container(
                            text("[DM] chats · [Fr] friends · [+] servers")
                                .size(12)
                                .font(mono())
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_text_muted()),
                                }),
                        )
                        .padding([8, 14])
                        .style(panel_box_style),
                    ]
                    .spacing(10)
                    .align_x(iced::Alignment::Center),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(chat_area_style)
                .into()
            }
            Some(_) => {
                let peer = self
                    .active_peer_name
                    .clone()
                    .unwrap_or_else(|| "Chat".to_string());
                let peer_friend = self
                    .active_conversation_peer_id
                    .as_ref()
                    .and_then(|id| self.friends.iter().find(|f| &f.user_id == id));
                let header_icon: Element<'_, Message> = match peer_friend {
                    Some(friend) => avatar(
                        &friend.display_name,
                        Some(is_online(friend.last_seen_at)),
                        &friend.avatar_color,
                        self.avatar_image_cache.get(&friend.avatar_image_url).cloned(),
                    ),
                    None => text("#").size(18).style(|_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    }).into(),
                };
                let mut header_title = row![
                    header_icon,
                    text(peer).size(17).style(|_theme: &Theme| text::Style {
                        color: Some(c_text_primary())
                    }),
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center);
                if self.active_conversation_kind.as_deref() == Some("direct") {
                    let cur_peer_id = self.active_conversation_peer_id.as_deref();
                    let connected = cur_peer_id
                        .and_then(|id| self.peer_connected.get(id))
                        .copied()
                        .unwrap_or(false);
                    let label = if connected {
                        let fp = cur_peer_id
                            .and_then(|id| self.peer_remote_fp.get(id))
                            .map(String::as_str)
                            .unwrap_or("…");
                        let tr = cur_peer_id
                            .and_then(|id| self.peer_transport.get(id))
                            .map(String::as_str)
                            .unwrap_or("?");
                        format!("peerseal · {tr} · {fp}")
                    } else {
                        cur_peer_id
                            .and_then(|id| self.peer_status.get(id))
                            .cloned()
                            .unwrap_or_else(|| "Connecting secure channel…".to_string())
                    };
                    header_title = header_title.push(
                        container(
                            text(label)
                                .size(11)
                                .font(mono())
                                .shaping(iced::widget::text::Shaping::Advanced)
                                .style(move |_theme: &Theme| text::Style {
                                    color: Some(if connected {
                                        c_online()
                                    } else {
                                        c_warning()
                                    }),
                                }),
                        )
                        .padding([4, 10])
                        .style(move |_theme: &Theme| container::Style {
                            background: Some(Background::Color(if connected {
                                c_accent_soft()
                            } else {
                                Color::from_rgba(0.82, 0.74, 0.30, 0.10)
                            })),
                            border: Border {
                                radius: r0(),
                                width: 1.0,
                                color: if connected {
                                    c_border_strong()
                                } else {
                                    Color::from_rgba(0.82, 0.74, 0.30, 0.35)
                                },
                            },
                            ..Default::default()
                        }),
                    );
                    if let Some(sas) = cur_peer_id.and_then(|id| self.peer_sas.get(id)) {
                        header_title = header_title.push(
                            row![
                                text("SAS ")
                                    .size(11)
                                    .font(mono())
                                    .line_height(emoji_line_height(11.0))
                                    .style(|_theme: &Theme| text::Style {
                                        color: Some(c_text_muted()),
                                    }),
                                emoji_label(sas.as_str(), 14),
                            ]
                            .align_y(iced::Alignment::Center)
                            .height(Length::Fixed(22.0)),
                        );
                    }
                }
                header_title = header_title.push(horizontal_space());
                if self.active_conversation_kind.as_deref() == Some("direct")
                    && self.my_call.is_none()
                {
                    header_title = header_title.push(
                        button(text("Call").size(13))
                            .on_press(Message::StartCall)
                            .padding([8, 14])
                            .style(accent_button_style),
                    );
                }

                // Store / Clear only for DMs & groups — server channels stay on Convex.
                let is_server_channel = matches!(
                    self.active_conversation_kind.as_deref(),
                    Some("channel") | Some("voice")
                );
                if !is_server_channel {
                    header_title = header_title.push(
                        button(
                            text(if self.chat_store_enabled {
                                "Store: ON"
                            } else {
                                "Store: OFF"
                            })
                            .size(11),
                        )
                        .on_press(Message::ToggleStoreHistoryThisChat)
                        .padding([8, 10])
                        .style(if self.chat_store_enabled {
                            accent_button_style
                        } else {
                            secondary_button_style
                        }),
                    );
                    if !self.chat_store_allowed {
                        header_title = header_title.push(
                            text("ephemeral")
                                .size(10)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_warning()),
                                }),
                        );
                    }

                    if self.clear_chat_busy {
                        header_title = header_title.push(
                            text("Clearing…")
                                .size(12)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_text_muted()),
                                }),
                        );
                    } else if self.clear_chat_confirm {
                        header_title = header_title.push(
                            text("Wipe?")
                                .size(11)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_warning()),
                                }),
                        );
                        header_title = header_title.push(
                            button(text("Yes").size(12))
                                .on_press(Message::ConfirmClearChat)
                                .padding([8, 12])
                                .style(danger_button_style),
                        );
                        header_title = header_title.push(
                            button(text("No").size(12))
                                .on_press(Message::ToggleClearChatConfirm)
                                .padding([8, 12])
                                .style(secondary_button_style),
                        );
                    } else {
                        header_title = header_title.push(
                            button(text("Clear").size(12))
                                .on_press(Message::ToggleClearChatConfirm)
                                .padding([8, 12])
                                .style(secondary_button_style),
                        );
                    }
                }

                // Voice room controls: server voice channels + groups.
                let can_voice = matches!(
                    self.active_conversation_kind.as_deref(),
                    Some("voice") | Some("group")
                );
                if can_voice {
                    let in_voice = self.active_voice_channel.as_deref()
                        == self.active_conversation.as_deref();
                    header_title = header_title.push(
                        button(
                            text(if in_voice { "Leave voice" } else { "Join voice" }).size(12),
                        )
                        .on_press(if in_voice {
                            Message::LeaveVoiceChannel
                        } else {
                            Message::JoinVoiceChannel
                        })
                        .padding([8, 12])
                        .style(accent_button_style),
                    );
                    if in_voice {
                        if let Some(status) = &self.room_voice_status {
                            header_title = header_title.push(
                                text(status.clone())
                                    .size(11)
                                    .style(|_theme: &Theme| text::Style {
                                        color: Some(c_text_muted()),
                                    }),
                            );
                        }
                        if !self.voice_users.is_empty() {
                            let names: String = self
                                .voice_users
                                .iter()
                                .map(|u| u.display_name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            header_title = header_title.push(
                                text(format!("· {names}"))
                                    .size(11)
                                    .style(|_theme: &Theme| text::Style {
                                        color: Some(c_text_muted()),
                                    }),
                            );
                        }
                    }
                }

                let chat_header = container(header_title)
                    .padding([14, 18])
                    .width(Length::Fill)
                    .style(chat_header_style);

                let mut messages_col = column![].spacing(2).padding([12, 16]).width(Length::Fill);
                let mut last_author: Option<String> = None;
                let mut last_day: Option<String> = None;
                // Convex history + live peerseal messages for DMs.
                let display_messages: Vec<&ChatMessage> = {
                    let mut v: Vec<&ChatMessage> = self.messages.iter().collect();
                    if let Some(id) = &self.active_conversation_peer_id {
                        if let Some(live) = self.peer_live_messages.get(id) {
                            v.extend(live.iter());
                        }
                    }
                    v
                };
                for msg in display_messages {
                    let mine = msg.author_id == session.user_id;
                    let is_call_log = msg.kind == "call";
                    let deleted_visible = msg.deleted && (session.is_admin || mine);
                    let can_edit = mine && !msg.deleted && !is_call_log;
                    let can_delete = if is_call_log {
                        session.is_admin && !msg.deleted
                    } else {
                        (mine || session.is_admin) && !msg.deleted
                    };
                    let can_purge = session.is_admin && msg.deleted;
                    let is_hovered = self.hovered_message_id.as_deref() == Some(msg.id.as_str());

                    let day = format_day(msg.sent_at);
                    if last_day.as_deref() != Some(day.as_str()) {
                        messages_col = messages_col.push(date_separator(&day));
                        last_day = Some(day);
                        last_author = None;
                    }

                    if is_call_log {
                        last_author = None;
                        let log_color = if deleted_visible {
                            c_danger()
                        } else {
                            c_text_muted()
                        };
                        let log_text = msg.body.clone();
                        let mut log_content = column![container(
                            text(log_text)
                                .size(12)
                                .style(move |_theme: &Theme| text::Style {
                                    color: Some(log_color)
                                })
                        )
                        .width(Length::Fill)
                        .center_x(Length::Fill)]
                        .spacing(4);

                        if is_hovered && (can_delete || can_purge) {
                            let mut log_actions = row![].spacing(10);
                            if can_delete {
                                log_actions = log_actions.push(
                                    button(text("Delete").size(10))
                                        .on_press(Message::DeleteMessage(msg.id.clone()))
                                        .style(link_button_style),
                                );
                            }
                            if can_purge {
                                log_actions = log_actions.push(
                                    button(text("Delete forever").size(10))
                                        .on_press(Message::PurgeMessage(msg.id.clone()))
                                        .style(link_button_style),
                                );
                            }
                            log_content = log_content.push(
                                container(log_actions).width(Length::Fill).center_x(Length::Fill),
                            );
                        }

                        let log_row_container = container(log_content)
                            .width(Length::Fill)
                            .padding(iced::Padding {
                                top: 3.0,
                                right: 6.0,
                                bottom: 3.0,
                                left: 6.0,
                            })
                            .style(if is_hovered {
                                message_hover_style
                            } else {
                                transparent_container_style
                            });

                        let log_hoverable = mouse_area(log_row_container)
                            .on_enter(Message::MessageHovered(Some(msg.id.clone())))
                            .on_exit(Message::MessageHovered(None));

                        messages_col = messages_col.push(log_hoverable);
                        continue;
                    }

                    let grouped = last_author.as_deref() == Some(msg.author_id.as_str());
                    last_author = Some(msg.author_id.clone());

                    let name_color = if mine { c_accent() } else { Color::WHITE };
                    let can_react = !msg.deleted && !is_call_log;

                    // Fill remaining width so message text wraps inside the
                    // chat pane instead of expanding the row horizontally.
                    let mut line = column![].spacing(2).width(Length::Fill);
                    if !grouped {
                        let mut meta = format_time(msg.sent_at);
                        if msg.edited {
                            meta = format!("{meta} (edited)");
                        }
                        let mut author_line = row![
                            text(msg.author_name.clone())
                                .size(14)
                                .line_height(chat_body_line_height())
                                .style(move |_theme: &Theme| text::Style {
                                    color: Some(name_color)
                                }),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Center);
                        if msg.author_is_bot {
                            author_line = author_line.push(
                                container(
                                    text("*bot").size(9).font(mono()).style(|_theme: &Theme| {
                                        text::Style {
                                            color: Some(Color::from_rgb(0.02, 0.05, 0.02)),
                                        }
                                    }),
                                )
                                .padding([1, 4])
                                .style(|_theme: &Theme| container::Style {
                                    background: Some(Background::Color(c_accent())),
                                    border: Border {
                                        radius: r0(),
                                        ..Default::default()
                                    },
                                    ..Default::default()
                                }),
                            );
                        }
                        author_line = author_line.push(
                            text(meta).size(10).font(mono()).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(c_text_muted()),
                                }
                            }),
                        );
                        line = line.push(author_line);
                    }

                    if let Some((reply_author, reply_snippet)) = &msg.reply_to {
                        line = line.push(
                            text(format!("↩ {reply_author}: {reply_snippet}"))
                                .size(11)
                                .line_height(emoji_line_height(11.0))
                                .shaping(iced::widget::text::Shaping::Advanced)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_text_muted()),
                                }),
                        );
                    }

                    let body_text = if deleted_visible {
                        if msg.body.is_empty() {
                            "(deleted)".to_string()
                        } else {
                            format!("{} (deleted)", msg.body)
                        }
                    } else {
                        msg.body.clone()
                    };
                    let body_color = if deleted_visible {
                        c_danger()
                    } else {
                        c_text_primary()
                    };
                    if !body_text.is_empty() {
                        // Plain text (not text_editor): color-emoji fallback
                        // fonts have huge vertical metrics that explode row
                        // height inside text_editor, and the editor also
                        // steals scroll/hover from the chat list.
                        line = line.push(
                            text(body_text)
                                .size(14)
                                .line_height(chat_body_line_height())
                                .width(Length::Fill)
                                .shaping(iced::widget::text::Shaping::Advanced)
                                .style(move |_theme: &Theme| text::Style {
                                    color: Some(body_color),
                                }),
                        );
                    }

                    if !deleted_visible && !msg.attachment_url.is_empty() {
                        if let Some(handle) =
                            self.avatar_image_cache.get(&msg.attachment_url).cloned()
                        {
                            let url = msg.attachment_url.clone();
                            line = line.push(
                                mouse_area(
                                    container(
                                        column![
                                            image(handle)
                                                .width(Length::Fixed(280.0))
                                                .height(Length::Fixed(210.0))
                                                .content_fit(ContentFit::Contain),
                                            text("click to enlarge")
                                                .size(10)
                                                .style(|_theme: &Theme| text::Style {
                                                    color: Some(c_accent()),
                                                }),
                                        ]
                                        .spacing(4)
                                        .align_x(iced::Alignment::Center),
                                    )
                                    .width(Length::Fixed(288.0))
                                    .padding(4)
                                    .style(attachment_frame_style),
                                )
                                .on_press(Message::OpenAttachmentPreview(url)),
                            );
                        } else {
                            line = line.push(text("[loading image...]").size(12).style(
                                |_theme: &Theme| text::Style {
                                    color: Some(c_text_muted()),
                                },
                            ));
                        }
                    }

                    if !deleted_visible && !msg.reactions.is_empty() {
                        let mut pill_row = row![].spacing(4).align_y(iced::Alignment::Center);
                        for (emoji, count, reacted_by_me) in &msg.reactions {
                            let reacted_by_me = *reacted_by_me;
                            // Single text widget (not nested row) — nested
                            // rows inside buttons + Segoe metrics broke pill
                            // sizing and the whole message row.
                            pill_row = pill_row.push(
                                button(
                                    text(format!("{emoji} {count}"))
                                        .size(13)
                                        .font(emoji_font())
                                        .line_height(emoji_line_height(13.0))
                                        .shaping(iced::widget::text::Shaping::Advanced),
                                )
                                    .on_press(Message::ToggleReaction(
                                        msg.id.clone(),
                                        emoji.clone(),
                                    ))
                                    .padding(iced::Padding {
                                        top: 2.0,
                                        right: 8.0,
                                        bottom: 2.0,
                                        left: 8.0,
                                    })
                                    .style(move |theme: &Theme, status| {
                                        reaction_pill_style(theme, status, reacted_by_me)
                                    }),
                            );
                        }
                        line = line.push(pill_row);
                    }

                    let avatar_slot: Element<'_, Message> = if grouped {
                        Space::new(Length::Fixed(36.0), Length::Shrink).into()
                    } else {
                        let photo = self.avatar_image_cache.get(&msg.author_avatar_url).cloned();
                        avatar(&msg.author_name, None, &msg.author_avatar_color, photo)
                    };

                    let mut message_row = column![
                        row![avatar_slot, line]
                            .spacing(12)
                            .width(Length::Fill)
                            .align_y(iced::Alignment::Start)
                    ]
                    .spacing(4)
                    .width(Length::Fill);

                    if is_hovered && (can_edit || can_delete || can_purge || can_react) {
                        let mut actions = row![].spacing(10).align_y(iced::Alignment::Center);
                        if can_react {
                            for emoji in QUICK_REACT_EMOJIS {
                                actions = actions.push(
                                    button(emoji_label(emoji, 16))
                                        .on_press(Message::ToggleReaction(
                                            msg.id.clone(),
                                            emoji.to_string(),
                                        ))
                                        .padding([2, 4])
                                        .style(link_button_style),
                                );
                            }
                            let reply_snippet: String = if msg.body.chars().count() > 60 {
                                format!("{}...", msg.body.chars().take(60).collect::<String>())
                            } else {
                                msg.body.clone()
                            };
                            actions = actions.push(
                                button(text("Reply").size(10))
                                    .on_press(Message::ReplyToMessage(
                                        msg.id.clone(),
                                        msg.author_name.clone(),
                                        reply_snippet,
                                    ))
                                    .style(link_button_style),
                            );
                            actions = actions.push(
                                button(text("Copy").size(10))
                                    .on_press(Message::CopyMessage(msg.body.clone()))
                                    .style(link_button_style),
                            );
                        }
                        if can_edit {
                            actions = actions.push(
                                button(text("Edit").size(10))
                                    .on_press(Message::EditMessage(
                                        msg.id.clone(),
                                        msg.body.clone(),
                                        msg.encrypted,
                                    ))
                                    .style(link_button_style),
                            );
                        }
                        if can_delete {
                            actions = actions.push(
                                button(text("Delete").size(10))
                                    .on_press(Message::DeleteMessage(msg.id.clone()))
                                    .style(link_button_style),
                            );
                        }
                        if can_purge {
                            actions = actions.push(
                                button(text("Delete forever").size(10))
                                    .on_press(Message::PurgeMessage(msg.id.clone()))
                                    .style(link_button_style),
                            );
                        }
                        message_row = message_row.push(container(actions).padding(iced::Padding {
                            top: 0.0,
                            right: 0.0,
                            bottom: 0.0,
                            left: 48.0,
                        }));
                    }

                    let row_container = container(message_row)
                        .width(Length::Fill)
                        .padding(iced::Padding {
                            top: 3.0,
                            right: 6.0,
                            bottom: 3.0,
                            left: 6.0,
                        })
                        .style(if is_hovered {
                            message_hover_style
                        } else {
                            transparent_container_style
                        });

                    let hoverable = mouse_area(row_container)
                        .on_enter(Message::MessageHovered(Some(msg.id.clone())))
                        .on_exit(Message::MessageHovered(None));

                    messages_col = messages_col.push(hoverable);
                }

                let history = container(
                    scrollable(messages_col)
                        .id(chat_scroll_id())
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .style(chat_area_style);

                let is_editing = self.editing_message_id.is_some();
                let input_placeholder = if is_editing {
                    "Edit message..."
                } else {
                    "Type a message..."
                };
                let send_label = if is_editing { "Save" } else { "Send" };
                let peer_connected_now = self
                    .active_conversation_peer_id
                    .as_deref()
                    .and_then(|id| self.peer_connected.get(id))
                    .copied()
                    .unwrap_or(false);
                let crypto_ready = self.active_conversation_kind.as_deref() != Some("direct")
                    || peer_connected_now;
                let input_placeholder = if self.active_conversation_kind.as_deref()
                    == Some("direct")
                    && !peer_connected_now
                {
                    "Waiting for secure channel…"
                } else {
                    input_placeholder
                };

                let mut input_row = row![].spacing(10).align_y(iced::Alignment::Center);
                if !is_editing && crypto_ready {
                    input_row = input_row.push(
                        button(text("+").size(16).font(mono()))
                        .on_press(Message::PickAttachmentImage)
                        .padding([10, 12])
                        .style(secondary_button_style),
                    );
                }
                let mut message_input = text_input(input_placeholder, &self.message_input)
                    .on_input(Message::MessageInputChanged)
                    .padding(14)
                    .width(Length::Fill)
                    .style(pill_input_style);
                if crypto_ready {
                    message_input = message_input.on_submit(Message::SendMessage);
                }
                // Terminal prompt marker in front of the input.
                input_row = input_row.push(
                    text(">")
                        .size(16)
                        .font(mono())
                        .style(|_theme: &Theme| text::Style {
                            color: Some(c_accent()),
                        }),
                );
                input_row = input_row.push(message_input);
                input_row = input_row.push(
                    button(text(send_label.to_uppercase()).size(13).font(mono()))
                        .on_press_maybe(crypto_ready.then_some(Message::SendMessage))
                        .padding([12, 18])
                        .style(accent_button_style),
                );

                if is_editing {
                    input_row = input_row.push(
                        button(text("Cancel").size(13))
                            .on_press(Message::CancelEdit)
                            .padding([12, 14])
                            .style(secondary_button_style),
                    );
                }

                let mut composer_col = column![].spacing(8);
                if let Some(error) = &self.chat_error {
                    composer_col = composer_col.push(
                        container(text(error.clone()).size(12))
                            .padding(10)
                            .width(Length::Fill)
                            .style(error_box_style),
                    );
                } else if self.active_conversation_kind.as_deref() == Some("direct")
                    && !peer_connected_now
                {
                    composer_col = composer_col.push(
                        container(
                            text(
                                self.active_conversation_peer_id
                                    .as_deref()
                                    .and_then(|id| self.peer_status.get(id))
                                    .map(String::as_str)
                                    .unwrap_or(
                                        "Connecting secure channel…",
                                    ),
                            )
                            .size(12)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_warning()),
                            }),
                        )
                        .padding(10)
                        .width(Length::Fill)
                        .style(|_theme: &Theme| container::Style {
                            background: Some(Background::Color(Color::from_rgba(
                                0.82, 0.74, 0.30, 0.10,
                            ))),
                            border: Border {
                                radius: r0(),
                                width: 1.0,
                                color: Color::from_rgba(0.82, 0.74, 0.30, 0.35),
                            },
                            ..Default::default()
                        }),
                    );
                }
                composer_col = composer_col.push(input_row);

                let attachment_preview: Element<'_, Message> = match &self.pending_attachment {
                    Some(att) => container(
                        row![
                            container(
                                image(att.preview.clone())
                                    .width(Length::Fixed(48.0))
                                    .height(Length::Fixed(48.0))
                                    .content_fit(ContentFit::Cover),
                            )
                            .style(attachment_frame_style),
                            text("Image attached").size(12).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(c_text_muted()),
                                }
                            }),
                            horizontal_space(),
                            button(text("Remove").size(11))
                                .on_press(Message::RemovePendingAttachment)
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(12)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding(10)
                    .style(panel_box_style)
                    .into(),
                    None => Space::new(Length::Shrink, Length::Fixed(0.0)).into(),
                };

                let reply_preview: Element<'_, Message> = match &self.pending_reply {
                    Some((_, author, snippet)) => container(
                        row![
                            text(format!("↩  Replying to {author}: {snippet}"))
                                .size(12)
                                .shaping(iced::widget::text::Shaping::Advanced)
                                .style(|_theme: &Theme| text::Style {
                                    color: Some(c_text_primary()),
                                }),
                            horizontal_space(),
                            button(text("Cancel").size(11))
                                .on_press(Message::CancelReply)
                                .padding([6, 10])
                                .style(secondary_button_style),
                        ]
                        .spacing(10)
                        .align_y(iced::Alignment::Center),
                    )
                    .padding(10)
                    .style(reply_preview_style)
                    .into(),
                    None => Space::new(Length::Shrink, Length::Fixed(0.0)).into(),
                };

                let typing_line: Element<'_, Message> = match typing_label(&self.typing_names) {
                    Some(label) => container(
                        text(format!("▌ {label}"))
                            .size(11)
                            .font(mono())
                            .shaping(iced::widget::text::Shaping::Advanced)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_accent()),
                            }),
                    )
                    .into(),
                    None => Space::new(Length::Shrink, Length::Fixed(0.0)).into(),
                };

                let input_bar = container(
                    column![typing_line, reply_preview, attachment_preview, composer_col]
                        .spacing(8)
                        .padding(iced::Padding {
                            top: 10.0,
                            right: 18.0,
                            bottom: 16.0,
                            left: 18.0,
                        }),
                )
                .width(Length::Fill)
                .style(composer_style);

                column![chat_header, history, input_bar].into()
            }
        };

        // Chat always fills remaining space; members drawer only when a server is open.
        let chat_fill = container(chat_panel)
            .width(Length::Fill)
            .height(Length::Fill);
        let main_row = if self.selected_server.is_some() {
            row![
                rail_container,
                channel_list_container,
                resize_handle(ResizePanel::ChannelList),
                chat_fill,
                self.view_members_drawer(),
            ]
            .height(Length::Fill)
        } else {
            row![
                rail_container,
                channel_list_container,
                resize_handle(ResizePanel::ChannelList),
                chat_fill,
            ]
            .height(Length::Fill)
        };

        match self.call_banner_view() {
            Some(banner) => column![banner, main_row].into(),
            None => main_row.into(),
        }
    }

    pub(crate) fn view_members_drawer(&self) -> Element<'_, Message> {
        // Snap to clean widths (animation still lerps) — no overlapping float.
        let w = if self.members_panel_width < 100.0 {
            28.0
        } else {
            220.0_f32.min(self.members_panel_width.max(180.0))
        };
        let collapsed = w <= 32.0;

        if collapsed {
            return container(
                column![
                    Space::with_height(Length::Fixed(12.0)),
                    button(text("«").size(14))
                        .on_press(Message::ToggleMembersPanel)
                        .padding([8, 4])
                        .style(secondary_button_style),
                    Space::with_height(Length::Fill),
                    text(format!("{}", self.server_members.len()))
                        .size(9)
                        .style(|_theme: &Theme| text::Style {
                            color: Some(c_text_muted()),
                        }),
                    Space::with_height(Length::Fixed(12.0)),
                ]
                .align_x(iced::Alignment::Center)
                .width(Length::Fill)
                .height(Length::Fill),
            )
            .width(Length::Fixed(28.0))
            .height(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(c_bg_secondary())),
                border: Border {
                    color: c_border(),
                    width: 1.0,
                    radius: r0(),
                },
                ..Default::default()
            })
            .into();
        }

        let online: Vec<_> = self
            .server_members
            .iter()
            .filter(|m| !m.is_bot && is_online(m.last_seen_at))
            .collect();
        let offline: Vec<_> = self
            .server_members
            .iter()
            .filter(|m| !m.is_bot && !is_online(m.last_seen_at))
            .collect();
        let bots: Vec<_> = self.server_members.iter().filter(|m| m.is_bot).collect();

        let mut col = column![
            row![
                text("MEMBERS").size(11).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
                horizontal_space(),
                button(text("›").size(14))
                    .on_press(Message::ToggleMembersPanel)
                    .padding([2, 6])
                    .style(link_button_style),
            ]
            .align_y(iced::Alignment::Center),
            text(format!(
                "{} online · {} total",
                online.len(),
                self.server_members.len()
            ))
            .size(10)
            .style(|_theme: &Theme| text::Style {
                color: Some(c_text_muted()),
            }),
        ]
        .spacing(6)
        .padding(10);

        let mut list = column![].spacing(2);
        if !online.is_empty() {
            list = list.push(
                text(format!("ONLINE — {}", online.len()))
                    .size(10)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_online()),
                    }),
            );
            for m in online {
                list = list.push(self.member_row_ui(m, true));
            }
        }
        if !offline.is_empty() {
            list = list.push(Space::with_height(Length::Fixed(6.0)));
            list = list.push(
                text(format!("OFFLINE — {}", offline.len()))
                    .size(10)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    }),
            );
            for m in offline.iter().take(40) {
                list = list.push(self.member_row_ui(m, false));
            }
        }
        if !bots.is_empty() {
            list = list.push(Space::with_height(Length::Fixed(6.0)));
            list = list.push(
                text(format!("BOTS — {}", bots.len()))
                    .size(10)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_accent()),
                    }),
            );
            for m in bots {
                list = list.push(self.member_row_ui(m, is_online(m.last_seen_at)));
            }
        }

        col = col.push(scrollable(list).height(Length::Fill));

        row![
            resize_handle(ResizePanel::Members),
            container(col)
                .width(Length::Fixed(w))
                .height(Length::Fill)
                .style(sidebar_style),
        ]
        .height(Length::Fill)
        .into()
    }

    pub(crate) fn member_row_ui<'a>(&'a self, m: &'a ServerMemberRow, online: bool) -> Element<'a, Message> {
        let mut name_row = row![
            text(m.display_name.clone()).size(12).style(|_theme: &Theme| {
                text::Style {
                    color: Some(c_text_primary()),
                }
            }),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        if m.is_bot {
            name_row = name_row.push(badge_chip("*bot", c_accent(), Color::from_rgb(0.02, 0.05, 0.02)));
        }
        if m.platform_role == "owner" {
            name_row = name_row.push(badge_chip(
                "OWNER",
                Color::from_rgb(0.88, 0.70, 0.28),
                Color::from_rgb(0.05, 0.04, 0.0),
            ));
        } else if m.platform_role == "admin" {
            name_row = name_row.push(badge_chip(
                "STAFF",
                Color::from_rgb(0.82, 0.40, 0.28),
                Color::WHITE,
            ));
        } else if m.platform_role == "moderator" {
            name_row = name_row.push(badge_chip(
                "MOD",
                Color::from_rgb(0.25, 0.55, 0.95),
                Color::WHITE,
            ));
        }
        if m.is_owner {
            name_row = name_row.push(badge_chip(
                "OWNER",
                c_warning(),
                Color::from_rgb(0.05, 0.05, 0.02),
            ));
        }

        let mut info_col = column![name_row].spacing(1);
        if !m.roles.is_empty() {
            let mut role_badges = row![].spacing(3);
            for tag in &m.roles {
                role_badges = role_badges.push(badge_chip(
                    &tag.name,
                    parse_hex_color(&tag.color).unwrap_or_else(c_accent_soft),
                    Color::WHITE,
                ));
            }
            info_col = info_col.push(role_badges);
        }

        mouse_area(
            row![
                avatar(
                    &m.display_name,
                    Some(online),
                    &m.avatar_color,
                    self.avatar_image_cache.get(&m.avatar_image_url).cloned(),
                ),
                info_col,
            ]
            .spacing(8)
            .padding([4, 2])
            .align_y(iced::Alignment::Center),
        )
        .on_press(Message::OpenProfile(m.user_id.clone()))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
    }

    pub(crate) fn call_banner_view(&self) -> Option<Element<'_, Message>> {
        let call = self.my_call.as_ref()?;

        let is_ringing = call.status == "ringing";
        let (label, actions): (String, Element<'_, Message>) = match call.status.as_str() {
            "ringing" if !call.is_caller => (
                format!("Incoming call from {}", call.peer_display_name),
                row![
                    button(text("Accept").size(13))
                        .on_press(Message::AcceptCall)
                        .padding([8, 14])
                        .style(success_button_style),
                    button(text("Decline").size(13))
                        .on_press(Message::DeclineCall)
                        .padding([8, 14])
                        .style(danger_button_style),
                ]
                .spacing(8)
                .into(),
            ),
            "ringing" => (
                format!("Calling {}…", call.peer_display_name),
                button(text("Cancel").size(13))
                    .on_press(Message::HangUp)
                    .padding([8, 14])
                    .style(secondary_button_style)
                    .into(),
            ),
            "active" => {
                let muted = self.call_muted.load(Ordering::Relaxed);
                let status = self
                    .call_status_text
                    .clone()
                    .unwrap_or_else(|| format!("On call with {}", call.peer_display_name));
                let share_label = if self.is_sharing {
                    "Stop sharing"
                } else {
                    "Share screen"
                };
                let share_message = if self.is_sharing {
                    Message::StopShare
                } else {
                    Message::ToggleSharePicker
                };
                let output_muted = self.call_output_muted.load(Ordering::Relaxed);
                let all_muted = muted && output_muted;
                (
                    status,
                    row![
                        button(text(if muted { "Unmute" } else { "Mute" }).size(13))
                            .on_press(Message::ToggleMute)
                            .padding([8, 12])
                            .style(secondary_button_style),
                        button(text(if all_muted { "Unmute all" } else { "Mute all" }).size(13))
                            .on_press(Message::ToggleMuteAll)
                            .padding([8, 12])
                            .style(secondary_button_style),
                        button(text(share_label).size(13))
                            .on_press(share_message)
                            .padding([8, 12])
                            .style(secondary_button_style),
                        button(text("Hang up").size(13))
                            .on_press(Message::HangUp)
                            .padding([8, 12])
                            .style(danger_button_style),
                    ]
                    .spacing(8)
                    .into(),
                )
            }
            _ => return None,
        };

        let banner = container(
            row![
                text(label)
                    .size(13)
                    .shaping(iced::widget::text::Shaping::Advanced)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_text_primary())
                    }),
                horizontal_space(),
                actions,
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .padding([12, 16]),
        )
        .width(Length::Fill)
        .style(if is_ringing {
            call_banner_ringing_style
        } else {
            call_banner_style
        });

        let mut stack = column![banner];

        if call.status == "active" && self.share_picker_open {
            let mut picker = row![text("Share:").size(12).style(|_theme: &Theme| {
                text::Style {
                    color: Some(c_text_muted()),
                }
            })]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            if self.share_targets.is_empty() {
                picker = picker.push(text("Loading sources...").size(12).style(
                    |_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    },
                ));
            } else {
                for target in &self.share_targets {
                    picker = picker.push(
                        button(text(target.label()).size(12))
                            .on_press(Message::StartShare(target.clone()))
                            .style(link_button_style),
                    );
                }
            }
            stack = stack.push(
                container(scrollable(picker).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new(),
                )))
                .width(Length::Fill)
                .padding(iced::Padding {
                    top: 6.0,
                    right: 16.0,
                    bottom: 6.0,
                    left: 16.0,
                })
                .style(chat_area_style),
            );
        }

        if let Some(handle) = &self.remote_share_frame {
            let expanded = self.share_view_expanded;
            let frame_header = row![
                text(format!("{}'s screen", call.peer_display_name))
                    .size(12)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_text_muted()),
                    }),
                horizontal_space(),
                button(text(if expanded { "Shrink" } else { "Enlarge" }).size(12))
                    .on_press(Message::ToggleShareViewSize)
                    .style(link_button_style),
            ]
            .align_y(iced::Alignment::Center)
            .padding(iced::Padding {
                top: 0.0,
                right: 16.0,
                bottom: 0.0,
                left: 16.0,
            });

            let (frame_width, frame_height) = if expanded {
                (Length::Fill, Length::Fixed(680.0))
            } else {
                (Length::Fixed(480.0), Length::Fixed(270.0))
            };

            stack = stack.push(
                container(
                    column![
                        frame_header,
                        container(
                            image(handle.clone())
                                .width(frame_width)
                                .height(frame_height)
                                .content_fit(ContentFit::Contain),
                        )
                        .width(Length::Fill)
                        .center_x(Length::Fill),
                    ]
                    .spacing(6),
                )
                .width(Length::Fill)
                .padding(8)
                .style(chat_area_style),
            );
        }

        Some(stack.into())
    }

}
