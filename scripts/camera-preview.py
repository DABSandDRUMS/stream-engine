#!/usr/bin/env python3
"""Owner-only loopback gateway for stock MediaMTX camera and desktop players."""
import argparse
import http.client
import json
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urljoin, urlsplit, urlunsplit

UPSTREAM = 'http://127.0.0.1:18889'
SESSION = re.compile(r'/(?:camera|desktop)/whep/[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}')
RESPONSE_HEADERS = {'content-type', 'etag', 'id', 'accept-post', 'accept-patch', 'link'}


class Handler(BaseHTTPRequestHandler):
    def parse_request(self):
        if not super().parse_request():
            return False
        # Serve strips client-supplied identity headers. Only local processes can
        # reach this origin directly; never bind this server outside loopback.
        logins = self.headers.get_all('Tailscale-User-Login', [])
        navigation = (self.command in ('GET', 'HEAD')
                      and self.headers.get('Sec-Fetch-Mode') == 'navigate'
                      and self.headers.get('Sec-Fetch-Dest') == 'document')
        if logins != [self.server.owner] or (
                self.headers.get('Sec-Fetch-Site') == 'cross-site' and not navigation):
            self.error(403, 'This camera monitor is restricted to its Tailscale owner')
            return False
        return True

    def respond(self, status, data=b'', headers=()):
        self.send_response(status)
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('X-Frame-Options', 'DENY')
        self.send_header('Referrer-Policy', 'no-referrer')
        self.send_header('Content-Security-Policy',
                         "default-src 'none'; script-src 'self' 'unsafe-inline'; "
                         "style-src 'unsafe-inline'; connect-src 'self'; "
                         "media-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'")
        for key, value in headers:
            self.send_header(key, value)
        try:
            self.end_headers()
            if self.command != 'HEAD':
                self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def error(self, status, message):
        self.respond(status, json.dumps({'error': message}).encode(),
                     [('Content-Type', 'application/json')])

    def handle_camera(self):
        connection = None
        try:
            url = urlsplit(self.path)
            if url.scheme or url.netloc or url.fragment:
                self.error(400, 'An origin-relative camera route is required')
                return
            if url.path in ('/', '/camera', '/desktop') and self.command in ('GET', 'HEAD'):
                stream = self.server.default_stream if url.path == '/' else url.path[1:]
                self.respond(302, headers=[('Location', f'/{stream}/?muted=true')])
                return
            if url.path in ('/camera/', '/camera/reader.js', '/desktop/', '/desktop/reader.js'):
                allowed = ('GET', 'HEAD')
            elif url.path in ('/camera/whep', '/desktop/whep'):
                allowed = ('POST', 'OPTIONS')
            elif SESSION.fullmatch(url.path):
                allowed = ('PATCH', 'DELETE', 'OPTIONS')
            else:
                self.error(404, 'Not found')
                return
            if self.command not in allowed:
                self.respond(405, headers=[('Allow', ', '.join(allowed))])
                return

            # Browser SDP uses Content-Length; do not accept ambiguous framing.
            lengths = self.headers.get_all('Content-Length', [])
            if len(lengths) > 1 or self.headers.get_all('Transfer-Encoding', []):
                self.error(400, 'Ambiguous request body framing')
                return
            length = int(lengths[0]) if lengths else 0
            if length < 0 or length > 1024 * 1024:
                self.error(413, 'SDP request exceeds limit')
                return
            self.connection.settimeout(15)
            body = self.rfile.read(length)
            if len(body) != length:
                self.error(400, 'Incomplete request body')
                return
            request_headers = {key: self.headers[key] for key in ('Content-Type', 'If-Match', 'Accept')
                               if key in self.headers}
            request_headers['Accept-Encoding'] = 'identity'
            connection = http.client.HTTPConnection('127.0.0.1', 18889, timeout=15)
            # MediaMTX serves static pages only on GET; implement HEAD here.
            method = 'GET' if self.command == 'HEAD' else self.command
            connection.request(method, urlunsplit(('', '', url.path, url.query, '')),
                               body=body, headers=request_headers)
            upstream = connection.getresponse()
            data = upstream.read()
            headers = [(key, value) for key, value in upstream.getheaders()
                       if key.lower() in RESPONSE_HEADERS]
            location = upstream.getheader('Location')
            if location is not None:
                target = urlsplit(urljoin(UPSTREAM + self.path, location))
                if target.scheme != 'http' or target.netloc != '127.0.0.1:18889' or not (
                        target.path in ('/camera/', '/desktop/') or SESSION.fullmatch(target.path)):
                    self.error(502, 'Relay returned an unexpected redirect')
                    return
                headers.append(('Location', urlunsplit(('', '', target.path, target.query, ''))))
            self.respond(upstream.status, data, headers)
        except ValueError:
            self.error(400, 'Invalid request')
        except (OSError, http.client.HTTPException) as error:
            self.log_error('camera relay unavailable: %s', error)
            self.error(503, 'Camera relay unavailable; waiting for it to reconnect')
        finally:
            if connection is not None:
                connection.close()

    do_GET = handle_camera
    do_HEAD = handle_camera
    do_POST = handle_camera
    do_PATCH = handle_camera
    do_DELETE = handle_camera
    do_OPTIONS = handle_camera
    do_PUT = handle_camera

    def log_message(self, format, *args):
        # Do not log session secret URLs or Tailscale account headers.
        pass

    def log_error(self, format, *args):
        BaseHTTPRequestHandler.log_message(self, format, *args)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--owner-login', required=True)
    parser.add_argument('--port', type=int, default=18770)
    parser.add_argument('--default-stream', choices=('camera', 'desktop'), default='camera')
    args = parser.parse_args()
    server = ThreadingHTTPServer(('127.0.0.1', args.port), Handler)
    server.owner = args.owner_login
    server.default_stream = args.default_stream
    print(f'Private WebRTC camera gateway listening on 127.0.0.1:{args.port}', flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == '__main__':
    main()
