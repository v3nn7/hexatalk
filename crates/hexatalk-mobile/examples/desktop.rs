//! Desktop preview of the mobile Slint UI.
//! `cargo run --example desktop`

fn main() -> Result<(), slint::PlatformError> {
    let _ = env_logger::try_init();
    hexatalk_mobile::start()
}
