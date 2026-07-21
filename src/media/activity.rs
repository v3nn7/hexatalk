//! Local "what are you doing" detection for the profile Activity line.
//!
//! Strategy:
//!  1. Prefer the **foreground** app (what the user is looking at).
//!  2. Fall back to any running process that matches a known game/IDE map
//!     (pretty names + emoji icons).
//!  3. Otherwise show a dynamic "Active in {ProcessName}" for the foreground
//!     process, excluding OS/system noise.
//!
//! Best-effort and offline — never blocks the UI thread (call from the tick).

/// Detected activity, ready for display and for the presence heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DetectedActivity {
    /// Short display line, e.g. "Playing Minecraft" or "Active in Notion".
    pub(crate) label: String,
    /// Machine-ish id of the matched process (lowercase exe stem).
    pub(crate) app_id: String,
    /// Single emoji / glyph used as a compact "app icon" in the profile.
    pub(crate) icon: String,
}

/// Scan running apps and return the best activity to show.
pub(crate) fn detect_activity() -> Option<DetectedActivity> {
    // 1) Foreground window process (most relevant).
    if let Some(fg) = foreground_process_stem() {
        if !is_system_process(&fg) {
            if let Some(act) = activity_for_stem(&fg, /*prefer_playing*/ true) {
                return Some(act);
            }
            // Dynamic fallback: any non-system foreground app.
            return Some(DetectedActivity {
                label: format!("Active in {}", humanize_process(&fg)),
                app_id: fg.clone(),
                icon: dynamic_icon(&fg),
            });
        }
    }

    // 2) Scan all processes for a known high-priority app (game / IDE).
    let names = list_process_names();
    let mut best: Option<(i32, DetectedActivity)> = None;
    for name in &names {
        let stem = process_stem(name);
        if is_system_process(&stem) {
            continue;
        }
        if let Some((prio, act)) = known_activity(&stem) {
            if best.as_ref().map(|(p, _)| prio > *p).unwrap_or(true) {
                best = Some((prio, act));
            }
        }
    }
    best.map(|(_, a)| a)
}

fn activity_for_stem(stem: &str, _prefer_playing: bool) -> Option<DetectedActivity> {
    known_activity(stem).map(|(_, a)| a)
}

fn known_activity(stem: &str) -> Option<(i32, DetectedActivity)> {
    let (prio, kind, pretty, icon) = lookup_known(stem)?;
    let label = match kind {
        ActivityKind::Playing => format!("Playing {pretty}"),
        ActivityKind::Listening => format!("Listening to {pretty}"),
        ActivityKind::Watching => format!("Watching {pretty}"),
        ActivityKind::ActiveIn => format!("Active in {pretty}"),
    };
    Some((
        prio,
        DetectedActivity {
            label,
            app_id: stem.to_string(),
            icon: icon.to_string(),
        },
    ))
}

#[derive(Clone, Copy)]
enum ActivityKind {
    Playing,
    Listening,
    Watching,
    ActiveIn,
}

