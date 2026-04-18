#!/usr/bin/env python3
"""Simple HTTP server with Range request support for the Rós Madair demo."""

import http.server
import io
import os
import sys


class RangeRequestHandler(http.server.SimpleHTTPRequestHandler):
    """HTTP handler that supports Range requests (needed for page file access)."""

    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".ttl": "text/turtle",
        ".wasm": "application/wasm",
        ".json": "application/json",
        ".js": "application/javascript",
    }

    def send_head(self):
        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            return super().send_head()

        range_header = self.headers.get("Range")
        if range_header is None:
            return super().send_head()

        # Parse "bytes=start-end"
        try:
            range_spec = range_header.replace("bytes=", "")
            start_str, end_str = range_spec.split("-")
            file_size = os.path.getsize(path)
            start = int(start_str) if start_str else 0
            end = int(end_str) if end_str else file_size - 1
            end = min(end, file_size - 1)
            length = end - start + 1
        except (ValueError, IndexError):
            self.send_error(416, "Invalid range")
            return None

        with open(path, "rb") as fh:
            fh.seek(start)
            data = fh.read(length)
        f = io.BytesIO(data)

        self.send_response(206)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Content-Length", str(length))
        self.send_header("Content-Range", f"bytes {start}-{end}/{file_size}")
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

        return f

    def end_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Range")
        self.send_header("Access-Control-Expose-Headers", "Content-Range")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_OPTIONS(self):
        self.send_response(200)
        self.end_headers()


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    directory = sys.argv[2] if len(sys.argv) > 2 else "."

    os.chdir(directory)
    handler = RangeRequestHandler
    server = http.server.HTTPServer(("", port), handler)
    print(f"Serving on http://localhost:{port}")
    print(f"Directory: {os.getcwd()}")
    server.serve_forever()


if __name__ == "__main__":
    main()
