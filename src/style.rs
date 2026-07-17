//! Colors, container/button/text styles, and small reusable view-building
//! helpers (avatar, badges, section titles, ...) shared across every screen
//! in `view/`. Pure functions only -- nothing here touches `App` state.

use std::collections::BTreeMap;

use chrono::{Local, TimeZone};
use convex::Value;
use iced::widget::{button, column, container, image, mouse_area, row, scrollable, stack, text, text_input, Space};
use iced::{
    Background, Border, Color, ContentFit, Element, Length, Shadow, Theme, Vector,
};

use crate::{Message, ResizePanel, SidebarTab, ONLINE_THRESHOLD_MS};

// ---------- Colors (muted emerald, low-neon dark theme) ----------

pub(crate) fn c_bg_primary() -> Color {
    // Main canvas — near-black, faint cool cast (no green glow)
    Color::from_rgb(0.045, 0.052, 0.050)
}

pub(crate) fn c_bg_secondary() -> Color {
    Color::from_rgb(0.034, 0.040, 0.038)
}

pub(crate) fn c_bg_tertiary() -> Color {
    Color::from_rgb(0.020, 0.026, 0.024)
}

pub(crate) fn c_bg_elevated() -> Color {
    Color::from_rgb(0.060, 0.072, 0.068)
}

pub(crate) fn c_bg_hover() -> Color {
    Color::from_rgb(0.078, 0.092, 0.086)
}

pub(crate) fn c_accent() -> Color {
    // Muted emerald — calm, not neon
    Color::from_rgb(0.30, 0.72, 0.52)
}

pub(crate) fn c_accent_dim() -> Color {
    Color::from_rgb(0.24, 0.58, 0.42)
}

pub(crate) fn c_accent_soft() -> Color {
    Color::from_rgba(0.30, 0.72, 0.52, 0.14)
}

pub(crate) fn c_text_primary() -> Color {
    Color::from_rgb(0.74, 0.82, 0.77)
}

pub(crate) fn c_text_muted() -> Color {
    Color::from_rgb(0.50, 0.58, 0.53)
}

pub(crate) fn c_border() -> Color {
    Color::from_rgba(0.30, 0.72, 0.52, 0.18)
}

pub(crate) fn c_border_strong() -> Color {
    Color::from_rgba(0.30, 0.72, 0.52, 0.35)
}

pub(crate) fn c_online() -> Color {
    c_accent()
}

pub(crate) fn c_offline() -> Color {
    Color::from_rgb(0.30, 0.38, 0.30)
}

pub(crate) fn c_danger() -> Color {
    Color::from_rgb(0.85, 0.32, 0.34)
}

pub(crate) fn c_danger_soft() -> Color {
    Color::from_rgba(0.85, 0.32, 0.34, 0.14)
}

pub(crate) fn c_warning() -> Color {
    Color::from_rgb(0.82, 0.74, 0.30)
}

pub(crate) fn c_success() -> Color {
    c_accent_dim()
}

/// Sharp corners everywhere — almost no rounding.
pub(crate) fn r0() -> iced::border::Radius {
    0.0.into()
}

/// Accent face for "terminal readout" chrome: timestamps, labels, badges,
/// rail glyphs, the brand mark. Uses the bundled Roboto Medium so text still
/// Color emoji face when the OS provides one; monochrome Noto otherwise.
/// Pure-emoji widgets should use this so shaping hits the COLR glyphs.
pub(crate) fn emoji_font() -> iced::Font {
    #[cfg(windows)]
    {
        iced::Font::with_name("Segoe UI Emoji")
    }
    #[cfg(target_os = "macos")]
    {
        iced::Font::with_name("Apple Color Emoji")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        iced::Font::with_name("Noto Color Emoji")
    }
    #[cfg(not(any(windows, unix)))]
    {
        iced::Font::with_name("Noto Emoji")
    }
}

