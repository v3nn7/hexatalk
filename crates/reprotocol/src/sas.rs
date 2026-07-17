//! Short Authentication String (emoji) for out-of-band verification.

use sha2::{Digest, Sha256};

/// Signal-style emoji list (64 entries → 6 bits each).
const EMOJIS: &[&str] = &[
    "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🐔",
    "🐧", "🐦", "🐤", "🦆", "🦅", "🦉", "🦇", "🐺", "🐗", "🐴", "🦄", "🐝", "🐛", "🦋", "🐌", "🐞",
    "🐜", "🦟", "🦗", "🕷", "🦂", "🐢", "🐍", "🦎", "🦖", "🦕", "🐙", "🦑", "🦐", "🦞", "🦀", "🐡",
    "🐠", "🐟", "🐬", "🐳", "🐋", "🦈", "🐊", "🐅", "🐆", "🦓", "🦍", "🦧", "🐘", "🦛", "🦏", "🐪",
];

/// Derive a short authentication string from the Noise handshake hash (and optional labels).
///
/// Both peers with the same handshake material compute the same emoji sequence.
/// Users compare visually / by voice (e.g. phone call) to detect MITM.
pub fn sas_emojis(handshake_hash: &[u8], count: usize) -> String {
    let count = count.clamp(1, 12);
    let mut hasher = Sha256::new();
    hasher.update(b"peerseal-v1-sas");
    hasher.update(handshake_hash);
    let mut seed = hasher.finalize().to_vec();
    let mut out = String::new();
    for i in 0..count {
        if i > 0 {
            // stretch
            let mut h = Sha256::new();
            h.update(&seed);
            h.update([i as u8]);
            seed = h.finalize().to_vec();
        }
        let idx = (seed[0] as usize) % EMOJIS.len();
        out.push_str(EMOJIS[idx]);
    }
    out
}

/// Compact hex SAS (alternative to emoji) — first `n` bytes of hash as hex.
pub fn sas_hex(handshake_hash: &[u8], n_bytes: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"peerseal-v1-sas-hex");
    hasher.update(handshake_hash);
    let dig = hasher.finalize();
    let n = n_bytes.clamp(2, 16);
    dig[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Numeric SAS: groups of digits for reading aloud.
pub fn sas_numbers(handshake_hash: &[u8], groups: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"peerseal-v1-sas-num");
    hasher.update(handshake_hash);
    let dig = hasher.finalize();
    let groups = groups.clamp(1, 8);
    let mut parts = Vec::with_capacity(groups);
    for i in 0..groups {
        let off = (i * 2) % 30;
        let n = u16::from_be_bytes([dig[off], dig[off + 1]]) % 10000;
        parts.push(format!("{n:04}"));
    }
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_stable() {
        let h = [7u8; 32];
        assert_eq!(sas_emojis(&h, 5), sas_emojis(&h, 5));
        assert_ne!(sas_emojis(&h, 5), sas_emojis(&[8u8; 32], 5));
        assert_eq!(sas_emojis(&h, 5).chars().count(), 5);
    }

    #[test]
    fn numbers_format() {
        let s = sas_numbers(&[1u8; 32], 3);
        assert_eq!(s.matches('-').count(), 2);
    }
}