/// `(priority, kind, pretty name, emoji)` — higher priority wins.
fn lookup_known(stem: &str) -> Option<(i32, ActivityKind, &'static str, &'static str)> {
    let games: &[(&str, &str, &str)] = &[
        ("minecraft", "Minecraft", "⛏️"),
        ("javaw", "Java app", "☕"),
        ("lunarclient", "Lunar Client", "🌙"),
        ("fortnite", "Fortnite", "🪂"),
        ("fortniteclient-win64-shipping", "Fortnite", "🪂"),
        ("valorant", "VALORANT", "🎯"),
        ("valorant-win64-shipping", "VALORANT", "🎯"),
        ("leagueclient", "League of Legends", "⚔️"),
        ("league of legends", "League of Legends", "⚔️"),
        ("cs2", "Counter-Strike 2", "🔫"),
        ("csgo", "CS:GO", "🔫"),
        ("dota2", "Dota 2", "🛡️"),
        ("gta5", "GTA V", "🚗"),
        ("gtav", "GTA V", "🚗"),
        ("r5apex", "Apex Legends", "🎖️"),
        ("rocketleague", "Rocket League", "⚽"),
        ("overwatch", "Overwatch", "🦸"),
        ("destiny2", "Destiny 2", "🌌"),
        ("wow", "World of Warcraft", "🐉"),
        ("robloxplayerbeta", "Roblox", "🧱"),
        ("roblox", "Roblox", "🧱"),
        ("osu", "osu!", "🎵"),
        ("terraria", "Terraria", "⛏️"),
        ("stardewvalley", "Stardew Valley", "🌾"),
        ("amongus", "Among Us", "👾"),
        ("steam", "Steam", "🎮"),
        ("epicgameslauncher", "Epic Games", "🎮"),
    ];
    for (key, pretty, icon) in games {
        if stem == *key || stem.contains(key) {
            let prio = if matches!(*key, "javaw" | "steam") {
                40
            } else {
                100
            };
            return Some((prio, ActivityKind::Playing, pretty, icon));
        }
    }

    let work: &[(&str, &str, &str)] = &[
        ("rustrover", "RustRover", "🦀"),
        ("idea64", "IntelliJ IDEA", "💡"),
        ("idea", "IntelliJ IDEA", "💡"),
        ("webstorm", "WebStorm", "🌐"),
        ("pycharm", "PyCharm", "🐍"),
        ("clion", "CLion", "⚙️"),
        ("goland", "GoLand", "🐹"),
        ("phpstorm", "PhpStorm", "🐘"),
        ("rider", "Rider", "🏇"),
        ("datagrip", "DataGrip", "🗄️"),
        ("code", "Visual Studio Code", "💙"),
        ("cursor", "Cursor", "✨"),
        ("devenv", "Visual Studio", "🔷"),
        ("studio64", "Android Studio", "🤖"),
        ("sublime_text", "Sublime Text", "📝"),
        ("notepad++", "Notepad++", "📝"),
        ("zed", "Zed", "⚡"),
        ("nvim", "Neovim", "💚"),
        ("vim", "Vim", "💚"),
        ("blender", "Blender", "🎨"),
        ("figma", "Figma", "🎨"),
        ("photoshop", "Photoshop", "🖼️"),
        ("obs64", "OBS Studio", "📹"),
        ("obs", "OBS Studio", "📹"),
        ("unity", "Unity", "🕹️"),
        ("godot", "Godot", "🕹️"),
        ("unrealeditor", "Unreal Editor", "🕹️"),
        ("ue5editor", "Unreal Editor", "🕹️"),
        ("ue4editor", "Unreal Editor", "🕹️"),
    ];
    for (key, pretty, icon) in work {
        if stem == *key || stem.starts_with(key) {
            return Some((70, ActivityKind::ActiveIn, pretty, icon));
        }
    }

    let music: &[(&str, &str, &str)] = &[
        ("spotify", "Spotify", "🎧"),
        ("applemusic", "Apple Music", "🎧"),
        ("vlc", "VLC", "🎬"),
        ("foobar2000", "foobar2000", "🎵"),
    ];
    for (key, pretty, icon) in music {
        if stem == *key || stem.contains(key) {
            return Some((60, ActivityKind::Listening, pretty, icon));
        }
    }

    let media: &[(&str, ActivityKind, &str, &str)] = &[
        ("discord", ActivityKind::ActiveIn, "Discord", "💬"),
        ("slack", ActivityKind::ActiveIn, "Slack", "💬"),
        ("teams", ActivityKind::ActiveIn, "Microsoft Teams", "💼"),
        ("zoom", ActivityKind::ActiveIn, "Zoom", "📹"),
        ("chrome", ActivityKind::Watching, "Chrome", "🌐"),
        ("msedge", ActivityKind::Watching, "Edge", "🌐"),
        ("firefox", ActivityKind::Watching, "Firefox", "🦊"),
        ("brave", ActivityKind::Watching, "Brave", "🦁"),
        ("opera", ActivityKind::Watching, "Opera", "🌐"),
    ];
    for (key, kind, pretty, icon) in media {
        if stem == *key || stem.starts_with(key) {
            return Some((20, *kind, pretty, icon));
        }
    }

    None
}

/// Title-case a process stem: `my_cool_app` → `My Cool App`.
fn humanize_process(stem: &str) -> String {
    let cleaned = stem
        .trim_end_matches("64")
        .trim_end_matches("32")
        .replace(['_', '-', '.'], " ");
    cleaned
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn dynamic_icon(stem: &str) -> String {
    // First letter as a simple glyph when we don't know the app.
    stem.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "💻".into())
}

