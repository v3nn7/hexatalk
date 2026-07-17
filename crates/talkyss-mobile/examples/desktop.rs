//! Desktop preview of the mobile UI (Windows / Linux / macOS).
//!
//! ```powershell
//! cd crates\talkyss-mobile
//! cargo run --example desktop
//! ```

fn main() -> eframe::Result {
    talkyss_mobile::run_native("Talkyss Mobile")
}