/// Segoe UI Emoji / color emoji faces have huge vertical metrics and will
/// blow up row heights unless line height is clamped absolutely.
pub(crate) fn emoji_line_height(size: f32) -> iced::widget::text::LineHeight {
    iced::widget::text::LineHeight::Absolute((size + 4.0).into())
}

/// Compact colored emoji label (reactions, icons). Always uses Advanced
/// shaping + fixed line height so COLR fonts don't wreck the layout.
pub(crate) fn emoji_label<'a>(
    content: impl iced::widget::text::IntoFragment<'a>,
    size: u16,
) -> iced::widget::Text<'a> {
    let size = size as f32;
    text(content)
        .size(size)
        .font(emoji_font())
        .line_height(emoji_line_height(size))
        .shaping(iced::widget::text::Shaping::Advanced)
}

/// Chat body line height — absolute so a single color-emoji fallback glyph
/// cannot stretch the whole message row to Segoe's giant ascent/descent.
pub(crate) fn chat_body_line_height() -> iced::widget::text::LineHeight {
    iced::widget::text::LineHeight::Absolute(20.0.into())
}

/// UI chrome uses Roboto Medium so glyphs still paint when system monospace
/// discovery fails. Message bodies stay on default Roboto Regular.
pub(crate) fn mono() -> iced::Font {
    iced::Font {
        family: iced::font::Family::Name("Roboto"),
        weight: iced::font::Weight::Medium,
        stretch: iced::font::Stretch::Normal,
        style: iced::font::Style::Normal,
    }
}

pub(crate) fn soft_shadow() -> Shadow {
    // Hard offset only — no soft blur (avoids "AI glow" look).
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.65),
        offset: Vector::new(3.0, 3.0),
        blur_radius: 0.0,
    }
}

pub(crate) fn light_shadow() -> Shadow {
    Shadow {
        color: Color::from_rgba(0.0, 0.0, 0.0, 0.50),
        offset: Vector::new(2.0, 2.0),
        blur_radius: 0.0,
    }
}

// ---------- Styling ----------

pub(crate) fn rail_button<'a>(
    glyph: &'a str,
    tab: SidebarTab,
    current: SidebarTab,
    badge: usize,
) -> Element<'a, Message> {
    rail_button_with(
        container(text(glyph).size(15).font(mono()))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center)
            .into(),
        tab,
        current,
        badge,
    )
}

/// Same rail slot as `rail_button`, but with an image glyph instead of text
/// (e.g. the Friends tab's icon).
pub(crate) fn rail_button_image<'a>(
    icon_bytes: &'static [u8],
    tab: SidebarTab,
    current: SidebarTab,
    badge: usize,
) -> Element<'a, Message> {
    rail_button_with(
        container(
            image(image::Handle::from_bytes(icon_bytes))
                .width(Length::Fixed(28.0))
                .height(Length::Fixed(28.0))
                .content_fit(ContentFit::Cover),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center)
        .into(),
        tab,
        current,
        badge,
    )
}

fn rail_button_with<'a>(
    content: Element<'a, Message>,
    tab: SidebarTab,
    current: SidebarTab,
    badge: usize,
) -> Element<'a, Message> {
    let active = tab == current;
    let circle = button(content)
        .on_press(Message::SidebarTabChanged(tab))
        .width(Length::Fixed(44.0))
        .height(Length::Fixed(44.0))
        .padding(0)
        .style(move |theme: &Theme, status| rail_button_style(theme, status, active));

    // Active-tab hard bar on the left.
    let indicator = container(Space::new(Length::Fixed(0.0), Length::Fixed(0.0)))
        .width(Length::Fixed(if active { 3.0 } else { 0.0 }))
        .height(Length::Fixed(if active { 32.0 } else { 0.0 }))
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(c_accent())),
            border: Border {
                radius: r0(),
                ..Default::default()
            },
            ..Default::default()
        });
    let indicator_slot = container(indicator)
        .width(Length::Fixed(48.0))
        .height(Length::Fixed(44.0))
        .align_x(iced::alignment::Horizontal::Left)
        .align_y(iced::alignment::Vertical::Center)
        .padding(iced::Padding {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    let button_slot = container(circle)
        .width(Length::Fixed(48.0))
        .height(Length::Fixed(44.0))
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center);

    let core = stack![button_slot, indicator_slot];

    if badge == 0 {
        return core.into();
    }

    let badge_label = if badge > 9 {
        "9+".to_string()
    } else {
        badge.to_string()
    };
    let badge_box = container(text(badge_label).size(10).font(mono()))
        .padding([2, 5])
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(c_danger())),
            text_color: Some(Color::WHITE),
            border: Border {
                radius: r0(),
                width: 1.0,
                color: c_bg_tertiary(),
            },
            ..Default::default()
        });
    let badge_positioned = container(badge_box)
        .width(Length::Fixed(48.0))
        .height(Length::Fixed(44.0))
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Top);

    stack![core, badge_positioned].into()
}

