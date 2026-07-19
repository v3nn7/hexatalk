//! HexaTalk mobile — native Rust UI (egui/eframe).
//!
//! Desktop preview: `cargo run --example desktop` (from this crate)
//! Android APK: see crates/hexatalk-mobile/README.md (`cargo apk build --release`)

mod app;
mod clipboard_util;
mod convex_api;

pub use app::HexaTalkApp;

/// Shared entry used by desktop `main` and Android `android_main`.
pub fn run_native(title: &str) -> eframe::Result {
    #[cfg(not(target_os = "android"))]
    let _ = env_logger::try_init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([390.0, 780.0])
            .with_min_inner_size([320.0, 480.0])
            .with_title(title),
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(|cc| Ok(Box::new(HexaTalkApp::new(cc)))),
    )
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    let options = eframe::NativeOptions {
        android_app: Some(app),
        viewport: egui::ViewportBuilder::default().with_title("HexaTalk"),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "HexaTalk",
        options,
        Box::new(|cc| Ok(Box::new(HexaTalkApp::new(cc)))),
    );
}
