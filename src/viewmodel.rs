//! Converts `App`'s raw domain state into the plain, pre-flattened Slint
//! view-model structs declared in `ui/chat_types.slint` (display strings,
//! colors and flags already computed here, exactly like the inline
//! formatting `src/view/chat.rs` used to do inside its `view()` calls).
//!
//! Avatar/attachment/screenshare images are intentionally left as the
//! default empty `slint::Image` for now -- see the image-handling pass
//! (ports `image::Handle` -> `slint::Image`) that fills these in.

use crate::ui;
use crate::*;

pub(crate) fn hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return slint::Color::from_rgb_u8(61, 148, 107);
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(61);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(148);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(107);
    slint::Color::from_rgb_u8(r, g, b)
}

pub(crate) fn initial(s: &str) -> slint::SharedString {
    s.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

pub(crate) fn badge_for_platform_role(role: &str) -> (slint::SharedString, slint::Color, slint::Color) {
    match role {
        "owner" => (
            "OWNER".into(),
            slint::Color::from_rgb_u8(224, 179, 71),
            slint::Color::from_rgb_u8(13, 10, 0),
        ),
        "admin" => (
            "STAFF".into(),
            slint::Color::from_rgb_u8(209, 102, 71),
            slint::Color::from_rgb_u8(255, 255, 255),
        ),
        "moderator" => (
            "MOD".into(),
            slint::Color::from_rgb_u8(64, 140, 242),
            slint::Color::from_rgb_u8(255, 255, 255),
        ),
        _ => (
            "".into(),
            slint::Color::from_rgb_u8(0, 0, 0),
            slint::Color::from_rgb_u8(0, 0, 0),
        ),
    }
}

pub(crate) fn conversation_rows(
    conversations: &[ConversationSummary],
    active_id: Option<&str>,
) -> Vec<ui::ConversationRow> {
    conversations
        .iter()
        .map(|c| ui::ConversationRow {
            id: c.conversation_id.clone().into(),
            title: c.title.clone().into(),
            unread: c.unread,
            active: active_id == Some(c.conversation_id.as_str()),
        })
        .collect()
}

pub(crate) fn friend_rows(friends: &[Friend]) -> Vec<ui::FriendRow> {
    friends
        .iter()
        .map(|f| {
            let online = f.is_online_like();
            let mut subtitle = format!("@{}", f.username);
            if !f.status_message.is_empty() {
                subtitle.push_str(" · ");
                subtitle.push_str(&f.status_message);
            } else if !f.mutual_servers.is_empty() {
                subtitle.push_str(" · ");
                subtitle.push_str(&f.mutual_servers.join(", "));
            }
            let meta = format!(
                "{} · friends since {}",
                presence_label(&f.presence),
                format_relative_time(f.friends_since)
            );
            let label = if f.favorite {
                format!("★ {}", f.label())
            } else {
                f.label().to_string()
            };
            ui::FriendRow {
                user_id: f.user_id.clone().into(),
                label: label.into(),
                subtitle: subtitle.into(),
                meta: meta.into(),
                initial: initial(f.label()),
                avatar_color: hex_color(&f.avatar_color),
                photo: Default::default(),
                photo_url: f.avatar_image_url.clone().into(),
                online,
                favorite: f.favorite,
            }
        })
        .collect()
}

pub(crate) fn group_candidate_rows(
    friends: &[Friend],
    selected: &std::collections::BTreeSet<String>,
) -> Vec<ui::GroupCandidateRow> {
    friends
        .iter()
        .map(|f| ui::GroupCandidateRow {
            user_id: f.user_id.clone().into(),
            label: f.display_name.clone().into(),
            selected: selected.contains(&f.user_id),
        })
        .collect()
}

pub(crate) fn incoming_request_rows(incoming_requests: &[IncomingRequest]) -> Vec<ui::IncomingRequestRow> {
    incoming_requests
        .iter()
        .map(|r| ui::IncomingRequestRow {
            request_id: r.request_id.clone().into(),
            from_user_id: r.from_user_id.clone().into(),
            from_display_name: r.from_display_name.clone().into(),
            sub: format!(
                "@{} · {}",
                r.from_username,
                format_relative_time(r.sent_at)
            )
            .into(),
            note: r.note.clone().into(),
            status_message: r.from_status_message.clone().into(),
            initial: initial(&r.from_display_name),
            avatar_color: hex_color(&r.from_avatar_color),
            photo: Default::default(),
            photo_url: r.from_avatar_image_url.clone().into(),
            online: r.presence != "offline",
        })
        .collect()
}

pub(crate) fn outgoing_request_rows(outgoing_requests: &[OutgoingRequest]) -> Vec<ui::OutgoingRequestRow> {
    outgoing_requests
        .iter()
        .map(|r| ui::OutgoingRequestRow {
            request_id: r.request_id.clone().into(),
            to_user_id: r.to_user_id.clone().into(),
            to_display_name: r.to_display_name.clone().into(),
            to_username: r.to_username.clone().into(),
            sent_label: format!("Sent · {}", format_relative_time(r.sent_at)).into(),
            note: r.note.clone().into(),
            initial: initial(&r.to_display_name),
            avatar_color: hex_color(&r.to_avatar_color),
            photo: Default::default(),
            photo_url: r.to_avatar_image_url.clone().into(),
        })
        .collect()
}

pub(crate) fn people_hit_rows(people_hits: &[PeopleHit]) -> Vec<ui::PeopleHitRow> {
    people_hits
        .iter()
        .map(|h| {
            let mut meta = format!("@{}", h.username);
            if !h.mutual_servers.is_empty() {
                meta.push_str(" · ");
                meta.push_str(&h.mutual_servers.join(", "));
            }
            ui::PeopleHitRow {
                user_id: h.user_id.clone().into(),
                username: h.username.clone().into(),
                display_name: h.display_name.clone().into(),
                meta: meta.into(),
                initial: initial(&h.display_name),
                avatar_color: hex_color(&h.avatar_color),
                photo: Default::default(),
                photo_url: h.avatar_image_url.clone().into(),
                online: h.presence != "offline",
                relation: h.relation.clone().into(),
                incoming_request_id: h.incoming_request_id.clone().into(),
                is_staff: h.is_staff,
            }
        })
        .collect()
}

pub(crate) fn suggestion_rows(suggestions: &[FriendSuggestion]) -> Vec<ui::SuggestionRow> {
    suggestions
        .iter()
        .take(6)
        .map(|s| {
            let meta = if s.mutual_servers.is_empty() {
                format!("@{}", s.username)
            } else {
                format!("@{} · {}", s.username, s.mutual_servers.join(", "))
            };
            ui::SuggestionRow {
                user_id: s.user_id.clone().into(),
                username: s.username.clone().into(),
                display_name: s.display_name.clone().into(),
                meta: meta.into(),
                initial: initial(&s.display_name),
                avatar_color: hex_color(&s.avatar_color),
                photo: Default::default(),
                photo_url: s.avatar_image_url.clone().into(),
                online: s.presence != "offline",
            }
        })
        .collect()
}

pub(crate) fn blocked_rows(blocked: &[BlockedUser]) -> Vec<ui::BlockedRow> {
    blocked
        .iter()
        .map(|b| ui::BlockedRow {
            user_id: b.user_id.clone().into(),
            display_name: b.display_name.clone().into(),
        })
        .collect()
}

pub(crate) fn server_rows(servers: &[ServerSummary], selected_id: Option<&str>) -> Vec<ui::ServerRow> {
    servers
        .iter()
        .map(|s| ui::ServerRow {
            server_id: s.server_id.clone().into(),
            name: s.name.clone().into(),
            initial: s
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "#".to_string())
                .into(),
            icon: Default::default(),
            icon_url: s.icon_url.clone().into(),
            active: selected_id == Some(s.server_id.as_str()),
        })
        .collect()
}

