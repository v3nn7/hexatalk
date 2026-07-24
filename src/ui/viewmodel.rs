//! Converts `App`'s raw domain state into the plain, pre-flattened Slint
//! view-model structs declared in `ui/chat_types.slint` (display strings,
//! colors and flags already computed here, exactly like the inline
//! formatting `src/view/chat.rs` used to do inside its `view()` calls).
//!
//! Avatar/attachment/screenshare images are intentionally left as the
//! default empty `slint::Image` for now -- see the image-handling pass
//! (ports `image::Handle` -> `slint::Image`) that fills these in.

use crate::media::screenshare;
use crate::slint_ui as ui;
use crate::state::types::{
    AdminUserRow, BlockedUser, ChannelSummary, ChatMessage, ConversationSummary, Friend,
    FriendSuggestion, IncomingRequest, MessageReport, OutgoingRequest, PeopleHit, ServerMemberRow,
    ServerSummary, is_online,
};
use crate::ui::mentions;
use crate::state::types::AdminUserDetail;
use crate::ui::utils::{
    format_day, format_relative_time, format_time, format_time_short, presence_label,
};

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

pub(crate) fn badge_for_platform_role(
    role: &str,
) -> (slint::SharedString, slint::Color, slint::Color) {
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
            title: if c.is_support {
                "Support".into()
            } else {
                c.title.clone().into()
            },
            unread: c.unread,
            active: active_id == Some(c.conversation_id.as_str()),
            is_support: c.is_support,
        })
        .collect()
}

/// `"verified"` / `"changed"` / `""` (unverified — the common, unremarkable
/// case, left blank so the UI shows nothing rather than a permanent
/// "unverified" label on every fresh friend).
fn trust_badge_label(badge: Option<&crate::state::trust::TrustBadge>) -> slint::SharedString {
    use crate::state::trust::TrustBadge;
    match badge {
        Some(TrustBadge::Verified) => "verified",
        Some(TrustBadge::FingerprintChanged { .. }) => "changed",
        Some(TrustBadge::Unverified) | None => "",
    }
    .into()
}

