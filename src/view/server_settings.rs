//! The per-server Settings screen (Overview / Channels / Members / Invites /
//! Danger Zone) opened from a server's own settings button.

use iced::widget::{
    button, checkbox, column, container, horizontal_space, image, row, text, text_input, Space,
};
use iced::{Background, Border, Color, ContentFit, Element, Length, Theme};

use crate::style::*;
use crate::*;

impl App {
    pub(crate) fn view_server_settings<'a>(&'a self, server: &'a ServerSummary) -> Element<'a, Message> {
        let icon: Element<'_, Message> =
            if let Some(handle) = self.avatar_image_cache.get(&server.icon_url) {
                container(
                    image(handle.clone())
                        .width(Length::Fixed(36.0))
                        .height(Length::Fixed(36.0))
                        .content_fit(ContentFit::Cover),
                )
                .width(Length::Fixed(36.0))
                .height(Length::Fixed(36.0))
                .into()
            } else {
                let initial = server
                    .name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "#".into());
                container(text(initial).size(15))
                    .width(Length::Fixed(36.0))
                    .height(Length::Fixed(36.0))
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Center)
                    .style(|_theme: &Theme| container::Style {
                        background: Some(Background::Color(c_bg_elevated())),
                        text_color: Some(c_accent()),
                        border: Border {
                            radius: r0(),
                            width: 1.0,
                            color: c_border_strong(),
                        },
                        ..Default::default()
                    })
                    .into()
            };

        let header = row![
            button(text("← Back").size(13))
                .on_press(Message::ToggleServerSettings)
                .padding([8, 14])
                .style(secondary_button_style),
            icon,
            column![
                text(format!("{} · Settings", server.name))
                    .size(18)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_text_primary()),
                    }),
                muted_text(
                    format!(
                        "{} channels · {} members{}",
                        self.channels.len(),
                        self.server_members.len(),
                        if server.custom_slug.is_empty() {
                            String::new()
                        } else {
                            format!(" · /{}", server.custom_slug)
                        }
                    ),
                    11,
                ),
            ]
            .spacing(2),
            horizontal_space(),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .padding(16);

        let categories: [(ServerSettingsCategory, &str, &str); 6] = [
            (
                ServerSettingsCategory::Overview,
                "Overview",
                "Name, icon, vanity URL",
            ),
            (
                ServerSettingsCategory::Channels,
                "Channels",
                "Create, rename, delete",
            ),
            (
                ServerSettingsCategory::Members,
                "Members",
                "Assign roles, kick",
            ),
            (
                ServerSettingsCategory::Roles,
                "Roles",
                "Create roles, edit permissions",
            ),
            (
                ServerSettingsCategory::Invites,
                "Invites",
                "Share join codes",
            ),
            (
                ServerSettingsCategory::Danger,
                "Danger Zone",
                "Irreversible actions",
            ),
        ];
        let mut sidebar = column![
            text("SERVER SETTINGS")
                .size(11)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
            Space::with_height(Length::Fixed(6.0)),
        ]
        .spacing(2);
        for (category, label, hint) in categories {
            let is_active = self.server_settings_category == category;
            let is_danger = category == ServerSettingsCategory::Danger;
            sidebar = sidebar.push(
                button(
                    column![
                        text(label).size(13).style(move |_theme: &Theme| text::Style {
                            color: Some(if is_danger {
                                c_danger()
                            } else {
                                c_text_primary()
                            }),
                        }),
                        text(hint).size(10).style(|_theme: &Theme| text::Style {
                            color: Some(c_text_muted()),
                        }),
                    ]
                    .spacing(2),
                )
                .on_press(Message::ServerSettingsCategoryChanged(category))
                .width(Length::Fill)
                .padding([12, 12])
                .style(move |theme: &Theme, status| {
                    sidebar_item_style(theme, status, is_active)
                }),
            );
        }

        let content = match self.server_settings_category {
            ServerSettingsCategory::Overview => self.view_server_settings_overview(),
            ServerSettingsCategory::Channels => self.view_server_settings_channels(),
            ServerSettingsCategory::Members => self.view_server_settings_members(),
            ServerSettingsCategory::Roles => self.view_server_settings_roles(),
            ServerSettingsCategory::Invites => self.view_server_settings_invites(server),
            ServerSettingsCategory::Danger => self.view_server_settings_danger(server),
        };

        let body = row![
            container(sidebar)
                .width(Length::Fixed(240.0))
                .height(Length::Fill)
                .padding(16)
                .style(sidebar_style),
            settings_pane(content),
        ]
        .height(Length::Fill);

        column![container(header).style(chat_header_style), body]
            .height(Length::Fill)
            .into()
    }

    pub(crate) fn view_server_settings_overview(&self) -> iced::widget::Column<'_, Message> {
        let is_owner = self.selected_server.as_ref().is_some_and(|s| s.is_owner);
        let is_platform_admin = self.session.as_ref().is_some_and(|s| s.is_admin);
        let has_icon = self
            .selected_server
            .as_ref()
            .is_some_and(|s| !s.icon_url.is_empty());
        let icon_url = self
            .selected_server
            .as_ref()
            .map(|s| s.icon_url.as_str())
            .unwrap_or("");

        let mut col = column![].spacing(20).width(Length::Fill);

        let icon_preview: Element<'_, Message> =
            if let Some(handle) = self.avatar_image_cache.get(icon_url) {
                container(
                    image(handle.clone())
                        .width(Length::Fixed(80.0))
                        .height(Length::Fixed(80.0))
                        .content_fit(ContentFit::Cover),
                )
                .width(Length::Fixed(80.0))
                .height(Length::Fixed(80.0))
                .style(|_theme: &Theme| container::Style {
                    border: Border {
                        radius: r0(),
                        width: 1.0,
                        color: c_border_strong(),
                    },
                    ..Default::default()
                })
                .into()
            } else {
                let name = self
                    .selected_server
                    .as_ref()
                    .map(|s| s.name.as_str())
                    .unwrap_or("?");
                let initial = name
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".into());
                let label = if has_icon && !self.server_icon_busy {
                    "…" // URL known, image still loading
                } else {
                    initial.as_str()
                };
                container(text(label.to_string()).size(26))
                    .width(Length::Fixed(80.0))
                    .height(Length::Fixed(80.0))
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Center)
                    .style(|_theme: &Theme| container::Style {
                        background: Some(Background::Color(c_accent_soft())),
                        text_color: Some(c_accent()),
                        border: Border {
                            radius: r0(),
                            width: 1.0,
                            color: c_border_strong(),
                        },
                        ..Default::default()
                    })
                    .into()
            };

        let mut icon_actions = column![].spacing(6);
        if is_owner {
            icon_actions = icon_actions.push(
                button(
                    text(if self.server_icon_busy {
                        "Uploading…"
                    } else {
                        "Upload icon"
                    })
                    .size(12),
                )
                .on_press_maybe((!self.server_icon_busy).then_some(Message::PickServerIcon))
                .padding([10, 14])
                .style(secondary_button_style),
            );
            if has_icon {
                icon_actions = icon_actions.push(
                    button(text("Remove icon").size(12))
                        .on_press_maybe((!self.server_icon_busy).then_some(Message::RemoveServerIcon))
                        .padding([6, 10])
                        .style(link_button_style),
                );
            }
            icon_actions = icon_actions.push(muted_text(
                "PNG or JPG · under 2MB · square looks best",
                10,
            ));
        } else {
            icon_actions = icon_actions.push(muted_text(
                "Only the server owner can change the icon.",
                11,
            ));
        }

        let mut identity = column![
            section_title("Server identity"),
            muted_text(
                "Icon and display name shown in the rail and member lists.",
                12,
            ),
            row![icon_preview, icon_actions]
                .spacing(16)
                .align_y(iced::Alignment::Center),
            field_label("Server name"),
        ]
        .spacing(12)
        .width(Length::Fill);

        if is_owner {
            identity = identity.push(
                row![
                    text_input("Server name", &self.rename_server_input)
                        .on_input(Message::RenameServerInputChanged)
                        .on_submit(Message::RenameServer)
                        .padding(12)
                        .width(Length::Fill)
                        .style(pill_input_style),
                    button(text("Save").size(13))
                        .on_press(Message::RenameServer)
                        .padding([12, 16])
                        .style(accent_button_style),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        } else {
            identity = identity.push(
                text(
                    self.selected_server
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                )
                .size(16)
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_primary()),
                }),
            );
            identity = identity.push(muted_text(
                "Only the server owner can rename this server.",
                11,
            ));
        }

        if let Some(status) = &self.server_status {
            let danger = status.to_lowercase().contains("only the")
                || status.to_lowercase().contains("failed")
                || status.to_lowercase().contains("error")
                || status.to_lowercase().contains("must");
            identity = identity.push(status_banner(status, danger));
        }
        col = col.push(settings_card(identity));

        if is_platform_admin {
            col = col.push(settings_card(
                column![
                    section_title("Custom URL"),
                    muted_text(
                        "Platform admin only. Vanity path for this server.",
                        12,
                    ),
                    row![
                        text("talkyss.app/").size(13).style(|_theme: &Theme| {
                            text::Style {
                                color: Some(c_text_muted()),
                            }
                        }),
                        text_input("slug", &self.custom_slug_input)
                            .on_input(Message::CustomSlugInputChanged)
                            .on_submit(Message::SaveCustomSlug)
                            .padding(12)
                            .width(Length::Fill)
                            .style(pill_input_style),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                    row![
                        button(text("Save URL").size(12))
                            .on_press(Message::SaveCustomSlug)
                            .padding([10, 14])
                            .style(accent_button_style),
                        button(text("Clear").size(12))
                            .on_press(Message::ClearCustomSlug)
                            .padding([10, 14])
                            .style(secondary_button_style),
                    ]
                    .spacing(8),
                ]
                .spacing(12)
                .width(Length::Fill),
            ));
        } else if self
            .selected_server
            .as_ref()
            .is_some_and(|s| !s.custom_slug.is_empty())
        {
            col = col.push(settings_card(
                column![
                    section_title("Custom URL"),
                    text(format!(
                        "/{}",
                        self.selected_server.as_ref().unwrap().custom_slug
                    ))
                    .size(16)
                    .style(|_theme: &Theme| text::Style {
                        color: Some(c_accent()),
                    }),
                    muted_text("Set by Talkyss administration.", 11),
                ]
                .spacing(8)
                .width(Length::Fill),
            ));
        }

        col
    }

    pub(crate) fn view_server_settings_channels(&self) -> iced::widget::Column<'_, Message> {
        let mut col = column![].spacing(20).width(Length::Fill);
        let can_manage_channels = self.my_server_permissions & PERM_MANAGE_CHANNELS != 0;

        let mut create = column![
            section_title("Channels"),
            muted_text(
                "Text for chat, voice for hangouts. At least one channel is required.",
                12,
            ),
        ]
        .spacing(12)
        .width(Length::Fill);

        if can_manage_channels {
            create = create.push(field_label("Create channel"));
            create = create.push(
                row![
                    text_input("channel-name", &self.new_channel_name_input)
                        .on_input(Message::NewChannelNameChanged)
                        .on_submit(Message::CreateChannel)
                        .padding(12)
                        .width(Length::Fill)
                        .style(pill_input_style),
                    button(
                        text(if self.new_channel_is_voice {
                            "Voice"
                        } else {
                            "Text"
                        })
                        .size(12),
                    )
                    .on_press(Message::NewChannelIsVoice(!self.new_channel_is_voice))
                    .padding([12, 14])
                    .style(secondary_button_style),
                    button(text("Create").size(12))
                        .on_press(Message::CreateChannel)
                        .padding([12, 16])
                        .style(accent_button_style),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        } else {
            create = create.push(muted_text(
                "Channels are managed by members with the Manage Channels permission.",
                12,
            ));
        }

        if let Some(status) = &self.server_status {
            create = create.push(status_banner(status, false));
        }
        col = col.push(settings_card(create));

        let mut list = column![field_label_owned(format!(
            "All channels ({})",
            self.channels.len()
        ))]
        .spacing(8)
        .width(Length::Fill);

        if self.channels.is_empty() {
            list = list.push(muted_text("No channels yet — create one above.", 13));
        } else {
            let can_delete = self.channels.len() > 1;
            for channel in &self.channels {
                let is_voice = channel.channel_type == "voice";
                let is_renaming = self.renaming_channel_id.as_deref()
                    == Some(channel.conversation_id.as_str());
                let row_content: Element<'_, Message> = if is_renaming && can_manage_channels {
                    row![
                        text_input("channel-name", &self.rename_channel_input)
                            .on_input(Message::RenameChannelInputChanged)
                            .on_submit(Message::RenameChannel)
                            .padding(10)
                            .width(Length::Fill)
                            .style(pill_input_style),
                        button(text("Save").size(11))
                            .on_press(Message::RenameChannel)
                            .padding([8, 12])
                            .style(accent_button_style),
                        button(text("Cancel").size(11))
                            .on_press(Message::CancelRenameChannel)
                            .padding([8, 12])
                            .style(secondary_button_style),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into()
                } else {
                    let mut item = row![
                        text(if is_voice { "V" } else { "#" }).size(14).style(
                            |_theme: &Theme| text::Style {
                                color: Some(c_text_muted()),
                            }
                        ),
                        column![
                            text(channel.name.clone()).size(14).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(c_text_primary()),
                                }
                            }),
                            muted_text(
                                if is_voice {
                                    "Voice channel"
                                } else {
                                    "Text channel"
                                },
                                11,
                            ),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                    ]
                    .spacing(10)
                    .align_y(iced::Alignment::Center);
                    if can_manage_channels {
                        item = item.push(
                            button(text("Rename").size(11))
                                .on_press(Message::StartRenameChannel(
                                    channel.conversation_id.clone(),
                                    channel.name.clone(),
                                ))
                                .padding([8, 12])
                                .style(secondary_button_style),
                        );
                        if can_delete {
                            item = item.push(
                                button(text("Delete").size(11))
                                    .on_press(Message::DeleteChannel(channel.conversation_id.clone()))
                                    .padding([8, 12])
                                    .style(danger_button_style),
                            );
                        }
                    }
                    item.into()
                };
                list = list.push(container(row_content).padding(12).width(Length::Fill).style(panel_box_style));
            }
        }

        col = col.push(settings_card(list));
        col
    }

    pub(crate) fn view_server_settings_members(&self) -> iced::widget::Column<'_, Message> {
        let mut col = column![].spacing(20).width(Length::Fill);

        let can_manage_roles = self.my_server_permissions & PERM_MANAGE_ROLES != 0;
        let can_kick = self.my_server_permissions & PERM_KICK_MEMBERS != 0;

        let members_hint = if can_manage_roles || can_kick {
            "Add roles from the flyout — click one to grant it, click again to revoke. Kick removes the user from this server."
        } else {
            "Roles are managed by members with the Manage Roles permission."
        };
        let mut members_card = column![
            section_title_owned(format!("Members ({})", self.server_members.len())),
            muted_text(members_hint, 12),
        ]
        .spacing(12)
        .width(Length::Fill);

        if self.server_members.is_empty() {
            members_card = members_card.push(muted_text("Loading members…", 13));
        } else {
            for member in &self.server_members {
                let photo = self.avatar_image_cache.get(&member.avatar_image_url).cloned();
                let role_label = if member.is_owner {
                    "Owner".to_string()
                } else if member.roles.is_empty() {
                    "Member".to_string()
                } else {
                    member
                        .roles
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };

                let mut item = column![
                    row![
                        avatar(&member.display_name, None, &member.avatar_color, photo),
                        column![
                            text(member.display_name.clone()).size(14).style(
                                |_theme: &Theme| text::Style {
                                    color: Some(c_text_primary()),
                                }
                            ),
                            muted_text(format!("@{} · {}", member.username, role_label), 11),
                        ]
                        .spacing(2)
                        .width(Length::Fill),
                        if member.is_owner {
                            container(text("OWNER").size(10).style(|_theme: &Theme| {
                                text::Style {
                                    color: Some(Color::from_rgb(0.05, 0.04, 0.0)),
                                }
                            }))
                            .padding([4, 8])
                            .style(|_theme: &Theme| container::Style {
                                background: Some(Background::Color(Color::from_rgb(
                                    0.88, 0.70, 0.28,
                                ))),
                                border: Border {
                                    radius: r0(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            })
                        } else {
                            container(Space::new(Length::Fixed(0.0), Length::Fixed(0.0)))
                        },
                    ]
                    .spacing(10)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(8);

                if !member.is_owner && (can_manage_roles || can_kick) {
                    let mut actions_row = row![].spacing(6);
                    let is_open =
                        self.member_role_picker_open.as_deref() == Some(member.user_id.as_str());
                    if can_manage_roles {
                        actions_row = actions_row.push(
                            button(text(if is_open { "Close" } else { "+ Add role" }).size(11))
                                .on_press(Message::ToggleMemberRolePicker(member.user_id.clone()))
                                .padding([6, 10])
                                .style(if is_open {
                                    accent_button_style
                                } else {
                                    secondary_button_style
                                }),
                        );
                    }
                    if can_kick {
                        actions_row = actions_row.push(
                            button(text("Kick").size(11))
                                .on_press(Message::KickMember(member.user_id.clone()))
                                .padding([6, 10])
                                .style(danger_button_style),
                        );
                    }
                    item = item.push(actions_row);

                    if can_manage_roles && is_open {
                        item = item.push(self.member_role_picker_flyout(member));
                    }
                }

                members_card =
                    members_card.push(container(item).padding(12).width(Length::Fill).style(panel_box_style));
            }
        }

        col = col.push(settings_card(members_card));
        col
    }

    /// A slide-out panel of every assignable (non-default) role for one
    /// member — click a role to grant it, click again to revoke. A member
    /// can hold any number of these at once, on top of the implicit
    /// @everyone baseline (see convex/roles.ts).
    pub(crate) fn member_role_picker_flyout<'a>(
        &'a self,
        member: &'a ServerMemberRow,
    ) -> Element<'a, Message> {
        let assignable: Vec<_> = self.server_roles.iter().filter(|r| r.position != 0).collect();
        let mut picker = column![muted_text("Click to grant, click again to revoke:", 11)]
            .spacing(6)
            .width(Length::Fill);

        if assignable.is_empty() {
            picker = picker.push(muted_text(
                "No custom roles yet — create one in the Roles tab.",
                11,
            ));
        } else {
            let mut role_row = row![].spacing(6);
            for role in assignable {
                let assigned = member.roles.iter().any(|t| t.role_id == role.role_id);
                let uid = member.user_id.clone();
                let rid = role.role_id.clone();
                role_row = role_row.push(
                    button(text(format!("{} {}", if assigned { "✓" } else { "+" }, role.name)).size(11))
                        .on_press(Message::ToggleMemberRole(uid, rid))
                        .padding([6, 10])
                        .style(if assigned {
                            accent_button_style
                        } else {
                            secondary_button_style
                        }),
                );
            }
            picker = picker.push(role_row);
        }

        container(picker)
            .padding(10)
            .width(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(c_bg_elevated())),
                border: Border {
                    radius: r0(),
                    width: 1.0,
                    color: c_border(),
                },
                ..Default::default()
            })
            .into()
    }

    pub(crate) fn view_server_settings_roles(&self) -> iced::widget::Column<'_, Message> {
        let mut col = column![].spacing(20).width(Length::Fill);
        let can_manage_roles = self.my_server_permissions & PERM_MANAGE_ROLES != 0;

        let mut roles_card = column![
            section_title("Roles"),
            muted_text(
                if can_manage_roles {
                    "Create roles, then click one to edit its name, color, and permissions."
                } else {
                    "Roles are managed by members with the Manage Roles permission."
                },
                12,
            ),
        ]
        .spacing(12)
        .width(Length::Fill);

        if can_manage_roles {
            roles_card = roles_card.push(
                row![
                    text_input("New role name", &self.new_role_name_input)
                        .on_input(Message::NewRoleNameChanged)
                        .on_submit(Message::CreateRole)
                        .padding(12)
                        .width(Length::Fill)
                        .style(pill_input_style),
                    button(text("Add role").size(12))
                        .on_press(Message::CreateRole)
                        .padding([12, 16])
                        .style(accent_button_style),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }

        if self.server_roles.is_empty() {
            roles_card = roles_card.push(muted_text("No custom roles yet.", 12));
        } else {
            let mut list = column![].spacing(6);
            for role in &self.server_roles {
                let swatch_color = parse_hex_color(&role.color).unwrap_or_else(c_accent);
                let is_editing = self.editing_role_id.as_deref() == Some(role.role_id.as_str());

                let mut row_el = row![
                    container(Space::new(Length::Fixed(12.0), Length::Fixed(12.0))).style(
                        move |_theme: &Theme| container::Style {
                            background: Some(Background::Color(swatch_color)),
                            border: Border {
                                radius: r0(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    ),
                    text(role.name.clone()).size(13).style(|_theme: &Theme| text::Style {
                        color: Some(c_text_primary()),
                    }),
                    horizontal_space(),
                ]
                .spacing(10)
                .align_y(iced::Alignment::Center);

                if can_manage_roles {
                    let role_id = role.role_id.clone();
                    row_el = row_el.push(
                        button(text(if is_editing { "Close" } else { "Edit" }).size(11))
                            .on_press(if is_editing {
                                Message::CloseRoleEditor
                            } else {
                                Message::SelectRoleForEdit(role_id)
                            })
                            .padding([6, 10])
                            .style(if is_editing {
                                accent_button_style
                            } else {
                                secondary_button_style
                            }),
                    );
                }

                list = list.push(
                    container(row_el)
                        .padding(10)
                        .width(Length::Fill)
                        .style(panel_box_style),
                );

                if is_editing {
                    list = list.push(self.role_editor_panel(role));
                }
            }
            roles_card = roles_card.push(list);
        }

        if let Some(status) = &self.server_status {
            roles_card = roles_card.push(status_banner(status, false));
        }
        col = col.push(settings_card(roles_card));
        col
    }

    fn role_editor_panel<'a>(&'a self, role: &'a ServerRoleRow) -> Element<'a, Message> {
        let role_id = role.role_id.clone();

        let mut panel = column![
            field_label("Rename"),
            row![
                text_input("Role name", &self.role_name_edit_input)
                    .on_input(Message::RoleNameEditChanged)
                    .on_submit(Message::SaveRoleName)
                    .padding(10)
                    .width(Length::Fill)
                    .style(pill_input_style),
                button(text("Save").size(12))
                    .on_press(Message::SaveRoleName)
                    .padding([10, 14])
                    .style(accent_button_style),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            field_label("Color"),
        ]
        .spacing(10)
        .width(Length::Fill);

        let mut color_row = row![].spacing(8);
        for hex in ROLE_COLOR_PALETTE {
            let selected = role.color.eq_ignore_ascii_case(hex);
            let rid = role_id.clone();
            let swatch = button(Space::new(Length::Fixed(24.0), Length::Fixed(24.0)))
                .on_press(Message::SetRoleColor(rid, hex.to_string()))
                .style(move |_theme: &Theme, _status| button::Style {
                    background: parse_hex_color(hex).map(Background::Color),
                    border: Border {
                        radius: r0(),
                        width: if selected { 2.0 } else { 1.0 },
                        color: if selected { c_accent() } else { c_border() },
                    },
                    ..Default::default()
                });
            color_row = color_row.push(swatch);
        }
        panel = panel.push(color_row);

        panel = panel.push(field_label("Permissions"));
        let perms: [(u32, &str); 8] = [
            (PERM_VIEW_CHANNELS, "View channels"),
            (PERM_SEND_MESSAGES, "Send messages"),
            (PERM_MANAGE_CHANNELS, "Manage channels"),
            (PERM_KICK_MEMBERS, "Kick members"),
            (PERM_MANAGE_ROLES, "Manage roles"),
            (PERM_MANAGE_SERVER, "Manage server"),
            (PERM_CONNECT_VOICE, "Connect to voice"),
            (PERM_SPEAK, "Speak"),
        ];
        let mut perm_col = column![].spacing(6);
        for (bit, label) in perms {
            let rid = role_id.clone();
            perm_col = perm_col.push(
                checkbox(label, role.permissions & bit != 0)
                    .on_toggle(move |_| Message::ToggleRolePermission(rid.clone(), bit))
                    .text_size(12),
            );
        }
        panel = panel.push(perm_col);

        let is_default_role = role.position == 0;
        if self.confirm_delete_role_id.as_deref() == Some(role_id.as_str()) {
            panel = panel.push(
                row![
                    muted_text("Delete this role? This can't be undone.", 12),
                    button(text("Yes, delete").size(12))
                        .on_press(Message::DeleteRole(role_id.clone()))
                        .padding([8, 12])
                        .style(danger_button_style),
                    button(text("Cancel").size(12))
                        .on_press(Message::CancelDeleteRole)
                        .padding([8, 12])
                        .style(secondary_button_style),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        } else if !is_default_role {
            panel = panel.push(
                button(text("Delete role").size(12))
                    .on_press(Message::ConfirmDeleteRole(role_id.clone()))
                    .padding([8, 12])
                    .style(danger_button_style),
            );
        } else {
            panel = panel.push(muted_text("The default role can't be deleted.", 11));
        }

        container(panel)
            .padding(14)
            .width(Length::Fill)
            .style(panel_box_style)
            .into()
    }

    pub(crate) fn view_server_settings_invites<'a>(&'a self, server: &'a ServerSummary) -> iced::widget::Column<'a, Message> {
        let mut col = column![].spacing(20).width(Length::Fill);

        let mut card = column![
            section_title("Invite people"),
            muted_text(
                "Share this code. Anyone with it can join. Regenerating invalidates the old code.",
                12,
            ),
        ]
        .spacing(12)
        .width(Length::Fill);

        if server.invite_code.is_empty() {
            card = card.push(muted_text("Loading invite code…", 13));
        } else {
            let mut code_col = column![
                field_label("Invite code"),
                text(server.invite_code.clone()).size(28).font(mono()).style(
                    |_theme: &Theme| text::Style {
                        color: Some(c_accent()),
                    }
                ),
            ]
            .spacing(12)
            .align_x(iced::Alignment::Center);

            let mut actions = row![
                button(text("Copy code").size(13))
                    .on_press(Message::CopyInviteCode(server.invite_code.clone()))
                    .padding([12, 16])
                    .style(accent_button_style),
                button(text("Copy link").size(13))
                    .on_press(Message::CopyInviteLink(server.invite_code.clone()))
                    .padding([12, 16])
                    .style(secondary_button_style),
            ]
            .spacing(10);
            if server.is_owner {
                actions = actions.push(
                    button(text("Regenerate").size(13))
                        .on_press(Message::RegenerateInviteCode)
                        .padding([12, 16])
                        .style(secondary_button_style),
                );
            }
            code_col = code_col.push(actions);

            card = card.push(
                container(code_col)
                    .width(Length::Fill)
                    .padding(24)
                    .style(panel_box_style),
            );
        }

        if let Some(status) = &self.server_status {
            card = card.push(status_banner(status, false));
        }

        col = col.push(settings_card(card));
        col
    }

    pub(crate) fn view_server_settings_danger<'a>(&'a self, server: &'a ServerSummary) -> iced::widget::Column<'a, Message> {
        let mut col = column![].spacing(20).width(Length::Fill);

        let mut card = column![
            text("Danger Zone").size(16).style(|_theme: &Theme| text::Style {
                color: Some(c_danger()),
            }),
            muted_text(
                "Deleting a server permanently removes all channels, messages, roles and memberships. This cannot be undone.",
                12,
            ),
        ]
        .spacing(12)
        .width(Length::Fill);

        if !server.is_owner {
            card = card.push(muted_text(
                "Only the server owner can delete this server.",
                12,
            ));
        } else if self.confirm_delete_server {
            card = card.push(
                container(
                    column![
                        text("Are you sure you want to delete this server?")
                            .size(13)
                            .style(|_theme: &Theme| text::Style {
                                color: Some(c_danger()),
                            }),
                        muted_text("This action is permanent.", 11),
                        row![
                            button(text("Yes, delete forever").size(13))
                                .on_press(Message::DeleteServer)
                                .padding([12, 16])
                                .style(danger_button_style),
                            button(text("Cancel").size(13))
                                .on_press(Message::ToggleConfirmDeleteServer)
                                .padding([12, 16])
                                .style(secondary_button_style),
                        ]
                        .spacing(10),
                    ]
                    .spacing(12),
                )
                .padding(16)
                .style(error_box_style),
            );
        } else {
            card = card.push(
                button(text("Delete server").size(13))
                    .on_press(Message::ToggleConfirmDeleteServer)
                    .padding([12, 16])
                    .style(danger_button_style),
            );
        }

        if let Some(status) = &self.server_status {
            card = card.push(status_banner(status, true));
        }

        col = col.push(
            container(card)
                .padding(22)
                .width(Length::Fill)
                .style(error_box_style),
        );
        col
    }

}