pub(crate) fn channel_rows(
    channels: &[ChannelSummary],
    active_id: Option<&str>,
    voice: bool,
) -> Vec<ui::ChannelRow> {
    channels
        .iter()
        .filter(|c| (c.channel_type == "voice") == voice)
        .map(|c| ui::ChannelRow {
            conversation_id: c.conversation_id.clone().into(),
            label: if voice {
                format!("v  {}", c.name)
            } else {
                format!("#  {}", c.name)
            }
            .into(),
            is_voice: voice,
            active: active_id == Some(c.conversation_id.as_str()),
        })
        .collect()
}

pub(crate) fn admin_user_rows(
    admin_users: &[AdminUserRow],
    search_input: &str,
    my_username: &str,
) -> Vec<ui::AdminUserRow> {
    let search = search_input.trim().to_lowercase();
    admin_users
        .iter()
        .filter(|u| {
            search.is_empty()
                || u.username.to_lowercase().contains(&search)
                || u.display_name.to_lowercase().contains(&search)
        })
        .map(|u| {
            let role_locked = u.role == "owner" || u.username == my_username;
            let status_line = if u.banned {
                format!("{} · banned", u.role)
            } else {
                u.role.clone()
            };
            let (badge_text, badge_bg, badge_fg) = badge_for_platform_role(&u.role);
            ui::AdminUserRow {
                user_id: u.user_id.clone().into(),
                username: u.username.clone().into(),
                display_name: u.display_name.clone().into(),
                role: u.role.clone().into(),
                banned: u.banned,
                role_locked,
                status_line: status_line.into(),
                badge_text,
                badge_bg,
                badge_fg,
            }
        })
        .collect()
}