pub(crate) fn rail_button_style(_theme: &Theme, status: button::Status, active: bool) -> button::Style {
    // Hard squares — no soft pills.
    let bg = if active {
        Color::from_rgba(0.30, 0.72, 0.52, 0.18)
    } else {
        match status {
            button::Status::Hovered => c_bg_hover(),
            button::Status::Pressed => c_bg_elevated(),
            _ => c_bg_secondary(),
        }
    };
    let border_color = if active {
        c_accent()
    } else {
        match status {
            button::Status::Hovered => c_border_strong(),
            _ => c_border(),
        }
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if active { c_accent() } else { c_text_primary() },
        border: Border {
            radius: r0(),
            width: 1.0,
            color: border_color,
        },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

pub(crate) fn rail_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_tertiary())),
        border: Border {
            color: c_border(),
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
    ))
}

pub(crate) fn avatar<'a>(
    name: &str,
    online: Option<bool>,
    color_hex: &str,
    photo: Option<image::Handle>,
) -> Element<'a, Message> {
    let circle: Element<'a, Message> = if let Some(handle) = photo {
        container(
            image(handle)
                .width(Length::Fixed(36.0))
                .height(Length::Fixed(36.0))
                .content_fit(ContentFit::Cover),
        )
        .width(Length::Fixed(36.0))
        .height(Length::Fixed(36.0))
        .into()
    } else {
        let initial = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        let bg_color = parse_hex_color(color_hex).unwrap_or_else(c_accent);
        container(text(initial).size(14))
            .width(Length::Fixed(36.0))
            .height(Length::Fixed(36.0))
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center)
            .style(move |_theme: &Theme| container::Style {
                background: Some(Background::Color(bg_color)),
                text_color: Some(Color::from_rgb(0.02, 0.04, 0.02)),
                border: Border {
                    radius: r0(),
                    width: 1.0,
                    color: c_border_strong(),
                },
                ..Default::default()
            })
            .into()
    };

    let Some(is_user_online) = online else {
        return circle.into();
    };

    let dot_color = if is_user_online { c_online() } else { c_offline() };
    let dot = container(Space::new(Length::Fixed(0.0), Length::Fixed(0.0)))
        .width(Length::Fixed(10.0))
        .height(Length::Fixed(10.0))
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(dot_color)),
            border: Border {
                radius: r0(),
                width: 2.0,
                color: c_bg_secondary(),
            },
            ..Default::default()
        });
    let dot_positioned = container(dot)
        .width(Length::Fixed(36.0))
        .height(Length::Fixed(36.0))
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom);

    stack![circle, dot_positioned].into()
}

pub(crate) fn handle_key_press(
    key: iced::keyboard::Key,
    _modifiers: iced::keyboard::Modifiers,
) -> Option<Message> {
    match key {
        iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
            Some(Message::EscapePressed)
        }
        _ => None,
    }
}

/// Feeds raw window mouse events into the active panel drag-resize, if any.
/// Only subscribed to while `App::resizing_panel` is `Some` (see
/// `App::subscription`), since these events would otherwise flood the
/// update loop on every cursor move.
pub(crate) fn panel_resize_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
            Some(Message::PanelResizeMoved(position.x))
        }
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Message::PanelResizeEnded)
        }
        _ => None,
    }
}

