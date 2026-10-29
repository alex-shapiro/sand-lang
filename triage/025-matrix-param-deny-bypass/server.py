import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def normalize(raw):
    out = []
    for seg in raw.split("?")[0].split("/"):
        seg = seg.split(";", 1)[0]
        if seg in ("", "."):
            continue
        if seg == "..":
            if out:
                out.pop()
            continue
        out.append(seg)
    return "/" + "/".join(out)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = normalize(self.path)
        body = (
            (b"SECRET" if path == "/admin/secret" else b"public")
            + b" (raw "
            + self.path.encode()
            + b" -> "
            + path.encode()
            + b")\n"
        )
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