fn is_system_process(stem: &str) -> bool {
    const SYSTEM: &[&str] = &[
        "system",
        "idle",
        "smss",
        "csrss",
        "wininit",
        "services",
        "lsass",
        "svchost",
        "fontdrvhost",
        "dwm",
        "conhost",
        "runtimebroker",
        "searchhost",
        "startmenuexperiencehost",
        "shellexperiencehost",
        "applicationframehost",
        "textinputhost",
        "securityhealthservice",
        "securityhealthsystray",
        "sihost",
        "taskhostw",
        "ctfmon",
        "explorer", // shell — not a user "activity"
        "hexatalk",
        "talkyss",
        "powershell",
        "pwsh",
        "cmd",
        "windows terminal",
        "windowsterminal",
        "openconsole",
        "dllhost",
        "wmiprvse",
        "searchindexer",
        "spoolsv",
        "registry",
        "memory compression",
        "systemsettings",
        "lockapp",
        "widgetservice",
        "widgets",
        "msedgewebview2",
        "crashpad_handler",
        "vmmem",
        "vmmemwsl",
    ];
    let s = stem.to_ascii_lowercase();
    SYSTEM.iter().any(|x| s == *x || s.starts_with(x))
}

fn process_stem(name: &str) -> String {
    let lower = name.to_lowercase();
    let trimmed = lower
        .trim_end_matches(".exe")
        .trim_end_matches(".bin")
        .trim_end_matches(".app");
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

// ---------- OS process listing ----------

#[cfg(windows)]
fn list_process_names() -> Vec<String> {
    let output = std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .ok();
    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = csv_first_field(line) {
            names.push(name);
        }
    }
    names
}

#[cfg(windows)]
fn csv_first_field(line: &str) -> Option<String> {
    let mut chars = line.chars().peekable();
    if chars.peek() != Some(&'"') {
        let field = line.split(',').next()?.trim();
        return (!field.is_empty()).then(|| field.to_string());
    }
    chars.next();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                out.push('"');
            } else {
                break;
            }
        } else {
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Foreground window → process image name (stem).
#[cfg(windows)]
fn foreground_process_stem() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        let stem = process_stem(&path);
        if stem.is_empty() {
            None
        } else {
            Some(stem)
        }
    }
}

#[cfg(not(windows))]
fn list_process_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let path = entry.path().join("comm");
            if let Ok(s) = std::fs::read_to_string(path) {
                let s = s.trim();
                if !s.is_empty() {
                    names.push(s.to_string());
                }
            }
        }
    }
    if !names.is_empty() {
        return names;
    }
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "comm="])
        .output()
        .ok();
    let Some(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Best-effort: `/proc/self` doesn't help; try `xdotool` / `xprop` if present,
/// else None (known-list scan still works).
#[cfg(not(windows))]
fn foreground_process_stem() -> Option<String> {
    // xdotool getwindowpid $(xdotool getactivewindow) → /proc/PID/comm
    let win = std::process::Command::new("xdotool")
        .args(["getactivewindow"])
        .output()
        .ok()?;
    if !win.status.success() {
        return None;
    }
    let wid = String::from_utf8_lossy(&win.stdout).trim().to_string();
    if wid.is_empty() {
        return None;
    }
    let pid_out = std::process::Command::new("xdotool")
        .args(["getwindowpid", &wid])
        .output()
        .ok()?;
    if !pid_out.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&pid_out.stdout).trim().to_string();
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let stem = process_stem(comm.trim());
    if stem.is_empty() {
        None
    } else {
        Some(stem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_rustrover() {
        let (prio, kind, pretty, icon) = lookup_known("rustrover64").unwrap();
        assert!(prio >= 70);
        assert!(matches!(kind, ActivityKind::ActiveIn));
        assert_eq!(pretty, "RustRover");
        assert!(!icon.is_empty());
    }

    #[test]
    fn maps_minecraft() {
        let (_, kind, pretty, _) = lookup_known("minecraft").unwrap();
        assert!(matches!(kind, ActivityKind::Playing));
        assert_eq!(pretty, "Minecraft");
    }

    #[test]
    fn humanizes_unknown() {
        assert_eq!(humanize_process("my_cool_app"), "My Cool App");
    }

    #[test]
    fn filters_system() {
        assert!(is_system_process("svchost"));
        assert!(is_system_process("explorer"));
        assert!(!is_system_process("notion"));
    }
}