/// A thin drag handle placed between two panels; dragging it adjusts the
/// given panel's stored width via `Message::PanelResize*`.
pub(crate) fn resize_handle(kind: ResizePanel) -> Element<'static, Message> {
    mouse_area(
        container(Space::new(Length::Fixed(4.0), Length::Fill))
            .style(|_theme: &Theme| container::Style {
                background: Some(Background::Color(c_border())),
                ..Default::default()
            }),
    )
    .on_press(Message::PanelResizeStarted(kind))
    .interaction(iced::mouse::Interaction::ResizingHorizontally)
    .into()
}

pub(crate) fn is_online(last_seen_at: f64) -> bool {
    if last_seen_at <= 0.0 {
        return false;
    }
    let now = chrono::Utc::now().timestamp_millis() as f64;
    (now - last_seen_at) < ONLINE_THRESHOLD_MS
}

pub(crate) fn admin_badge_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_danger_soft())),
        text_color: Some(c_danger()),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: Color::from_rgba(0.85, 0.32, 0.34, 0.45),
        },
        ..Default::default()
    }
}

pub(crate) fn sidebar_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_secondary())),
        border: Border {
            color: c_border(),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn account_panel_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_tertiary())),
        border: Border {
            color: c_border(),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn chat_area_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_primary())),
        ..Default::default()
    }
}

pub(crate) fn composer_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_primary())),
        border: Border {
            color: c_border(),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn pill_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused);
    let hovered = matches!(status, text_input::Status::Hovered);
    text_input::Style {
        background: Background::Color(if focused || hovered {
            Color::from_rgb(0.04, 0.06, 0.04)
        } else {
            c_bg_tertiary()
        }),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: if focused {
                c_accent()
            } else if hovered {
                c_border_strong()
            } else {
                c_border()
            },
        },
        icon: c_text_muted(),
        placeholder: c_text_muted(),
        value: c_text_primary(),
        selection: Color::from_rgba(0.30, 0.72, 0.52, 0.28),
    }
}

pub(crate) fn link_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let color = match status {
        button::Status::Hovered | button::Status::Pressed => c_accent(),
        _ => c_text_muted(),
    };
    button::Style {
        background: match status {
            button::Status::Hovered => Some(Background::Color(c_accent_soft())),
            button::Status::Pressed => Some(Background::Color(Color::from_rgba(0.30, 0.72, 0.52, 0.22))),
            _ => None,
        },
        text_color: color,
        border: Border {
            radius: r0(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub(crate) fn secondary_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => c_bg_hover(),
        button::Status::Pressed => c_bg_elevated(),
        button::Status::Disabled => Color::from_rgb(0.08, 0.10, 0.08),
        _ => c_bg_elevated(),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: c_text_primary(),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border(),
        },
        ..Default::default()
    }
}

pub(crate) fn danger_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(0.85, 0.18, 0.18),
        button::Status::Pressed => Color::from_rgb(0.70, 0.12, 0.12),
        button::Status::Disabled => Color::from_rgb(0.30, 0.12, 0.12),
        _ => c_danger(),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::WHITE,
        border: Border {
            radius: r0(),
            width: 1.0,
            color: Color::from_rgba(0.85, 0.32, 0.34, 0.50),
        },
        ..Default::default()
    }
}

pub(crate) fn success_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(0.26, 0.66, 0.47),
        button::Status::Pressed => c_accent_dim(),
        button::Status::Disabled => Color::from_rgb(0.10, 0.22, 0.12),
        _ => c_accent_dim(),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::from_rgb(0.02, 0.05, 0.02),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_accent(),
        },
        ..Default::default()
    }
}

