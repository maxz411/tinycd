"""Serve the deployed fixture without HTTPServer's unused DNS lookup."""
import os
from http.server import SimpleHTTPRequestHandler
from socketserver import TCPServer


class Server(TCPServer):
    allow_reuse_address = True


with Server(('127.0.0.1', int(os.environ['PORT'])), SimpleHTTPRequestHandler) as server:
    server.serve_forever()
