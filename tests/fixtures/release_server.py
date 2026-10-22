#!/usr/bin/env python3
"""Serve a release directory as rundispatch.sh and GitHub serve it, through
their redirects:

  /install -> /releases/latest/download/install.sh   (the website's 302)
  /releases/latest/download/<file> -> /releases/download/<tag>/<file>
  /releases/download/<tag>/<file> -> /objects/<file>  (GitHub's storage)
  /objects/<file>: the file

Usage: release_server.py <directory> <tag>. Prints its port, then serves
on 127.0.0.1 until killed.
"""
import http.server, sys

root, tag = sys.argv[1], sys.argv[2]


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=root, **kwargs)

    def do_GET(self):
        path, name = self.path, self.path.rsplit('/', 1)[-1]
        if path == '/install':
            self.redirect('/releases/latest/download/install.sh')
        elif path.startswith('/releases/latest/download/'):
            self.redirect(f'/releases/download/{tag}/{name}')
        elif path.startswith(f'/releases/download/{tag}/'):
            self.redirect(f'/objects/{name}')
        elif path.startswith('/objects/'):
            self.path = '/' + name
            super().do_GET()
        else:
            self.send_error(404)

    def redirect(self, to):
        self.send_response(302)
        self.send_header('Location', to)
        self.send_header('Content-Length', '0')
        self.end_headers()

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