pub(crate) fn reaction_pill_style(
    _theme: &Theme,
    status: button::Status,
    reacted_by_me: bool,
) -> button::Style {
    let base = if reacted_by_me {
        c_accent_soft()
    } else {
        Color::from_rgba(0.30, 0.72, 0.52, 0.04)
    };
    let bg = match status {
        button::Status::Hovered => {
            if reacted_by_me {
                Color::from_rgba(0.30, 0.72, 0.52, 0.24)
            } else {
                Color::from_rgba(0.30, 0.72, 0.52, 0.10)
            }
        }
        _ => base,
    };
    let border_color = if reacted_by_me {
        c_accent()
    } else {
        c_border()
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if reacted_by_me {
            c_accent()
        } else {
            c_text_muted()
        },
        border: Border {
            radius: r0(),
            width: 1.0,
            color: border_color,
        },
        ..Default::default()
    }
}

pub(crate) fn accent_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(0.34, 0.74, 0.55),
        button::Status::Pressed => c_accent_dim(),
        button::Status::Disabled => Color::from_rgb(0.12, 0.28, 0.14),
        _ => c_accent(),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::from_rgb(0.02, 0.05, 0.02),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_accent(),
        },
        shadow: match status {
            button::Status::Disabled => Shadow::default(),
            _ => soft_shadow(),
        },
        ..Default::default()
    }
}

pub(crate) fn auth_card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_secondary())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border_strong(),
        },
        shadow: soft_shadow(),
        ..Default::default()
    }
}

pub(crate) fn auth_shell_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_tertiary())),
        ..Default::default()
    }
}

pub(crate) fn field_label<'a>(label: &str) -> Element<'a, Message> {
    text(label.to_uppercase())
        .size(10)
        .font(mono())
        .style(|_theme: &Theme| text::Style {
            color: Some(c_text_muted()),
        })
        .into()
}

pub(crate) fn badge_chip<'a>(
    label: &'a str,
    bg: Color,
    fg: Color,
) -> Element<'a, Message> {
    container(
        text(label)
            .size(9)
            .font(mono())
            .style(move |_theme: &Theme| text::Style {
                color: Some(fg),
            }),
    )
    .padding([2, 5])
    .style(move |_theme: &Theme| container::Style {
        background: Some(Background::Color(bg)),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: Color::from_rgba(fg.r, fg.g, fg.b, 0.35),
        },
        ..Default::default()
    })
    .into()
}

pub(crate) fn privacy_toggle_row<'a>(
    label: &'a str,
    hint: &'a str,
    on: bool,
    on_press: Message,
) -> Element<'a, Message> {
    container(
        row![
            column![
                text(label).size(13).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_primary()),
                }),
                text(hint).size(11).style(|_theme: &Theme| text::Style {
                    color: Some(c_text_muted()),
                }),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text(if on { "ON" } else { "OFF" }).size(12))
                .on_press(on_press)
                .padding([8, 14])
                .style(if on {
                    accent_button_style
                } else {
                    secondary_button_style
                }),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center),
    )
    .padding(12)
    .style(panel_box_style)
    .into()
}

pub(crate) fn section_title<'a>(label: &'a str) -> Element<'a, Message> {
    text(label)
        .size(16)
        .style(|_theme: &Theme| text::Style {
            color: Some(c_text_primary()),
        })
        .into()
}

pub(crate) fn section_title_owned(label: impl Into<String>) -> Element<'static, Message> {
    text(label.into())
        .size(16)
        .style(|_theme: &Theme| text::Style {
            color: Some(c_text_primary()),
        })
        .into()
}

pub(crate) fn field_label_owned(label: impl Into<String>) -> Element<'static, Message> {
    text(label.into().to_uppercase())
        .size(10)
        .font(mono())
        .style(|_theme: &Theme| text::Style {
            color: Some(c_text_muted()),
        })
        .into()
}

pub(crate) fn muted_text<'a>(label: impl Into<String>, size: u16) -> Element<'a, Message> {
    text(label.into())
        .size(size)
        .style(|_theme: &Theme| text::Style {
            color: Some(c_text_muted()),
        })
        .into()
}

