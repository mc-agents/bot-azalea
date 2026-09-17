#!/usr/bin/env python3
"""Stand in for mcp-server long enough to see the bot join a world and answer from it.

    python3 dev/smoke.py --port 8765 --server 127.0.0.1 25578 --username azalea_bot
    # then start the bot pointed at that port

conform.py in mcp-server holds the bot to the protocol without a world. This is the other half:
listen, accept the hello, send it into the Paper server given, and check that what it answers
came from being there -- a position, and a new position after the server teleported it. A bot
that links, spawns and reports where the server put it has every layer working: the link, the
game connection, the packets that place the player, and a command sent as the player.

No dependencies, so a runner needs nothing but python3.
"""

import argparse
import json
import socket
import struct
import sys
import threading
import time
import uuid

JSON_FRAME = 0

# Somewhere on the flat world's surface, away from spawn, so the position after the teleport
# cannot be the one before it.
AWAY = (37, -60, 41)


class Link:
    def __init__(self, conn):
        self.conn = conn
        self.frames = []
        self.lock = threading.Condition()
        self.alive = True

    def send(self, message):
        payload = json.dumps(message).encode()
        self.conn.sendall(struct.pack(">IB", len(payload) + 1, JSON_FRAME) + payload)
        print("--> %s" % json.dumps(message), flush=True)

    def pump(self):
        buffer = b""
        while self.alive:
            try:
                chunk = self.conn.recv(65536)
            except OSError:
                break
            if not chunk:
                break
            buffer += chunk
            while len(buffer) >= 4:
                (length,) = struct.unpack(">I", buffer[:4])
                if len(buffer) < 4 + length:
                    break
                frame, buffer = buffer[4:4 + length], buffer[4 + length:]
                if frame[0] == JSON_FRAME:
                    self.on_frame(json.loads(frame[1:]))
        with self.lock:
            self.alive = False
            self.lock.notify_all()

    def on_frame(self, message):
        print("<-- %s" % json.dumps(message)[:1200], flush=True)
        with self.lock:
            self.frames.append(message)
            self.lock.notify_all()

    def until(self, predicate, seconds):
        deadline = time.monotonic() + seconds
        with self.lock:
            while True:
                for frame in self.frames:
                    if predicate(frame):
                        return frame
                left = deadline - time.monotonic()
                if left <= 0 or not self.alive:
                    return None
                self.lock.wait(left)

    def result(self, call_id, seconds):
        return self.until(lambda f: f.get("t") == "result" and f.get("id") == call_id, seconds)

    def call(self, tool, args=None, deadline=10000):
        call_id = str(uuid.uuid4())
        self.send({"t": "call", "id": call_id, "tool": tool, "args": args or {}, "deadlineMs": deadline})
        return self.result(call_id, deadline / 1000 + 5)


def ok(answer, what):
    if answer is None:
        raise SystemExit("FAIL  %s: no result in time" % what)
    if not answer.get("ok"):
        raise SystemExit("FAIL  %s: %s" % (what, json.dumps(answer.get("error"))))
    return answer


def position(answer):
    at = answer["data"]["position"]
    return (at["x"], at["y"], at["z"])


def main(arguments):
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", arguments.port))
    listener.listen(1)
    print("waiting for a bot on :%d" % arguments.port, flush=True)

    conn, address = listener.accept()
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    print("bot connected from %s" % (address,), flush=True)

    link = Link(conn)
    threading.Thread(target=link.pump, daemon=True).start()

    hello = link.until(lambda f: f.get("t") == "hello", 30)
    if hello is None:
        raise SystemExit("FAIL  the bot connected and never said hello")

    link.send({
        "t": "helloOk", "protocol": 1, "sessionId": str(uuid.uuid4()),
        "heartbeatMs": 5000, "repeatFlushMs": 1000, "limits": {}, "events": {"chat": True},
        "acceptedTools": [c["tool"] for c in hello["capabilities"]], "rejectedTools": [],
    })

    host, port = arguments.server
    spawn_timeout = 60000
    connect_id = str(uuid.uuid4())
    link.send({"t": "connect", "id": connect_id, "host": host, "port": int(port),
               "username": arguments.username, "spawnTimeoutMs": spawn_timeout})
    ok(link.result(connect_id, spawn_timeout / 1000 + 5), "connect")

    before = position(ok(link.call("get-position"), "get-position"))

    # A teleport is the server placing the player: the answer has to come back through the
    # position packets, not from anything the bot assumed about its own movement.
    ok(link.call("run-command", {"command": "tp %s %d %d %d" % ((arguments.username,) + AWAY)}), "run-command")

    after = None
    for _ in range(20):
        after = position(ok(link.call("get-position"), "get-position after the teleport"))
        if after == AWAY:
            break
        time.sleep(0.5)
    if after != AWAY:
        raise SystemExit("FAIL  stood at %s, then at %s after being sent to %s" % (before, after, AWAY))

    disconnect_id = str(uuid.uuid4())
    link.send({"t": "disconnect", "id": disconnect_id, "reason": "smoke finished"})
    ok(link.result(disconnect_id, 15), "disconnect")

    print("\nok    joined %s:%s at %s, teleported to %s, left" % (host, port, before, after), flush=True)
    link.send({"t": "shutdown", "reason": "smoke finished", "graceMs": 1000})
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=8765, help="where the bot dials in")
    parser.add_argument("--server", nargs=2, metavar=("HOST", "PORT"), default=("127.0.0.1", "25578"),
                        help="the Minecraft server the bot is sent to, as the bot reaches it")
    parser.add_argument("--username", default="azalea_bot")
    sys.exit(main(parser.parse_args()))
