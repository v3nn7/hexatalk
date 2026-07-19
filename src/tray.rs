//! System tray icon: keeps Talkyss reachable after the main window is
//! hidden, mirroring how Discord/Slack sit in the tray instead of fully
//! quitting when the close button is clicked. `tray-icon` needs a live
//! event loop on whichever thread owns the icon -- a Win32 message loop on
//! Windows, a GTK/glib main loop on Linux -- and iced already owns its own
//! loop on the main thread, so this runs on its own dedicated OS thread
//! (same pattern as the mic-capture/playback threads in call.rs), polling
//! its two event channels and forwarding them to the iced app through a
//! plain channel.
//!
//! Not implemented on macOS/other platforms yet -- `spawn` there is a no-op
//! and the app just quits normally when the window is closed (see
//! `exit_on_close_request` in main.rs).

use tokio::sync::mpsc::UnboundedSender;
#[cfg(any(windows, target_os = "linux"))]
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
#[cfg(any(windows, target_os = "linux"))]
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone)]
pub(crate) enum TrayEvent {
    Show,
    Quit,
    /// The tray icon came up successfully — the window can safely hide to
    /// it on close, since "Show"/"Quit" will actually be reachable.
    Ready,
    /// The tray icon failed to spawn (menu/icon build error, no
    /// Shell_NotifyIcon slot, no StatusNotifierItem host on Linux, ...).
    /// Without this signal the app would silently hide on close with no
    /// way back — closing should just quit instead. Carries the reason so
    /// it can be surfaced to the user instead of failing silently.
    Unavailable(String),
}

/// A sharp square glyph in muted emerald (#30, 0x72, 0x52). Generated at
/// runtime instead of shipping an .ico asset -- good enough for a 32x32 mark.
#[cfg(any(windows, target_os = "linux"))]
fn build_icon() -> Result<Icon, String> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    // Hard square with a 2px inset so it doesn't bleed into the tray edge.
    let inset = 3u32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            if x >= inset && x < SIZE - inset && y >= inset && y < SIZE - inset {
                let idx = ((y * SIZE + x) * 4) as usize;
                rgba[idx] = 0x30;
                rgba[idx + 1] = 0x72;
                rgba[idx + 2] = 0x52;
                rgba[idx + 3] = 0xFF;
            }
        }
    }
    // `from_rgba` validates the dimensions/length; fallibly handle it so a
    // failure can't panic the tray OS thread and make the app unreachable.
    Icon::from_rgba(rgba, SIZE, SIZE).map_err(|err| format!("tray icon bitmap: {err}"))
}

/// Builds the "Show Talkyss" / "Quit" menu shared by both backends.
#[cfg(any(windows, target_os = "linux"))]
fn build_menu() -> Result<Menu, String> {
    let show_item = MenuItem::with_id("show", "Show Talkyss", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    let menu = Menu::new();
    menu.append(&show_item)
        .map_err(|err| format!("tray menu (show item): {err}"))?;
    menu.append(&quit_item)
        .map_err(|err| format!("tray menu (quit item): {err}"))?;
    Ok(menu)
}

/// Spawns the tray icon on its own OS thread and forwards Show/Quit events
/// through `event_tx` for as long as the process runs. Must only be called
/// once -- callers are expected to spawn it from inside a subscription
/// stream, which iced guarantees only starts once per subscription id (see
/// the same assumption already relied on by call.rs's engine subscription).
#[cfg(windows)]
pub(crate) fn spawn(event_tx: UnboundedSender<TrayEvent>) {
    std::thread::spawn(move || {
        eprintln!("[tray] thread started");
        // A panic on this thread would otherwise vanish silently -- no
        // Ready/Unavailable ever sent, no visible icon, no crash -- exactly
        // the "app just disappears with no tray icon" symptom this is meant
        // to catch. Report it loudly instead.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tray_thread(event_tx.clone());
        }));
        if let Err(panic) = result {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            eprintln!("[tray] thread PANICKED: {msg}");
            let _ = event_tx.send(TrayEvent::Unavailable(format!(
                "tray thread panicked: {msg}"
            )));
        }
    });
}