pub(crate) fn friend_rows<'a>(
    friends: impl IntoIterator<Item = &'a Friend>,
    trust_badges: &std::collections::HashMap<String, crate::state::trust::TrustBadge>,
) -> Vec<ui::FriendRow> {
    friends
        .into_iter()
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
                trust_badge: trust_badge_label(trust_badges.get(&f.user_id)),
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

pub(crate) fn incoming_request_rows(
    incoming_requests: &[IncomingRequest],
) -> Vec<ui::IncomingRequestRow> {
    incoming_requests
        .iter()
        .map(|r| ui::IncomingRequestRow {
            request_id: r.request_id.clone().into(),
            from_user_id: r.from_user_id.clone().into(),
            from_display_name: r.from_display_name.clone().into(),
            sub: format!("@{} · {}", r.from_username, format_relative_time(r.sent_at)).into(),
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

pub(crate) fn outgoing_request_rows(
    outgoing_requests: &[OutgoingRequest],
) -> Vec<ui::OutgoingRequestRow> {
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

pub(crate) fn server_rows(
    servers: &[ServerSummary],
    selected_id: Option<&str>,
) -> Vec<ui::ServerRow> {
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
        .map(|c| {
            let prefix = if voice {
                "v"
            } else if c.is_announcement {
                "!"
            } else {
                "#"
            };
            let mute_mark = if c.muted { " 🔇" } else { "" };
            ui::ChannelRow {
                conversation_id: c.conversation_id.clone().into(),
                label: format!("{prefix}  {}{mute_mark}", c.name).into(),
                is_voice: voice,
                active: active_id == Some(c.conversation_id.as_str()),
                is_announcement: c.is_announcement,
                muted: c.muted,
                can_send: c.can_send,
                category_id: c.category_id.clone().into(),
            }
        })
        .collect()
}

pub(crate) fn admin_detail_view(detail: Option<&AdminUserDetail>) -> ui::AdminDetailView {
    let Some(d) = detail else {
        return ui::AdminDetailView {
            open: false,
            user_id: "".into(),
            username: "".into(),
            display_name: "".into(),
            role: "".into(),
            banned: false,
            ban_label: "".into(),
            muted: false,
            mute_label: "".into(),
            online: false,
            bio: "".into(),
            status_message: "".into(),
            friend_count: 0,
            servers_label: "".into(),
            created_label: "".into(),
            last_seen_label: "".into(),
        };
    };
    let ban_label = if d.banned {
        if d.ban_expires_at > 0 {
            format!("temp ban until {}", format_time_short(d.ban_expires_at))
        } else {
            "permanent ban".into()
        }
    } else {
        "not banned".into()
    };
    let mute_label = if d.muted {
        if d.mute_expires_at > 0 {
            format!("muted until {}", format_time_short(d.mute_expires_at))
        } else {
            "muted".into()
        }
    } else {
        "not muted".into()
    };
    ui::AdminDetailView {
        open: true,
        user_id: d.user_id.clone().into(),
        username: d.username.clone().into(),
        display_name: d.display_name.clone().into(),
        role: d.role.clone().into(),
        banned: d.banned,
        ban_label: ban_label.into(),
        muted: d.muted,
        mute_label: mute_label.into(),
        online: d.online,
        bio: d.bio.clone().into(),
        status_message: d.status_message.clone().into(),
        friend_count: d.friend_count as i32,
        servers_label: if d.server_names.is_empty() {
            "—".into()
        } else {
            d.server_names.join(", ").into()
        },
        created_label: format_time_short(d.created_at).into(),
        last_seen_label: if d.last_seen_at > 0 {
            format_time_short(d.last_seen_at).into()
        } else {
            "—".into()
        },
    }
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
            let mut status_line = if u.banned {
                if u.ban_expires_at > 0 {
                    format!(
                        "{} · temp ban · until {}",
                        u.role,
                        format_time_short(u.ban_expires_at)
                    )
                } else {
                    format!("{} · permanent ban", u.role)
                }
            } else {
                u.role.clone()
            };
            if u.muted {
                if u.mute_expires_at > 0 {
                    status_line.push_str(&format!(
                        " · muted until {}",
                        format_time_short(u.mute_expires_at)
                    ));
                } else {
                    status_line.push_str(" · muted");
                }
            }
            if u.plus_active {
                if u.plus_expires_at > 0 {
                    status_line.push_str(&format!(
                        " · PLUS until {}",
                        format_time_short(u.plus_expires_at)
                    ));
                } else {
                    status_line.push_str(" · PLUS");
                }
            }
            let (badge_text, badge_bg, badge_fg) = badge_for_platform_role(&u.role);
            ui::AdminUserRow {
                user_id: u.user_id.clone().into(),
                username: u.username.clone().into(),
                display_name: u.display_name.clone().into(),
                role: u.role.clone().into(),
                banned: u.banned,
                muted: u.muted,
                plus_active: u.plus_active,
                role_locked,
                status_line: status_line.into(),
                badge_text,
                badge_bg,
                badge_fg,
            }
        })
        .collect()
}

pub(crate) fn member_rows<'a>(
    members: impl IntoIterator<Item = &'a ServerMemberRow>,
) -> Vec<ui::MemberRow> {
    members
        .into_iter()
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
                is_plus: m.plus_active,
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
/// source list `view_chat` built in src/view/chat.rs. Iterator-based: no
/// intermediate `Vec` allocation per render pass.
fn display_messages<'a>(
    messages: &'a [ChatMessage],
    live_messages: Option<&'a [ChatMessage]>,
) -> impl Iterator<Item = &'a ChatMessage> {
    messages
        .iter()
        .chain(live_messages.unwrap_or(&[]).iter())
}

/// `my_names` are the current user's display name + username (used to flag
/// rows that ping us); `everyone_allowed` gates `@everyone` highlighting to
/// channels/groups (it's plain text in 1:1 DMs).
pub(crate) fn chat_message_rows(
    messages: &[ChatMessage],
    live_messages: Option<&[ChatMessage]>,
    my_user_id: &str,
    is_admin: bool,
    my_names: &[String],
    everyone_allowed: bool,
    reporting_message_id: Option<&str>,
) -> Vec<ui::ChatMessageRow> {
    let mut rows = Vec::new();
    let mut last_author: Option<String> = None;
    let mut last_day: Option<String> = None;
    // Lowercased once for the whole pass instead of per message (the render
    // loop is the hot path for `mentions_any`; see mentions.rs).
    let my_names_lower: Vec<String> = my_names.iter().map(|n| n.to_lowercase()).collect();

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
                is_plus: false,
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
                is_voice_note: false,
                reactions: Default::default(),
                can_edit: false,
                can_delete: false,
                can_purge: false,
                can_react: false,
                can_report: false,
                reporting: false,
                mentions_me: false,
                mentions_everyone: false,
                ping_label: "".into(),
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
                is_plus: false,
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
                is_voice_note: false,
                reactions: Default::default(),
                can_edit: false,
                can_delete,
                can_purge,
                can_react: false,
                can_report: false,
                reporting: false,
                mentions_me: false,
                mentions_everyone: false,
                ping_label: "".into(),
            });
            continue;
        }

        let grouped = last_author.as_deref() == Some(msg.author_id.as_str());
        last_author = Some(msg.author_id.clone());

        let mut meta = format_time(msg.sent_at);
        if msg.edited {
            meta.push_str(" (edited)");
        }
        let reply_line = msg
            .reply_to
            .as_ref()
            .map(|(author, snippet)| format!("↩ {author}: {snippet}"))
            .unwrap_or_default();
        // The `messages:list` REST shape has no attachment content-type field
        // at all (see `net/api/dispatch_conv.rs` module doc) -- there is no
        // backend-provided way to tell a voice note apart from any other
        // attachment once it's round-tripped through history. Same trick as
        // `PEER_CLEAR_HISTORY_CTRL`: a voice note's body is set to
        // `VOICE_NOTE_BODY_TAG` at send time (see `Message::SendMessage`),
        // so detection here is just a body comparison, valid for the
        // sender's own echo, the live peer, and any later history reload.
        let is_voice_note =
            !deleted_visible && !msg.attachment_url.is_empty() && msg.body == crate::VOICE_NOTE_BODY_TAG;
        let body = if deleted_visible {
            if msg.body.is_empty() {
                "(deleted)".to_string()
            } else {
                format!("{} (deleted)", msg.body)
            }
        } else if is_voice_note {
            String::new()
        } else {
            msg.body.clone()
        };

        // Mention highlight (Discord-style): the row is tinted when the body
        // pings the current user (by display name or username) or, in
        // channels/groups, contains the literal @everyone. Mentions are
        // parsed from the raw body, never from the "(deleted)" decoration.
        let mentions_me = !msg.deleted && mentions::mentions_any_lower(&msg.body, &my_names_lower);
        let mentions_everyone =
            !msg.deleted && everyone_allowed && mentions::has_everyone(&msg.body);
        let ping_label = if mentions_me || mentions_everyone {
            format!(
                "pinged {}",
                format_relative_time(msg.sent_at).to_lowercase()
            )
        } else {
            String::new()
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
            is_plus: msg.author_plus_active,
            mine,
            encrypted: msg.encrypted,
            is_call_log: false,
            grouped,
            meta: meta.into(),
            reply_line: reply_line.into(),
            body: body.into(),
            body_danger: deleted_visible,
            has_attachment: !deleted_visible && !msg.attachment_url.is_empty(),
            // Voice notes never go through the image decode/cache path
            // (see the `!row.is_voice_note` guard in main.rs), so there's
            // nothing to "load" -- true here would get stuck forever since
            // nothing ever clears it for a non-image attachment.
            attachment_loading: !deleted_visible && !msg.attachment_url.is_empty() && !is_voice_note,
            attachment: Default::default(),
            is_voice_note,
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
            can_report: !mine && !deleted_visible,
            reporting: reporting_message_id == Some(msg.id.as_str()),
            mentions_me,
            mentions_everyone,
            ping_label: ping_label.into(),
        });
    }
    rows
}

