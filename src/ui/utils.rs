//! Small pure helper functions shared across the Slint UI code (date/time
//! formatting, presence/online helpers, ...). Split out of the old
//! iced-only `style.rs` so the crate no longer depends on `iced`.

use chrono::TimeZone;

pub(super) fn format_time(sent_at_ms: i64) -> String {
    match chrono::Local.timestamp_millis_opt(sent_at_ms) {
        chrono::LocalResult::Single(dt) => dt.format("%H:%M").to_string(),
        _ => String::new(),
    }
}

pub(super) fn format_relative_time(sent_at_ms: i64) -> String {
    if sent_at_ms <= 0 {
        return String::new();
    }
    let now = chrono::Utc::now().timestamp_millis();
    let delta_ms = (now - sent_at_ms).max(0);
    let mins = delta_ms / 60_000;
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
    match chrono::Local.timestamp_millis_opt(sent_at_ms) {
        chrono::LocalResult::Single(dt) => dt.format("%b %-d").to_string(),
        _ => String::new(),
    }
}

pub(super) fn format_day(sent_at_ms: i64) -> String {
    let now = chrono::Local::now();
    match chrono::Local.timestamp_millis_opt(sent_at_ms) {
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

pub(crate) fn typing_label(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [a] => Some(format!("{a} is typing…")),
        [a, b] => Some(format!("{a} and {b} are typing…")),
        _ => Some("Several people are typing…".to_string()),
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

pub(crate) fn friend_request_privacy_label(current: &str) -> &'static str {
    match current {
        "mutual_servers" => "Shared servers only",
        "nobody" => "Nobody",
        _ => "Everyone",
    }
}

pub(crate) fn next_friend_request_privacy(current: &str) -> &'static str {
    match current {
        "everyone" => "mutual_servers",
        "mutual_servers" => "nobody",
        _ => "everyone",
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
