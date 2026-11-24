"""Loopback-only Synapse relay for the workspace uncertain-create scenario."""

import http.client
import http.server
import json
import sys
import threading


class Relay(http.server.BaseHTTPRequestHandler):
    injected = False
    lock = threading.Lock()

    def log_message(self, *_):
        pass

    def forward(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ("host", "connection", "content-length", "accept-encoding")}
        connection = http.client.HTTPConnection("127.0.0.1", int(sys.argv[2]), timeout=75)
        try:
            connection.request(self.command, self.path, body, headers)
            response = connection.getresponse()
            status, data = response.status, response.read()
            if self.command == "POST" and self.path.partition("?")[0].endswith("/createRoom") and status == 200:
                name = json.loads(body).get("name", "")
                room = json.loads(data)["room_id"]
                with Relay.lock:
                    if name == "Recovery plans" and not Relay.injected:
                        Relay.injected = True
                        status = 502
                        data = b'{"errcode":"M_UNKNOWN","error":"fixture hid a successful createRoom response"}'
                    print(f"create {name} {room} {status}", flush=True)
            self.send_response(status)
            self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # The long-poll client can stop or restart during the suite.
        finally:
            connection.close()

    do_GET = do_POST = do_PUT = do_DELETE = forward


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Relay).serve_forever()
