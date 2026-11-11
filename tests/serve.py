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


port, root = int(sys.argv[1]), sys.argv[2]
Server(("::", port), functools.partial(Quiet, directory=root)).serve_forever()
