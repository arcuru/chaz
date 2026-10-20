#!/usr/bin/env python3
"""Loopback-only Synapse proxy: duplicate timeline events in bridge /sync replies."""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.request import Request, urlopen

PORT, UPSTREAM_PORT = map(int, sys.argv[1:])


class Handler(BaseHTTPRequestHandler):
    def forward(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        headers = {
            key: value
            for key, value in self.headers.items()
            if key.lower() not in ("host", "content-length", "accept-encoding")
        }
        request = Request(
            f"http://127.0.0.1:{UPSTREAM_PORT}{self.path}",
            data=body if body else None,
            headers=headers,
            method=self.command,
        )
        try:
            response = urlopen(request, timeout=40)
        except HTTPError as error:
            response = error
        with response:
            payload = response.read()
            if response.status == 200 and self.path.startswith("/_matrix/client/v3/sync?"):
                data = json.loads(payload)
                for room in data.get("rooms", {}).get("join", {}).values():
                    events = room.get("timeline", {}).get("events", [])
                    duplicates = [e for e in events if e.get("type") == "m.room.message"]
                    events.extend(duplicates)
                    for event in duplicates:
                        print("redelivered: " + event["event_id"], flush=True)
                payload = json.dumps(data).encode()
            self.send_response(response.status)
            self.send_header("Content-Type", response.headers.get("Content-Type", "application/json"))
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    do_GET = do_POST = do_PUT = forward

    def log_message(self, *_):
        pass


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
