//! The login/register screen.

use iced::widget::{button, column, container, row, text, text_input, Space};
use iced::{Background, Border, Color, Element, Length, Theme};

use crate::style::*;
use crate::*;

impl App {
    pub(crate) fn view_auth(&self) -> Element<'_, Message> {
        let heading = if self.auth_mode == AuthMode::Login {
            "Welcome back"
        } else {
            "Create an account"
        };
        let subheading = if self.auth_mode == AuthMode::Login {
            "Sign in to continue your conversations."
        } else {
            "Join Talkyss — private chats, servers, and calls."
        };

        let brand_mark = container(
            text("T")
                .size(22)
                .font(mono())
                .style(|_theme: &Theme| text::Style {
                    color: Some(Color::from_rgb(0.02, 0.05, 0.02)),
                }),
        )
        .width(Length::Fixed(48.0))
        .height(Length::Fixed(48.0))
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

        let brand = column![
            brand_mark,
            text("Talkyss").size(28).font(mono()).style(|_theme: &Theme| text::Style {
                color: Some(c_text_primary()),
            }),
        ]
        .spacing(12)
        .align_x(iced::Alignment::Center);

        let title = text(heading).size(20).style(|_theme: &Theme| text::Style {
            color: Some(c_text_primary()),
        });
        let subtitle = text(subheading).size(13).style(|_theme: &Theme| text::Style {
            color: Some(c_text_muted()),
        });

        let username_field = text_input("Username", &self.username_input)
            .on_input(Message::UsernameInputChanged)
            .padding(14)
            .width(Length::Fill)
            .style(pill_input_style);

        let password_field = text_input("Password", &self.password_input)
            .on_input(Message::PasswordInputChanged)
            .on_submit(Message::SubmitAuth)
            .secure(true)
            .padding(14)
            .width(Length::Fill)
            .style(pill_input_style);

        let mut form = column![
            brand,
            Space::with_height(Length::Fixed(8.0)),
            title,
            subtitle,
            Space::with_height(Length::Fixed(4.0)),
            field_label("Username"),
            username_field,
        ]
        .spacing(10)
        .width(Length::Fixed(360.0))
        .align_x(iced::Alignment::Center);

        if self.auth_mode == AuthMode::Register {
            let display_name_field =
                text_input("Display name (optional)", &self.display_name_input)
                    .on_input(Message::DisplayNameInputChanged)
                    .padding(14)
                    .width(Length::Fill)
                    .style(pill_input_style);
            form = form.push(field_label("Display name"));
            form = form.push(display_name_field);
        }

        form = form.push(field_label("Password"));
        form = form.push(password_field);

        if let Some(error) = &self.auth_error {
            form = form.push(
                container(text(error).size(13))
                    .padding(12)
                    .width(Length::Fill)
                    .style(error_box_style),
            );
        }

        let submit_label = if self.auth_busy {
            "Please wait..."
        } else if self.auth_mode == AuthMode::Login {
            "Log in"
        } else {
            "Create account"
        };
        let submit_button = button(
            container(text(submit_label).size(15))
                .center_x(Length::Fill),
        )
        .on_press_maybe((!self.auth_busy).then_some(Message::SubmitAuth))
        .padding(14)
        .width(Length::Fill)
        .style(accent_button_style);
        form = form.push(Space::with_height(Length::Fixed(6.0)));
        form = form.push(submit_button);

        let (switch_prompt, switch_label, switch_mode) = if self.auth_mode == AuthMode::Login {
            ("Need an account?", "Sign up", AuthMode::Register)
        } else {
            ("Already have an account?", "Log in", AuthMode::Login)
        };
        let switch = row![
            text(switch_prompt).size(12).style(|_theme: &Theme| text::Style {
                color: Some(c_text_muted())
            }),
            button(text(switch_label).size(12))
                .on_press(Message::SwitchAuthMode(switch_mode))
                .padding([4, 8])
                .style(link_button_style),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        form = form.push(Space::with_height(Length::Fixed(4.0)));
        form = form.push(switch);
        form = form.push(
            text(&self.connect_status)
                .size(10)
                .font(mono())
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
        );
        form = form.push(
            text(format!("v{CURRENT_APP_VERSION} · E2EE · P2P CALLS"))
                .size(10)
                .font(mono())
                .style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
        );

        let card = container(form).padding(36).style(auth_card_style);

        container(card)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .padding(24)
            .style(auth_shell_style)
            .into()
    }

}
