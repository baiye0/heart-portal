"""Local-only WebSocket relay for the Windows connection guidance fixture."""
import base64
import hashlib
import json
import select
import socket
import struct
import sys
from pathlib import Path

root = Path(sys.argv[1])


def read_exact(connection, count):
    result = b""
    while len(result) < count:
        part = connection.recv(count - len(result))
        if not part:
            raise EOFError()
        result += part
    return result


def read_frame(connection):
    first, second = read_exact(connection, 2)
    size = second & 127
    if size == 126:
        size = struct.unpack("!H", read_exact(connection, 2))[0]
    elif size == 127:
        size = struct.unpack("!Q", read_exact(connection, 8))[0]
    if size > 1024 * 1024:
        raise ValueError("Fixture frame too large")
    mask = read_exact(connection, 4) if second & 128 else None
    payload = read_exact(connection, size)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return first & 15, payload


def send_frame(connection, opcode, payload):
    prefix = bytes([128 | opcode])
    if len(payload) < 126:
        prefix += bytes([len(payload)])
    else:
        prefix += bytes([126]) + struct.pack("!H", len(payload))
    connection.sendall(prefix + payload)


with socket.socket() as server:
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    server.settimeout(90)
    (root / "relay-ready.json").write_text(json.dumps({"port": server.getsockname()[1]}))
    connection, _ = server.accept()
    with connection:
        connection.settimeout(90)
        headers = b""
        while not headers.endswith(b"\r\n\r\n"):
            headers += read_exact(connection, 1)
        fields = dict(line.split(":", 1) for line in headers.decode().split("\r\n")[1:] if ":" in line)
        key = next(value.strip() for name, value in fields.items() if name.lower() == "sec-websocket-key")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
        connection.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
        opcode, payload = read_frame(connection)
        assert opcode == 1
        handshake = json.loads(payload)
        (root / "relay-handshake.json").write_text(json.dumps({
            "being_id": handshake["being_id"],
            "portal_name": handshake["portal_name"],
            "token_matches": handshake["loom_token"] == "fixture-token-$cash;quote'",
        }))
        send_frame(connection, 1, b'{"ok":true}')
        last_probe = ""
        try:
            while True:
                probe_path = root / "relay-probe.txt"
                probe = probe_path.read_text() if probe_path.exists() else ""
                if probe and probe != last_probe:
                    # Probes are sequential; an empty ping expects the empty
                    # pong used by Portal's existing heartbeat implementation.
                    send_frame(connection, 9, b"")
                    last_probe = probe
                if not select.select([connection], [], [], 0.1)[0]:
                    continue
                opcode, payload = read_frame(connection)
                if opcode == 9:
                    send_frame(connection, 10, payload)
                elif opcode == 10 and payload == b"" and last_probe:
                    (root / "relay-pong.txt").write_text(last_probe)
                elif opcode == 8:
                    break
        except (EOFError, ConnectionError):
            pass