pub(crate) fn member_rows(members: &[ServerMemberRow]) -> Vec<ui::MemberRow> {
    members
        .iter()
        .map(|m| {
            let (mut badge_text, mut badge_bg, mut badge_fg) = if m.is_owner {
                (
                    "OWNER".into(),
                    slint::Color::from_rgb_u8(209, 188, 77),
                    slint::Color::from_rgb_u8(13, 13, 5),
                )
            } else {
                badge_for_platform_role(&m.platform_role)
            };
            if m.is_bot && badge_text.as_str().is_empty() {
                badge_text = "*bot".into();
                badge_bg = hex_color("#4DB884");
                badge_fg = slint::Color::from_rgb_u8(5, 13, 5);
            }
            ui::MemberRow {
                user_id: m.user_id.clone().into(),
                display_name: m.display_name.clone().into(),
                initial: initial(&m.display_name),
                avatar_color: hex_color(&m.avatar_color),
                photo: Default::default(),
                photo_url: m.avatar_image_url.clone().into(),
                online: is_online(m.last_seen_at),
                is_bot: m.is_bot,
                badge_text,
                badge_bg,
                badge_fg,
                roles: m
                    .roles
                    .iter()
                    .map(|r| ui::RoleTagRow {
                        name: r.name.clone().into(),
                        color: hex_color(&r.color),
                    })
                    .collect::<Vec<_>>()
                    .as_slice()
                    .into(),
            }
        })
        .collect()
}

/// Merges Convex history + live peerseal messages for the open DM, same
/// source list `view_chat` built in src/view/chat.rs.
fn display_messages<'a>(
    messages: &'a [ChatMessage],
    live_messages: Option<&'a [ChatMessage]>,
) -> Vec<&'a ChatMessage> {
    let mut v: Vec<&ChatMessage> = messages.iter().collect();
    if let Some(live) = live_messages {
        v.extend(live.iter());
    }
    v
}

