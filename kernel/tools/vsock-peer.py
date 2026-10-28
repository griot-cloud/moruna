#!/usr/bin/env python3
"""The host's side of DR-T13 (SDD 25): what vsock-test talks to.

    vsock-peer.py LOG GUEST_CID

Listens on vsock port 5000 for the guest, answers its line with "pong: " and the
line, then connects to the guest's port 5001 (retrying until the guest listens),
sends a line and reads the answer. Writes a verdict line per exchange to LOG, which
check-boot.sh judges, and exits 0 only if both exchanges happened as described.
"""

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


def main():
    log_path, guest_cid = sys.argv[1], int(sys.argv[2])
    with open(log_path, "w", buffering=1) as log:
        try:
            server = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
            server.bind((socket.VMADDR_CID_ANY, 5000))
            server.listen(1)
            server.settimeout(TIMEOUT)
            conn, (cid, port) = server.accept()
            conn.settimeout(TIMEOUT)
            line = read_line(conn)
            conn.sendall(b"pong: " + line)
            log.write(
                f"vsock-peer: ok: the guest (cid {cid}, port {port}) connected to port 5000 "
                f"and sent {line.decode().strip()!r}\n"
            )
            conn.close()
            server.close()
            deadline = time.monotonic() + TIMEOUT
            while True:
                client = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
                client.settimeout(TIMEOUT)
                try:
                    client.connect((guest_cid, 5001))
                    break
                except OSError:
                    client.close()
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(0.2)
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
