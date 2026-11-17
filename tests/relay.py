"""TCP relay with a fixed delay per chunk, to measure SSH latency.

Listens on 127.0.0.1:2222 and forwards to the real sshd on 127.0.0.1:22.
Usage: python3 tests/relay.py <delay in ms>
"""

import socket
import sys
import threading
import time

DELAY = int(sys.argv[1]) / 1000.0 if len(sys.argv) > 1 else 0.0
UPSTREAM = ("127.0.0.1", 22)


def pump(src, dst):
    try:
        while True:
            chunk = src.recv(65536)
            if not chunk:
                break
            # Same shape as netem: hold each chunk for the delay.
            time.sleep(DELAY)
            dst.sendall(chunk)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(client):
    try:
        up = socket.create_connection(UPSTREAM)
    except OSError:
        client.close()
        return
    for a, b in ((client, up), (up, client)):
        threading.Thread(target=pump, args=(a, b), daemon=True).start()


server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", 2222))
server.listen(64)
print(f"relay 2222 to 22, {DELAY * 1000:.0f} ms per chunk", flush=True)
while True:
    conn, _ = server.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