pub(crate) fn chat_message_rows(
    messages: &[ChatMessage],
    live_messages: Option<&[ChatMessage]>,
    my_user_id: &str,
    is_admin: bool,
) -> Vec<ui::ChatMessageRow> {
    let mut rows = Vec::new();
    let mut last_author: Option<String> = None;
    let mut last_day: Option<String> = None;

    for msg in display_messages(messages, live_messages) {
        let mine = msg.author_id == my_user_id;
        let is_call_log = msg.kind == "call";
        let deleted_visible = msg.deleted && (is_admin || mine);
        let can_edit = mine && !msg.deleted && !is_call_log;
        let can_delete = if is_call_log {
            is_admin && !msg.deleted
        } else {
            (mine || is_admin) && !msg.deleted
        };
        let can_purge = is_admin && msg.deleted;
        let can_react = !msg.deleted && !is_call_log;

        let day = format_day(msg.sent_at);
        let new_day = last_day.as_deref() != Some(day.as_str());
        if new_day {
            last_day = Some(day.clone());
            last_author = None;
        }
        // Date separators are rendered as their own zero-interaction call-log-style row.
        if new_day {
            rows.push(ui::ChatMessageRow {
                id: format!("date-sep-{}", msg.id).into(),
                author_id: "".into(),
                author_name: "".into(),
                author_initial: "".into(),
                author_avatar_color: hex_color("#4DB884"),
                author_photo: Default::default(),
                author_photo_url: Default::default(),
                attachment_url: Default::default(),
                is_bot: false,
                mine: false,
                encrypted: false,
                is_call_log: true,
                grouped: false,
                meta: "".into(),
                reply_line: "".into(),
                body: day.into(),
                body_danger: false,
                has_attachment: false,
                attachment_loading: false,
                attachment: Default::default(),
                reactions: Default::default(),
                can_edit: false,
                can_delete: false,
                can_purge: false,
                can_react: false,
            });
        }

        if is_call_log {
            last_author = None;
            rows.push(ui::ChatMessageRow {
                id: msg.id.clone().into(),
                author_id: msg.author_id.clone().into(),
                author_name: "".into(),
                author_initial: "".into(),
                author_avatar_color: hex_color("#4DB884"),
                author_photo: Default::default(),
                author_photo_url: Default::default(),
                attachment_url: Default::default(),
                is_bot: false,
                mine,
                encrypted: msg.encrypted,
                is_call_log: true,
                grouped: false,
                meta: "".into(),
                reply_line: "".into(),
                body: msg.body.clone().into(),
                body_danger: deleted_visible,
                has_attachment: false,
                attachment_loading: false,
                attachment: Default::default(),
                reactions: Default::default(),
                can_edit: false,
                can_delete,
                can_purge,
                can_react: false,
            });
            continue;
        }

        let grouped = last_author.as_deref() == Some(msg.author_id.as_str());
        last_author = Some(msg.author_id.clone());

        let mut meta = format_time(msg.sent_at);
        if msg.edited {
            meta = format!("{meta} (edited)");
        }
        let reply_line = msg
            .reply_to
            .as_ref()
            .map(|(author, snippet)| format!("↩ {author}: {snippet}"))
            .unwrap_or_default();
        let body = if deleted_visible {
            if msg.body.is_empty() {
                "(deleted)".to_string()
            } else {
                format!("{} (deleted)", msg.body)
            }
        } else {
            msg.body.clone()
        };

        rows.push(ui::ChatMessageRow {
            id: msg.id.clone().into(),
            author_id: msg.author_id.clone().into(),
            author_name: msg.author_name.clone().into(),
            author_initial: initial(&msg.author_name),
                author_avatar_color: hex_color(&msg.author_avatar_color),
                author_photo: Default::default(),
                author_photo_url: msg.author_avatar_url.clone().into(),
                attachment_url: msg.attachment_url.clone().into(),
            is_bot: msg.author_is_bot,
            mine,
            encrypted: msg.encrypted,
            is_call_log: false,
            grouped,
            meta: meta.into(),
            reply_line: reply_line.into(),
            body: body.into(),
            body_danger: deleted_visible,
            has_attachment: !deleted_visible && !msg.attachment_url.is_empty(),
            attachment_loading: !deleted_visible && !msg.attachment_url.is_empty(),
            attachment: Default::default(),
            reactions: if deleted_visible {
                Default::default()
            } else {
                msg.reactions
                    .iter()
                    .map(|(emoji, count, reacted)| ui::ReactionRow {
                        emoji: emoji.clone().into(),
                        count: *count as i32,
                        reacted_by_me: *reacted,
                    })
                    .collect::<Vec<_>>()
                    .as_slice()
                    .into()
            },
            can_edit,
            can_delete,
            can_purge,
            can_react,
        });
    }
    rows
}

/// `id` round-trips through `encode_share_target`/`decode_share_target`
/// below since `ShareTarget` has no id of its own -- just a monitor/window
/// name -- and the Slint side only ever hands the id back on click.
pub(crate) fn share_target_rows(share_targets: &[screenshare::ShareTarget]) -> Vec<ui::ShareTargetRow> {
    share_targets
        .iter()
        .map(|t| ui::ShareTargetRow {
            id: encode_share_target(t).into(),
            label: t.label().to_string().into(),
        })
        .collect()
}

pub(crate) fn encode_share_target(t: &screenshare::ShareTarget) -> String {
    match t {
        screenshare::ShareTarget::Monitor(name) => format!("monitor:{name}"),
        screenshare::ShareTarget::Window(title) => format!("window:{title}"),
    }
}

pub(crate) fn decode_share_target(encoded: &str) -> Option<screenshare::ShareTarget> {
    if let Some(name) = encoded.strip_prefix("monitor:") {
        Some(screenshare::ShareTarget::Monitor(name.to_string()))
    } else if let Some(title) = encoded.strip_prefix("window:") {
        Some(screenshare::ShareTarget::Window(title.to_string()))
    } else {
        None
    }
}
