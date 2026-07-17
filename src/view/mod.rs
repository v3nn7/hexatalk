//! The GUI: everything that turns `App` state into an iced `Element`.
//! Split by screen so each file stays a manageable size; `mod.rs` itself
//! only holds the top-level `view()` dispatcher, the update-available
//! banner, and the toast/lightbox overlay logic shared by every screen.

mod auth;
mod chat;
mod profile;
mod server_settings;
mod settings;

use iced::widget::{button, column, container, horizontal_space, image, row, stack, text};
use iced::{Background, Border, Color, ContentFit, Element, Length, Theme};

use crate::style::*;
use crate::{App, Message};

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let content = match &self.session {
            None => self.view_auth(),
            Some(_) if self.viewing_profile.is_some() || self.profile_error.is_some() => {
                self.view_profile()
            }
            Some(session) if self.settings_open => self.view_settings(session),
            Some(_) if self.server_settings_open && self.selected_server.is_some() => {
                self.view_server_settings(self.selected_server.as_ref().unwrap())
            }
            Some(session) => self.view_chat(session),
        };

        let mut stack = column![];
        if let Some((msg, _)) = &self.toast {
            stack = stack.push(
                container(
                    row![
                        text(msg.clone()).size(12).style(|_theme: &Theme| text::Style {
                            color: Some(c_accent()),
                        }),
                        horizontal_space(),
                        button(text("×").size(14))
                            .on_press(Message::ClearToast)
                            .padding([2, 8])
                            .style(link_button_style),
                    ]
                    .align_y(iced::Alignment::Center)
                    .padding([8, 14]),
                )
                .width(Length::Fill)
                .style(|_theme: &Theme| container::Style {
                    background: Some(Background::Color(c_bg_elevated())),
                    border: Border {
                        color: c_border_strong(),
                        width: 1.0,
                        radius: r0(),
                    },
                    ..Default::default()
                }),
            );
        }
        stack = stack.push(content);

        // Fullscreen attachment lightbox (tap image in chat to open).
        if let Some(url) = &self.attachment_preview_url {
            let preview: Element<'_, Message> =
                if let Some(handle) = self.avatar_image_cache.get(url).cloned() {
                    container(
                        image(handle)
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .content_fit(ContentFit::Contain),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(24)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .into()
                } else {
                    container(
                        text("Image not loaded yet").size(14).style(|_theme: &Theme| {
                            text::Style {
                                color: Some(c_text_muted()),
                            }
                        }),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .into()
                };

            let lightbox = container(
                column![
                    row![
                        text("IMAGE").size(12).style(|_theme: &Theme| text::Style {
                            color: Some(c_accent()),
                        }),
                        horizontal_space(),
                        button(text("Close  Esc").size(12))
                            .on_press(Message::CloseAttachmentPreview)
                            .padding([6, 12])
                            .style(secondary_button_style),
                    ]
                    .align_y(iced::Alignment::Center)
                    .padding([10, 14]),
                    container(preview)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .style(|_theme: &Theme| container::Style {
                            background: Some(Background::Color(Color::from_rgba(
                                0.02, 0.04, 0.03, 0.97,
                            ))),
                            ..Default::default()
                        }),
                ]
                .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.92))),
                ..Default::default()
            });

            // Overlay on top of the whole app.
            return stack![stack, lightbox].into();
        }

        stack.into()
    }
}