/// `messages:listPinned` rows for the header pinned-messages panel -- a
/// flat, lightweight projection (no grouping/reactions/permissions).
pub(crate) fn pinned_rows(pinned: &[ChatMessage]) -> Vec<ui::PinnedMessageRow> {
    pinned
        .iter()
        .map(|msg| ui::PinnedMessageRow {
            id: msg.id.clone().into(),
            author_name: msg.author_name.clone().into(),
            author_avatar_color: hex_color(&msg.author_avatar_color),
            meta: format_time(msg.sent_at).into(),
            body: if msg.body.is_empty() && !msg.attachment_url.is_empty() {
                "[image]".into()
            } else {
                msg.body.clone().into()
            },
        })
        .collect()
}

/// `reports:adminListReports` rows for the admin panel's Reports section.
pub(crate) fn report_rows(reports: &[MessageReport]) -> Vec<ui::ReportRow> {
    reports
        .iter()
        .map(|r| ui::ReportRow {
            report_id: r.report_id.clone().into(),
            conversation_label: r.conversation_label.clone().into(),
            reporter_username: format!("@{}", r.reporter_username).into(),
            author_username: format!("@{}", r.author_username).into(),
            message_body: r.message_body.clone().into(),
            reason: match r.reason.as_str() {
                "spam" => "Spam",
                "harassment" => "Harassment",
                "illegal_content" => "Illegal content",
                _ => "Other",
            }
            .into(),
            age_label: format_relative_time(r.created_at).into(),
        })
        .collect()
}