pub(crate) fn status_banner(message: &str, danger: bool) -> Element<'_, Message> {
    container(
        text(message.to_string()).size(12).style(move |_theme: &Theme| {
            text::Style {
                color: Some(if danger { c_danger() } else { c_accent() }),
            }
        }),
    )
    .padding([10, 12])
    .width(Length::Fill)
    .style(move |_theme: &Theme| {
        if danger {
            error_box_style(_theme)
        } else {
            container::Style {
                background: Some(Background::Color(c_accent_soft())),
                border: Border {
                    radius: r0(),
                    width: 1.0,
                    color: c_border_strong(),
                },
                ..Default::default()
            }
        }
    })
    .into()
}

pub(crate) fn message_hover_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba(0.30, 0.72, 0.52, 0.05))),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: Color::from_rgba(0.30, 0.72, 0.52, 0.12),
        },
        ..Default::default()
    }
}

pub(crate) fn panel_box_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_elevated())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border(),
        },
        ..Default::default()
    }
}

pub(crate) fn section_card_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_secondary())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border_strong(),
        },
        shadow: light_shadow(),
        ..Default::default()
    }
}

/// A settings section wrapped as a full-width card. Every card fills the
/// content measure so they line up as a clean vertical stack instead of
/// each shrinking to its own content width (the ragged look).
pub(crate) fn settings_card<'a>(inner: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(inner)
        .padding(22)
        .width(Length::Fill)
        .style(section_card_style)
        .into()
}

/// Wraps a settings/pane content column in a centered, max-width scroll so
/// it sits in a readable measure in the middle of the pane instead of
/// hugging the left edge with dead space on the right. Content and the
/// cards inside it should be `width(Fill)` so everything shares one edge.
pub(crate) fn settings_pane<'a>(
    content: iced::widget::Column<'a, Message>,
) -> Element<'a, Message> {
    container(
        scrollable(
            container(content.width(Length::Fill).max_width(720))
                .center_x(Length::Fill)
                .padding(iced::Padding {
                    top: 28.0,
                    right: 32.0,
                    bottom: 40.0,
                    left: 32.0,
                }),
        )
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(chat_area_style)
    .into()
}

pub(crate) fn error_box_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_danger_soft())),
        text_color: Some(Color::from_rgb(1.0, 0.75, 0.75)),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: Color::from_rgba(0.85, 0.32, 0.34, 0.45),
        },
        ..Default::default()
    }
}

pub(crate) fn success_box_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_accent_soft())),
        text_color: Some(c_accent()),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border_strong(),
        },
        ..Default::default()
    }
}

pub(crate) fn sidebar_item_style(_theme: &Theme, status: button::Status, active: bool) -> button::Style {
    let (bg, text_color) = if active {
        (c_accent_soft(), c_accent())
    } else {
        match status {
            button::Status::Hovered => (c_bg_hover(), c_text_primary()),
            button::Status::Pressed => (c_bg_elevated(), c_text_primary()),
            _ => (Color::TRANSPARENT, c_text_muted()),
        }
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: r0(),
            width: if active { 1.0 } else { 0.0 },
            color: if active { c_accent() } else { Color::TRANSPARENT },
        },
        ..Default::default()
    }
}

pub(crate) fn chat_header_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_primary())),
        border: Border {
            color: c_border(),
            width: 1.0,
            radius: r0(),
        },
        ..Default::default()
    }
}

pub(crate) fn transparent_container_style(_theme: &Theme) -> container::Style {
    container::Style::default()
}

pub(crate) fn call_banner_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.28, 0.18))),
        border: Border {
            color: Color::from_rgba(0.24, 0.60, 0.44, 0.35),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn call_banner_ringing_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.28, 0.20, 0.10))),
        border: Border {
            color: Color::from_rgba(0.96, 0.68, 0.26, 0.40),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

pub(crate) fn attachment_frame_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_bg_tertiary())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border(),
        },
        ..Default::default()
    }
}

