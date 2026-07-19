//! Desktop preview of the mobile UI (Windows / Linux / macOS).
//!
//! ```powershell
//! cd crates\hexatalk-mobile
//! cargo run --example desktop
//! ```

fn main() -> eframe::Result {
    hexatalk_mobile::run_native("HexaTalk Mobile")
}