/// `id` round-trips through `encode_share_target`/`decode_share_target`
/// below since `ShareTarget` has no id of its own -- just a monitor/window
/// name -- and the Slint side only ever hands the id back on click.
pub(crate) fn share_target_rows(
    share_targets: &[screenshare::ShareTarget],
) -> Vec<ui::ShareTargetRow> {
    share_targets
        .iter()
        .map(|t| ui::ShareTargetRow {
            id: encode_share_target(t).into(),
            // Monitors and app windows share one list; make it obvious
            // which is which (raw xcap monitor names like "\\.\DISPLAY1"
            // don't read as "your screen" on their own).
            label: match t {
                screenshare::ShareTarget::Monitor(_) => {
                    format!("{} — entire screen", t.label())
                }
                screenshare::ShareTarget::Window(_) => {
                    format!("{} — app window", t.label())
                }
            }
            .into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_target_round_trips() {
        let targets = vec![
            screenshare::ShareTarget::Monitor("DISPLAY1".to_string()),
            screenshare::ShareTarget::Window("HexaTalk".to_string()),
            // Window titles can contain the same separator the encoding
            // uses -- the id must still decode to the original title.
            screenshare::ShareTarget::Window("window: tricky: title".to_string()),
            screenshare::ShareTarget::Window(String::new()),
        ];
        for target in &targets {
            let encoded = encode_share_target(target);
            let decoded = decode_share_target(&encoded);
            assert_eq!(decoded.as_ref(), Some(target), "round-trip of {encoded}");
        }
    }

    #[test]
    fn decode_rejects_unknown_prefix() {
        assert_eq!(decode_share_target(""), None);
        assert_eq!(decode_share_target("screen:1"), None);
        assert_eq!(decode_share_target("monitor"), None);
    }

    #[test]
    fn share_target_labels_distinguish_kinds() {
        let rows = share_target_rows(&[
            screenshare::ShareTarget::Monitor("DISPLAY1".to_string()),
            screenshare::ShareTarget::Window("HexaTalk".to_string()),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].label.contains("entire screen"));
        assert!(rows[1].label.contains("app window"));
        assert_ne!(rows[0].id, rows[1].id);
    }
}
