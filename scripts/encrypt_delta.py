"""Encrypt / decrypt HexaTalk qbsdiff delta patches (HTD1 frame).

Wire format (binary):
  magic  : b"HTD1"          (4 bytes)
  nonce  : 12 random bytes  (AES-GCM)
  body   : ciphertext || 16-byte GCM tag over the raw qbsdiff patch
  [sig]  : optional 64-byte ed25519 signature of the *target* HexaTalk.exe
           (appended AFTER the HTD1 frame so R2 only needs version.txt + delta)

The AES-256 key is read from RELEASE_DELTA_KEY_HEX (64 hex chars = 32 bytes).
That value MUST match the key baked into the client as UPDATE_DELTA_KEY_B64
(see build.rs / src/update_check.rs).

Usage:
  python encrypt_delta.py encrypt <plain.delta> <out.delta> [exe.sig]
  python encrypt_delta.py decrypt <enc.delta>  <out.delta>
"""

from __future__ import annotations

import os
import sys

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

MAGIC = b"HTD1"
NONCE_LEN = 12
SIG_LEN = 64


def _load_key() -> bytes:
    key_hex = os.environ.get("RELEASE_DELTA_KEY_HEX", "").strip()
    if not key_hex:
        # Default matches the baked UPDATE_DELTA_KEY_B64 in build.rs.
        # Prefer setting RELEASE_DELTA_KEY_HEX explicitly in production shells.
        key_hex = "c14e32b623c2318ee6bf67cc85d88ba546a220b269e1af52487eec20274a554f"
    try:
        key = bytes.fromhex(key_hex)
    except ValueError as exc:
        raise SystemExit(f"RELEASE_DELTA_KEY_HEX is not valid hex: {exc}") from exc
    if len(key) != 32:
        raise SystemExit(
            f"RELEASE_DELTA_KEY_HEX must be 32 bytes (64 hex chars), got {len(key)}"
        )
    return key


def encrypt_file(src: str, dst: str, sig_path: str | None = None) -> None:
    plain = open(src, "rb").read()
    if plain.startswith(MAGIC):
        raise SystemExit(f"{src} already looks HTD1-encrypted")
    key = _load_key()
    nonce = os.urandom(NONCE_LEN)
    ct = AESGCM(key).encrypt(nonce, plain, None)
    frame = MAGIC + nonce + ct

    sig = b""
    if sig_path:
        sig = open(sig_path, "rb").read()
        if len(sig) != SIG_LEN:
            raise SystemExit(
                f"signature must be exactly {SIG_LEN} raw bytes, got {len(sig)} from {sig_path}"
            )

    with open(dst, "wb") as f:
        f.write(frame + sig)


def decrypt_file(src: str, dst: str) -> None:
    blob = open(src, "rb").read()
    if len(blob) < 4 + NONCE_LEN + 16 or not blob.startswith(MAGIC):
        raise SystemExit(f"{src} is not an HTD1 encrypted delta")
    key = _load_key()

    def try_decrypt(frame: bytes) -> bytes | None:
        if len(frame) < 4 + NONCE_LEN + 16 or not frame.startswith(MAGIC):
            return None
        nonce = frame[4 : 4 + NONCE_LEN]
        ct = frame[4 + NONCE_LEN :]
        try:
            return AESGCM(key).decrypt(nonce, ct, None)
        except Exception:
            return None

    # Prefer full blob (legacy HTD1 without trailing sig). If AEAD fails and
    # the blob ends with a 64-byte trailer, strip it and retry (new format).
    plain = try_decrypt(blob)
    if plain is None and len(blob) > SIG_LEN:
        plain = try_decrypt(blob[:-SIG_LEN])
    if plain is None:
        raise SystemExit("decrypt failed (wrong key, corrupt delta, or bad framing)")

    with open(dst, "wb") as f:
        f.write(plain)


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[1] not in ("encrypt", "decrypt"):
        print(
            "usage: encrypt_delta.py encrypt <in.delta> <out.delta> [exe.sig]\n"
            "       encrypt_delta.py decrypt <in.delta> <out.delta>",
            file=sys.stderr,
        )
        return 2
    op, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
    sig_path = sys.argv[4] if len(sys.argv) > 4 else None
    if op == "decrypt" and sig_path:
        print("decrypt does not take a signature argument", file=sys.stderr)
        return 2
    if not os.path.isfile(src):
        print(f"input not found: {src}", file=sys.stderr)
        return 1
    if op == "encrypt":
        if sig_path and not os.path.isfile(sig_path):
            print(f"signature not found: {sig_path}", file=sys.stderr)
            return 1
        encrypt_file(src, dst, sig_path)
    else:
        decrypt_file(src, dst)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
