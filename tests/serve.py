"""Static file server for the fixtures: like `python -m http.server`, but with a
listen backlog big enough for many parallel browser sessions (Windows refuses
connections beyond the default backlog of 5), on IPv4 and IPv6 (localhost and
127.0.0.1 are both used), and quiet."""
import contextlib
import functools
import http.server
import socket
import sys


class Server(http.server.ThreadingHTTPServer):
    request_queue_size = 128
    address_family = socket.AF_INET6
    daemon_threads = True

    def server_bind(self):
        with contextlib.suppress(Exception):
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        # a sign-in that sets a cookie for the parent domain (Domain=app.localhost), as an
        # app on sub.app.localhost does when its OAuth callback lands on the apex
        if self.path.startswith("/set-parent-cookie"):
            return self.reply("signed in", "sid=abc123; Domain=app.localhost; Path=/; HttpOnly; SameSite=Lax")
        # what the server received: the page shows the Cookie header
        if self.path.startswith("/whoami"):
            return self.reply("cookie: " + (self.headers.get("Cookie") or "(none)"))
        return super().do_GET()

    def reply(self, text, cookie=None):
        body = f"<!doctype html><title>t</title><p>{text}</p>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        self.wfile.write(body)

    def end_headers(self):
        # cached.html is cacheable like a GitHub Pages deploy (10 minutes)
        if self.path.startswith("/cached.html"):
            self.send_header("Cache-Control", "max-age=600")
        super().end_headers()


port, root = int(sys.argv[1]), sys.argv[2]
Server(("::", port), functools.partial(Quiet, directory=root)).serve_forever()
