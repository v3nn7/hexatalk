//! Discord-style `@mention` parsing from plain message text. Parsed twice
//! with different goals: at render time to highlight rows that ping us, and
//! at send time (`resolve_mentions`) to store user-id metadata on the
//! message so the backend can count unread mentions for sidebar badges.
//!
//! Matching rules:
//! - a `@` only starts a mention at the beginning of the text or after a
//!   non-alphanumeric character (so `someone@example.com` never matches);
//! - candidate display names are matched case-insensitively, longest name
//!   first, so names containing spaces match greedily
//!   (`@Anna Maria` wins over `@Anna`);
//! - the name must be followed by the end of the text or a
//!   non-alphanumeric character (`@Annette` does not match `Anne`);
//! - the literal `@everyone` (any case) is always recognized and reported
//!   as `everyone: true`; callers decide whether it applies (channels and
//!   groups) or is just text (1:1 DMs).

/// One parsed mention. `start`/`end` are byte offsets into the source text
/// (exact for ASCII; names outside ASCII are still matched correctly
/// because offsets are computed on the lowercased copy, which preserves
/// byte positions for every name this app displays).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mention {
    pub(crate) start: usize,
    /// Exclusive end offset (one past the last name character).
    pub(crate) end: usize,
    /// Canonical name as it appears in the candidate list ("everyone" for
    /// the `@everyone` token).
    pub(crate) name: String,
    pub(crate) everyone: bool,
}

/// The implicit candidate always recognized alongside the member names.
const EVERYONE: &str = "everyone";

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn boundary_before(lower: &str, at: usize) -> bool {
    lower[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !is_name_char(c))
}

fn boundary_after(lower: &str, at: usize) -> bool {
    lower[at..].chars().next().is_none_or(|c| !is_name_char(c))
}

/// Parses every mention in `text` against the candidate display `names`
/// (friends, server members, group members -- whatever the caller has for
/// the active conversation). `@everyone` is detected automatically.
pub(crate) fn parse_mentions(text: &str, names: &[String]) -> Vec<Mention> {
    // Dedupe case-insensitively, drop blanks, longest name first so
    // space-containing names win over their prefixes.
    let mut cands: Vec<(String, String)> = Vec::new(); // (lowercase, display)
    let mut push = |display: &str| {
        let lower = display.to_lowercase();
        if lower.is_empty() || cands.iter().any(|(l, _)| *l == lower) {
            return;
        }
        cands.push((lower, display.to_string()));
    };
    for n in names {
        push(n);
    }
    push(EVERYONE);
    cands.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));

    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' || !boundary_before(&lower, i) {
            i += 1;
            continue;
        }
        let rest = &lower[i + 1..];
        let mut matched = false;
        for (cand_lower, cand_display) in &cands {
            if rest.starts_with(cand_lower.as_str())
                && boundary_after(&lower, i + 1 + cand_lower.len())
            {
                let end = i + 1 + cand_lower.len();
                out.push(Mention {
                    start: i,
                    end,
                    name: cand_display.clone(),
                    everyone: cand_lower == EVERYONE,
                });
                i = end;
                matched = true;
                break;
            }
        }
        if !matched {
            i += 1;
        }
    }
    out
}

/// True when `text` mentions any of `my_names` (case-insensitive).
/// `@everyone` is deliberately NOT included -- use `has_everyone`.
pub(crate) fn mentions_any(text: &str, my_names: &[String]) -> bool {
    let mine: Vec<String> = my_names.iter().map(|n| n.to_lowercase()).collect();
    parse_mentions(text, my_names)
        .iter()
        .any(|m| !m.everyone && mine.contains(&m.name.to_lowercase()))
}

/// True when `text` contains the literal `@everyone` token.
pub(crate) fn has_everyone(text: &str) -> bool {
    parse_mentions(text, &[]).iter().any(|m| m.everyone)
}

/// Finds the "@prefix" token currently being typed at the end of `input`:
/// returns `(byte offset of the '@', prefix after it)` when the last `@`
/// starts a plausible mention token. `None` hides the autocomplete popup.
pub(crate) fn active_at_token(input: &str) -> Option<(usize, String)> {
    let at = input.rfind('@')?;
    if !boundary_before(input, at) {
        return None;
    }
    let prefix = &input[at + 1..];
    // No newlines, and keep the popup from sticking around for long
    // completed sentences after an '@'.
    if prefix.contains('\n') || prefix.chars().count() > 24 {
        return None;
    }
    Some((at, prefix.to_string()))
}