pub(crate) fn reply_preview_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c_accent_soft())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border_strong(),
        },
        ..Default::default()
    }
}

pub(crate) fn date_separator<'a>(day: &str) -> Element<'a, Message> {
    let line = || {
        container(Space::new(Length::Fill, Length::Fixed(1.0))).style(|_theme: &Theme| {
            container::Style {
                background: Some(Background::Color(c_border())),
                ..Default::default()
            }
        })
    };
    let pill = container(
        text(day.to_uppercase())
            .size(10)
            .font(mono())
            .style(|_theme: &Theme| text::Style {
                color: Some(c_accent()),
            }),
    )
    .padding([4, 12])
    .style(|_theme: &Theme| container::Style {
        background: Some(Background::Color(c_bg_elevated())),
        border: Border {
            radius: r0(),
            width: 1.0,
            color: c_border(),
        },
        ..Default::default()
    });

    container(
        row![line().width(Length::Fill), pill, line().width(Length::Fill)]
            .spacing(12)
            .align_y(iced::Alignment::Center),
    )
    .padding([14, 8])
    .into()
}

pub(crate) fn format_time(sent_at_ms: f64) -> String {
    match Local.timestamp_millis_opt(sent_at_ms as i64) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M").to_string(),
        _ => String::new(),
    }
}

pub(crate) fn format_relative_time(sent_at_ms: f64) -> String {
    if sent_at_ms <= 0.0 {
        return String::new();
    }
    let now = chrono::Utc::now().timestamp_millis() as f64;
    let delta_ms = (now - sent_at_ms).max(0.0);
    let mins = (delta_ms / 60_000.0).floor() as i64;
    if mins < 1 {
        return "Just now".to_string();
    }
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    match Local.timestamp_millis_opt(sent_at_ms as i64) {
        chrono::LocalResult::Single(dt) => dt.format("%b %-d").to_string(),
        _ => String::new(),
    }
}

pub(crate) fn next_friend_request_privacy(current: &str) -> &'static str {
    match current {
        "everyone" => "mutual_servers",
        "mutual_servers" => "nobody",
        _ => "everyone",
    }
}

pub(crate) fn friend_request_privacy_label(current: &str) -> &'static str {
    match current {
        "mutual_servers" => "Shared servers only",
        "nobody" => "Nobody",
        _ => "Everyone",
    }
}

pub(crate) fn next_presence_status(current: &str) -> &'static str {
    match current {
        "online" => "idle",
        "idle" => "dnd",
        "dnd" => "invisible",
        _ => "online",
    }
}

pub(crate) fn presence_label(presence: &str) -> &'static str {
    match presence {
        "online" => "Online",
        "idle" => "Idle",
        "dnd" => "Do not disturb",
        "invisible" => "Invisible",
        _ => "Offline",
    }
}

pub(crate) fn filter_chip<'a>(
    label: &'a str,
    active: bool,
    on_press: Message,
) -> Element<'a, Message> {
    button(text(label).size(11))
        .on_press(on_press)
        .padding([6, 10])
        .style(if active {
            accent_button_style
        } else {
            secondary_button_style
        })
        .into()
}

pub(crate) fn obj_str_list(obj: &BTreeMap<String, Value>, key: &str) -> Vec<String> {
    match obj.get(key) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn typing_label(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [a] => Some(format!("{a} is typing…")),
        [a, b] => Some(format!("{a} and {b} are typing…")),
        _ => Some("Several people are typing…".to_string()),
    }
}

pub(crate) fn format_day(sent_at_ms: f64) -> String {
    let now = Local::now();
    match Local.timestamp_millis_opt(sent_at_ms as i64) {
        chrono::LocalResult::Single(dt) => {
            let today = now.date_naive();
            let msg_date = dt.date_naive();
            let diff = (today - msg_date).num_days();
            if diff == 0 {
                "Today".to_string()
            } else if diff == 1 {
                "Yesterday".to_string()
            } else {
                dt.format("%B %-d, %Y").to_string()
            }
        }
        _ => String::new(),
    }
}