#[cfg(windows)]
fn run_tray_thread(event_tx: UnboundedSender<TrayEvent>) {
    let menu = match build_menu() {
        Ok(menu) => {
            eprintln!("[tray] menu built OK");
            menu
        }
        Err(reason) => {
            eprintln!("[tray] menu build FAILED: {reason}");
            let _ = event_tx.send(TrayEvent::Unavailable(reason));
            return;
        }
    };
    let icon = match build_icon() {
        Ok(icon) => {
            eprintln!("[tray] icon bitmap built OK");
            icon
        }
        Err(reason) => {
            eprintln!("[tray] icon bitmap build FAILED: {reason}");
            let _ = event_tx.send(TrayEvent::Unavailable(reason));
            return;
        }
    };

    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("Talkyss")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build();
    let _tray_icon = match tray_icon {
        Ok(tray_icon) => {
            eprintln!("[tray] TrayIconBuilder::build() OK");
            tray_icon
        }
        Err(err) => {
            eprintln!("[tray] TrayIconBuilder::build() FAILED: {err}");
            let _ = event_tx.send(TrayEvent::Unavailable(format!(
                "tray icon registration failed: {err}"
            )));
            return;
        }
    };
    if event_tx.send(TrayEvent::Ready).is_err() {
        eprintln!("[tray] Ready send failed (receiver dropped) — exiting thread");
        return;
    }
    eprintln!("[tray] Ready sent, entering message loop");

    let tray_rx = TrayIconEvent::receiver();
    let menu_rx = MenuEvent::receiver();

    // Required for the tray icon's hidden window to actually receive
    // its Shell_NotifyIcon callback messages; also doubles as our
    // polling cadence for the two event channels above.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        loop {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if let Ok(event) = tray_rx.try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. })
                    && event_tx.send(TrayEvent::Show).is_err()
                {
                    return;
                }
            }
            if let Ok(event) = menu_rx.try_recv() {
                if event.id == "show" {
                    if event_tx.send(TrayEvent::Show).is_err() {
                        return;
                    }
                } else if event.id == "quit" {
                    let _ = event_tx.send(TrayEvent::Quit);
                    return;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Linux backend: tray-icon's StatusNotifierItem/libappindicator support
/// needs a running GTK main loop on the same thread that builds the icon,
/// unlike Win32's PeekMessage-style polling -- so instead of a manual poll
/// loop, the two event channels are drained from a glib timeout source that
/// runs inside `gtk::main()` itself.
///
/// Requires a StatusNotifierItem host to actually show anything -- KDE and
/// XFCE provide one out of the box, GNOME does not unless an extension like
/// "AppIndicator and KStatusNotifierItem Support" is installed. That's a
/// desktop-environment gap, not something this code can work around.
#[cfg(target_os = "linux")]
pub(crate) fn spawn(event_tx: UnboundedSender<TrayEvent>) {
    std::thread::spawn(move || {
        if let Err(err) = gtk::init() {
            let _ = event_tx.send(TrayEvent::Unavailable(format!("gtk::init failed: {err}")));
            return;
        }

        let menu = match build_menu() {
            Ok(menu) => menu,
            Err(reason) => {
                let _ = event_tx.send(TrayEvent::Unavailable(reason));
                return;
            }
        };
        let icon = match build_icon() {
            Ok(icon) => icon,
            Err(reason) => {
                let _ = event_tx.send(TrayEvent::Unavailable(reason));
                return;
            }
        };

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("Talkyss")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build();
        let _tray_icon = match tray_icon {
            Ok(tray_icon) => tray_icon,
            Err(err) => {
                let _ = event_tx.send(TrayEvent::Unavailable(format!(
                    "tray icon registration failed: {err}"
                )));
                return;
            }
        };
        if event_tx.send(TrayEvent::Ready).is_err() {
            return;
        }

        let tray_rx = TrayIconEvent::receiver();
        let menu_rx = MenuEvent::receiver();

        glib::source::timeout_add_local(std::time::Duration::from_millis(50), move || {
            if let Ok(event) = tray_rx.try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. })
                    && event_tx.send(TrayEvent::Show).is_err()
                {
                    gtk::main_quit();
                    return glib::ControlFlow::Break;
                }
            }
            if let Ok(event) = menu_rx.try_recv() {
                if event.id == "show" {
                    if event_tx.send(TrayEvent::Show).is_err() {
                        gtk::main_quit();
                        return glib::ControlFlow::Break;
                    }
                } else if event.id == "quit" {
                    let _ = event_tx.send(TrayEvent::Quit);
                    gtk::main_quit();
                    return glib::ControlFlow::Break;
                }
            }
            glib::ControlFlow::Continue
        });

        gtk::main();
        // `_tray_icon` is dropped here, once `gtk::main()` returns -- keeps
        // the icon alive for the whole time the loop (and thus the process)
        // is running.
    });
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn spawn(event_tx: UnboundedSender<TrayEvent>) {
    let _ = event_tx.send(TrayEvent::Unavailable(
        "not implemented on this platform".into(),
    ));
}
