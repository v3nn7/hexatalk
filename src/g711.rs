//! G.711 mu-law (PCMU) codec: pure Rust, no external C dependency.
//! Ported from the classic table-free reference implementation
//! (Sun/CCITT g711.c, as used historically in BSD and WebRTC codebases).

const BIAS: i32 = 0x84;
const CLIP: i32 = 8159;
const SIGN_BIT: u8 = 0x80;
const QUANT_MASK: i32 = 0xF;
const SEG_SHIFT: u8 = 4;
const SEG_MASK: u8 = 0x70;

const SEG_UEND: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];

fn search(val: i32, table: &[i32; 8]) -> i32 {
    for (i, &entry) in table.iter().enumerate() {
        if val <= entry {
            return i as i32;
        }
    }
    8
}

pub fn linear_to_ulaw(pcm_val: i16) -> u8 {
    let mut pcm_val = (pcm_val as i32) >> 2;
    let mask = if pcm_val < 0 {
        pcm_val = -pcm_val;
        0x7F
    } else {
        0xFF
    };
    if pcm_val > CLIP {
        pcm_val = CLIP;
    }
    pcm_val += BIAS >> 2;

    let seg = search(pcm_val, &SEG_UEND);
    if seg >= 8 {
        (0x7F ^ mask) as u8
    } else {
        let uval = ((seg << 4) | ((pcm_val >> (seg + 1)) & 0xF)) as u8;
        uval ^ (mask as u8)
    }
}

pub fn ulaw_to_linear(u_val: u8) -> i16 {
    let u_val = !u_val;
    let mut t: i32 = ((u_val as i32) & QUANT_MASK) << 3;
    t += BIAS;
    t <<= ((u_val & SEG_MASK) >> SEG_SHIFT) as i32;
    let result = if (u_val & SIGN_BIT) != 0 {
        BIAS - t
    } else {
        t - BIAS
    };
    result.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

// Batch counterpart to `decode`; kept for API symmetry (the live capture path
// encodes sample-by-sample inline for lower latency).
#[allow(dead_code)]
pub fn encode(samples: &[i16]) -> Vec<u8> {
    samples.iter().copied().map(linear_to_ulaw).collect()
}

pub fn decode(bytes: &[u8]) -> Vec<i16> {
    bytes.iter().copied().map(ulaw_to_linear).collect()
}