/// Autocomplete suggestions for the active `@` token: candidate names
/// starting with the typed prefix (case-insensitive), alphabetical,
/// deduped, capped at `limit`. Empty prefix lists everyone. `candidates`
/// may include "everyone" when the conversation supports it.
pub(crate) fn suggest(input: &str, candidates: &[String], limit: usize) -> Vec<String> {
    let Some((_, prefix)) = active_at_token(input) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    let mut hits: Vec<String> = Vec::new();
    for c in candidates {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        if !prefix.is_empty() && !c.to_lowercase().starts_with(&prefix) {
            continue;
        }
        if hits.iter().any(|h| h.to_lowercase() == c.to_lowercase()) {
            continue;
        }
        hits.push(c.to_string());
    }
    hits.sort_by_key(|a| a.to_lowercase());
    hits.truncate(limit);
    hits
}

/// Inserts a picked suggestion: replaces the active `@prefix` token with
/// `@Name ` (trailing space included). When no token is active (popup was
/// stale), appends the mention at the end instead.
pub(crate) fn complete(input: &str, name: &str) -> String {
    if let Some((at, prefix)) = active_at_token(input) {
        let mut s = String::with_capacity(input.len() + name.len() + 2);
        s.push_str(&input[..at]);
        s.push('@');
        s.push_str(name);
        s.push(' ');
        s.push_str(&input[at + 1 + prefix.len()..]);
        s
    } else {
        let mut s = input.to_string();
        if !s.is_empty() && !s.ends_with(char::is_whitespace) {
            s.push(' ');
        }
        s.push('@');
        s.push_str(name);
        s.push(' ');
        s
    }
}

