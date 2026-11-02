#!/usr/bin/env python3
"""The host's side of DR-T13 (SDD 25): what vsock-test talks to.

    vsock-peer.py LOG UDS

The guest's vsock is the vhost-user back end moruna-vhost-vsock (SDD 30 f.11), whose host
side is Unix sockets: a guest connection to host port P reaches the socket UDS_P, and a host
connection to UDS that writes "CONNECT <port>\\n" and reads "OK <host port>\\n" reaches guest
port <port>.

Listens at UDS_5000 for the guest, answers its line with "pong: " and the line, then
connects to the guest's port 5001 (retrying until the guest listens), sends a line and
reads the answer. Writes a verdict line per exchange to LOG, which check-boot.sh judges,
and exits 0 only if both exchanges happened as described.
"""

import os
import socket
import sys
import time

TIMEOUT = 600


def read_line(conn):
    got = b""
    while not got.endswith(b"\n"):
        chunk = conn.recv(4096)
        if not chunk:
            raise ConnectionError(f"the guest closed after {got!r}")
        got += chunk
    return got


def connect_guest(uds, port, deadline):
    """A stream to the guest's `port` through the back end, retrying until the guest listens
    or `deadline` (time.monotonic) passes: the back end ends the stream without an answer
    while nothing listens there."""
    while True:
        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.settimeout(TIMEOUT)
        try:
            conn.connect(uds)
            conn.sendall(f"CONNECT {port}\n".encode())
            answer = b""
            while not answer.endswith(b"\n"):
                chunk = conn.recv(1)
                if not chunk:
                    raise ConnectionRefusedError(f"the guest's port {port} took no connection")
                answer += chunk
            if not answer.startswith(b"OK "):
                raise ConnectionRefusedError(f"the back end answered {answer!r}")
            return conn
        except OSError:
            conn.close()
            if time.monotonic() > deadline:
                raise
            time.sleep(0.5)


def main():
    log_path, uds = sys.argv[1], sys.argv[2]
    with open(log_path, "w", buffering=1) as log:
        try:
            path = f"{uds}_5000"
            if os.path.exists(path):
                os.unlink(path)
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(path)
            server.listen(1)
            server.settimeout(TIMEOUT)
            conn, _ = server.accept()
            conn.settimeout(TIMEOUT)
            line = read_line(conn)
            conn.sendall(b"pong: " + line)
            log.write(
                f"vsock-peer: ok: the guest connected to port 5000 and sent "
                f"{line.decode().strip()!r}\n"
            )
            conn.close()
            server.close()
            os.unlink(path)
            client = connect_guest(uds, 5001, time.monotonic() + TIMEOUT)
            client.sendall(b"hello from the host\n")
            answer = read_line(client)
            client.close()
            log.write(
                f"vsock-peer: ok: connected to the guest's port 5001 and it answered "
                f"{answer.decode().strip()!r}\n"
            )
        except Exception as e:  # the verdict line names the failure, whatever it is
            log.write(f"vsock-peer: FAIL: {type(e).__name__}: {e}\n")
            sys.exit(1)


if __name__ == "__main__":
    main()
