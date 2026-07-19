"""Raw-socket WebSocket smoke test for talkyss-relay (no external deps).

Verifies the exact protocol the peerseal client expects:
  - plain HTTP endpoints (/healthz, /v1/limits)
  - room join status lines (waiting / peer joined / peer left)
  - binary frame forwarding between room peers
  - token rejection with a ❌ status line
"""
import base64
import os
import socket
import struct
import sys

HOST, PORT = "127.0.0.1", int(os.environ.get("RELAY_PORT", "19099"))
TOKEN = os.environ.get("RELAY_TOKEN", "sekret")

OP_TEXT, OP_BIN, OP_CLOSE, OP_PING, OP_PONG = 0x1, 0x2, 0x8, 0x9, 0xA


def http_get(path):
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(f"GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode())
    data = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
    s.close()
    return data.decode(errors="replace")


class WS:
    def __init__(self, room, token):
        self.sock = socket.create_connection((HOST, PORT), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /v1/room/{room}?token={token} HTTP/1.1\r\n"
            f"Host: {HOST}:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += self.sock.recv(4096)
        assert b"101" in head.split(b"\r\n")[0], head
        self.buf = b""
        self.sock.settimeout(10)

    def send(self, opcode, payload: bytes):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            hdr = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 65536:
            hdr = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            hdr = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(hdr + mask + masked)

    def send_text(self, s):
        self.send(OP_TEXT, s.encode())

    def send_binary(self, b):
        self.send(OP_BIN, b)

    def close(self):
        try:
            self.send(OP_CLOSE, b"")
        except OSError:
            pass
        self.sock.close()

    def recv(self):
        """Returns (opcode, payload). Auto-replies to pings."""
        while True:
            b1, b2 = self._read(2)
            opcode = b1 & 0x0F
            n = b2 & 0x7F
            if n == 126:
                (n,) = struct.unpack("!H", self._read(2))
            elif n == 127:
                (n,) = struct.unpack("!Q", self._read(8))
            if b2 & 0x80:
                mask = self._read(4)
                payload = bytes(x ^ mask[i % 4] for i, x in enumerate(self._read(n)))
            else:
                payload = self._read(n)
            if opcode == OP_PING:
                self.send(OP_PONG, payload)
                continue
            return opcode, payload

    def recv_text(self):
        op, p = self.recv()
        assert op == OP_TEXT, f"expected text, got opcode {op}: {p!r}"
        return p.decode()

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("eof")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out


def main():
    fails = []

    def check(name, cond, detail=""):
        print(("PASS" if cond else "FAIL"), name, detail)
        if not cond:
            fails.append(name)

    # --- plain HTTP endpoints ---
    body = http_get("/healthz")
    check("healthz", "200 OK" in body and "ok" in body)
    body = http_get("/v1/limits")
    check("limits json", '"max_binary_frame"' in body and '"max_peers_per_room"' in body)

    # --- room flow ---
    a = WS("pokoj1", TOKEN)
    line = a.recv_text()
    check("first peer waits", line.startswith("⏳"), repr(line))

    b = WS("pokoj1", TOKEN)
    line_b = b.recv_text()
    check("second peer ready", line_b.startswith("✅") and "peer joined" in line_b, repr(line_b))
    line_a = a.recv_text()
    check("first peer notified", line_a.startswith("✅") and "peer joined" in line_a, repr(line_a))

    payload = bytes(range(256)) * 100  # 25.6 KiB of binary "ciphertext"
    a.send_binary(payload)
    op, got = b.recv()
    check("binary forwarded", op == OP_BIN and got == payload, f"{len(got)} bytes")

    # third peer (group voice): everyone gets the join notice, fanout works
    c = WS("pokoj1", TOKEN)
    line_c = c.recv_text()
    check("third peer ready", line_c.startswith("✅") and "3 peers" in line_c, repr(line_c))
    a.recv_text()
    b.recv_text()
    c.send_binary(b"glos-grupowy")
    check("fanout to A", a.recv() == (OP_BIN, b"glos-grupowy"))
    check("fanout to B", b.recv() == (OP_BIN, b"glos-grupowy"))

    c.close()
    line = a.recv_text()
    check("peer left notice", line.startswith("⏳") and "peer left" in line, repr(line))
    b.recv_text()

    a.close()
    b.close()

    # --- bad token rejected with ❌ ---
    bad = WS("pokoj1", "zly-token")
    line = bad.recv_text()
    check("invalid token", line.startswith("❌"), repr(line))
    bad.close()

    if fails:
        print(f"\n{len(fails)} FAILURES: {fails}")
        sys.exit(1)
    print("\nALL SMOKE TESTS PASSED")


if __name__ == "__main__":
    main()
