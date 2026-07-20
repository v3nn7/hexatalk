//! Runtime side of the baked-in-secret obfuscation (see `build.rs`:
//! `emit_obfuscated_secrets`). Deployment values — Convex URL, TURN
//! credentials, peerseal relay, update endpoints, release public key — are
//! compiled into the exe as XOR-obfuscated byte arrays so they don't show
//! up as plain strings in the binary. This is obfuscation, not encryption:
//! it defeats `strings.exe` and casual inspection, not a determined
//! reverser. Runtime environment variables still override every value here
//! (see the call sites), so `.env.local` next to the exe keeps working.

use std::sync::LazyLock;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/obf_secrets.rs"));
}

/// Mirrors `obfuscate` in build.rs — keep the two in sync.
fn deobfuscate(bytes: &[u8]) -> String {
    let key = generated::OBF_KEY;
    let plain: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()] ^ (i as u8).wrapping_mul(0x5A))
        .collect();
    String::from_utf8(plain).unwrap_or_default()
}

macro_rules! baked {
    ($fn_name:ident, $generated:ident) => {
        pub(crate) fn $fn_name() -> &'static str {
            static VALUE: LazyLock<String> = LazyLock::new(|| deobfuscate(generated::$generated));
            VALUE.as_str()
        }
    };
}

baked!(convex_url, OBF_CONVEX_URL);
baked!(turn_url, OBF_TURN_URL);
baked!(turn_username, OBF_TURN_USERNAME);
baked!(turn_credential, OBF_TURN_CREDENTIAL);
baked!(peerseal_relay, OBF_PEERSEAL_RELAY);
baked!(update_version_url, OBF_UPDATE_VERSION_URL);
baked!(update_download_url, OBF_UPDATE_DOWNLOAD_URL);
baked!(update_signature_url, OBF_UPDATE_SIGNATURE_URL);
baked!(update_public_key_b64, OBF_UPDATE_PUBLIC_KEY_B64);
baked!(update_delta_base_url, OBF_UPDATE_DELTA_BASE_URL);
baked!(update_delta_key_b64, OBF_UPDATE_DELTA_KEY_B64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_values_roundtrip() {
        // The relay always has a non-empty default baked in (build.rs falls
        // back to the production host), so it doubles as a canary that the
        // build.rs/obf.rs XOR schemes are in sync.
        assert!(peerseal_relay().contains("railway.app") || !peerseal_relay().is_empty());
        assert!(update_version_url().starts_with("https://"));
        assert!(update_download_url().starts_with("https://"));
        assert!(update_signature_url().ends_with(".sig"));
        // 32 raw bytes base64-encoded = 44 chars with padding.
        assert_eq!(update_public_key_b64().len(), 44);
        assert!(update_delta_base_url().starts_with("https://") && update_delta_base_url().ends_with('/'));
        assert_eq!(update_delta_key_b64().len(), 44);
    }
}