/// Resolves the mentions in `text` against `(display_name, user_id)`
/// `candidates` (friends, server members -- whatever the caller has for
/// the active conversation), returning the deduped ids of every mentioned
/// user plus whether `@everyone` appears. This is the send-time counterpart
/// of `parse_mentions`: the parser finds the @names, this maps them to the
/// Convex user ids stored as mention metadata on the message.
///
/// Name matching is case-insensitive; when two members share a display
/// name the first candidate wins (same ambiguity the highlight parser has).
/// Unknown @names are ignored -- they never become ids.
pub(crate) fn resolve_mentions(text: &str, candidates: &[(String, String)]) -> (Vec<String>, bool) {
    let names: Vec<String> = candidates.iter().map(|(name, _)| name.clone()).collect();
    let found = parse_mentions(text, &names);
    let mut ids: Vec<String> = Vec::new();
    let mut everyone = false;
    for m in found {
        if m.everyone {
            everyone = true;
            continue;
        }
        let lower = m.name.to_lowercase();
        if let Some((_, id)) = candidates
            .iter()
            .find(|(name, _)| name.to_lowercase() == lower)
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    (ids, everyone)
}

/// Short single-line excerpt of a message body for toast notifications.
pub(crate) fn snippet(body: &str, max_chars: usize) -> String {
    let one_line = body.replace('\n', " ");
    if one_line.chars().count() <= max_chars {
        one_line
    } else {
        let mut s: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn snippet_truncates_and_flattens() {
        assert_eq!(snippet("short body", 140), "short body");
        assert_eq!(snippet("line one\nline two", 140), "line one line two");
        let long = "x".repeat(200);
        let out = snippet(&long, 140);
        assert_eq!(out.chars().count(), 140);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn mentions_basic_case_insensitive() {
        let m = parse_mentions("hey @ratko check this", &names(&["Ratko"]));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "Ratko");
        assert!(!m[0].everyone);
        assert_eq!(&"hey @ratko check this"[m[0].start..m[0].end], "@ratko");
    }

    #[test]
    fn mentions_longest_first_with_spaces() {
        let both = names(&["Anna", "Anna Maria"]);
        let m = parse_mentions("@Anna Maria are you here?", &both);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "Anna Maria");
    }

    #[test]
    fn mentions_requires_trailing_boundary() {
        // "Annette" must not match candidate "Anne".
        assert!(parse_mentions("@Annette hi", &names(&["Anne"])).is_empty());
        // Punctuation after the name is fine.
        assert_eq!(parse_mentions("@Anne, hi", &names(&["Anne"])).len(), 1);
    }

    #[test]
    fn mentions_requires_leading_boundary() {
        // E-mail style tokens never match.
        assert!(parse_mentions("mail me at anne@example.com", &names(&["example"])).is_empty());
        // Start of text and after whitespace/punctuation are fine.
        assert_eq!(parse_mentions("@Anne hi", &names(&["Anne"])).len(), 1);
        assert_eq!(parse_mentions("(@Anne) hi", &names(&["Anne"])).len(), 1);
    }

    #[test]
    fn mentions_everyone_literal_any_case() {
        assert!(has_everyone("ping @everyone now"));
        assert!(has_everyone("@Everyone"));
        assert!(!has_everyone("@everyoneelse"));
        assert!(!has_everyone("no mentions here"));
        let m = parse_mentions("@everyone", &[]);
        assert_eq!(m.len(), 1);
        assert!(m[0].everyone);
    }

    #[test]
    fn mentions_multiple_and_any() {
        let m = parse_mentions("@Anna and @Bob + @everyone", &names(&["Anna", "Bob"]));
        assert_eq!(m.len(), 3);
        assert!(mentions_any("@bob!", &names(&["Bob"])));
        assert!(!mentions_any("@bob!", &names(&["Alice"])));
        // mentions_any must not fire on @everyone alone.
        assert!(!mentions_any("@everyone", &names(&["Alice"])));
    }

    #[test]
    fn mentions_skips_unknown_names() {
        assert!(parse_mentions("@ghost hello", &names(&["Anna"])).is_empty());
        // Unknown tokens don't swallow later real mentions.
        let m = parse_mentions("@ghost then @Anna", &names(&["Anna"]));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "Anna");
    }

    #[test]
    fn resolve_maps_names_to_ids() {
        let cands: Vec<(String, String)> = vec![
            ("Ratko".to_string(), "u1".to_string()),
            ("Anna Maria".to_string(), "u2".to_string()),
            ("Bob".to_string(), "u3".to_string()),
        ];
        let (ids, everyone) =
            resolve_mentions("hey @ratko and @Anna Maria, ping @everyone", &cands);
        assert_eq!(ids, vec!["u1".to_string(), "u2".to_string()]);
        assert!(everyone);
    }

    #[test]
    fn resolve_dedupes_and_skips_unknown() {
        let cands: Vec<(String, String)> = vec![("Ratko".to_string(), "u1".to_string())];
        // Same user mentioned twice -> one id; unknown names never resolve.
        let (ids, everyone) = resolve_mentions("@Ratko @ghost @RATKO", &cands);
        assert_eq!(ids, vec!["u1".to_string()]);
        assert!(!everyone);
        // No candidates at all -> no ids, but @everyone still flags.
        let (ids, everyone) = resolve_mentions("@Ratko @everyone", &[]);
        assert!(ids.is_empty());
        assert!(everyone);
    }

    #[test]
    fn resolve_first_candidate_wins_on_duplicate_names() {
        let cands: Vec<(String, String)> = vec![
            ("Sam".to_string(), "u1".to_string()),
            ("Sam".to_string(), "u2".to_string()),
        ];
        let (ids, _) = resolve_mentions("@Sam hi", &cands);
        assert_eq!(ids, vec!["u1".to_string()]);
    }

    #[test]
    fn suggest_active_token() {
        assert_eq!(active_at_token("hello @ra"), Some((6, "ra".to_string())));
        assert_eq!(active_at_token("@"), Some((0, String::new())));
        assert_eq!(active_at_token("no at here"), None);
        assert_eq!(active_at_token("mail@x"), None);
    }

    #[test]
    fn suggest_filters_and_caps() {
        let cands = names(&["Ratko", "Rita", "Anna", "everyone"]);
        assert_eq!(suggest("@r", &cands, 8), names(&["Ratko", "Rita"]));
        assert_eq!(
            suggest("@", &cands, 8),
            names(&["Anna", "everyone", "Ratko", "Rita"])
        );
        assert_eq!(suggest("@", &cands, 2), names(&["Anna", "everyone"]));
        assert_eq!(suggest("@zzz", &cands, 8), Vec::<String>::new());
        assert_eq!(suggest("plain text", &cands, 8), Vec::<String>::new());
    }

    #[test]
    fn complete_replaces_token() {
        assert_eq!(complete("hi @ra", "Ratko"), "hi @Ratko ");
        assert_eq!(complete("@ev", "everyone"), "@everyone ");
        assert_eq!(complete("@", "Anna Maria"), "@Anna Maria ");
        // No active token: append.
        assert_eq!(complete("hello", "Ratko"), "hello @Ratko ");
        // Text after the last '@' is still a (space-containing) prefix, so
        // it gets replaced -- the popup only opens when a candidate matches
        // that prefix anyway.
        assert_eq!(complete("@Ratko done", "Anna"), "@Anna ");
        // Prefix way too long to be a name: token is stale, append instead.
        let long_tail = format!("@{}", "x".repeat(40));
        assert_eq!(complete(&long_tail, "Anna"), format!("{long_tail} @Anna "));
    }
}
