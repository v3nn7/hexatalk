"""Signs a release exe with the ed25519 release key, for scripts/release.ps1.

Reads the private key from the RELEASE_SIGNING_KEY_HEX environment variable
(never from argv, so it never shows up in a process listing) -- a 64-char
hex string, the 32-byte private seed generated once offline (see the
"Release signing procedure" doc comment in src/update_check.rs).

Usage: python sign_release.py <exe_path> <sig_path>
"""

import os
import sys

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: sign_release.py <exe_path> <sig_path>", file=sys.stderr)
        return 2

    key_hex = os.environ.get("RELEASE_SIGNING_KEY_HEX", "")
    if not key_hex:
        print("RELEASE_SIGNING_KEY_HEX is not set", file=sys.stderr)
        return 1

    exe_path, sig_path = sys.argv[1], sys.argv[2]
    key = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(key_hex))
    with open(exe_path, "rb") as f:
        data = f.read()
    signature = key.sign(data)
    with open(sig_path, "wb") as f:
        f.write(signature)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
