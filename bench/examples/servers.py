"""Local servers and clients used by the network benchmarks.

* `PageServer`: serves generated HTML pages (plus a redirect, a 404 and a
  non-HTML resource) for http_fetch.
* `FakeS3`: an in-memory, S3-shaped HTTP server for s3/cli. Like the mock in
  tests/examples.rs it only checks that requests carry SigV4-looking headers.
* `chat_session`: drives many concurrent clients against a chat server and
  summarises what each client received.
"""

import hashlib
import http.server
import re
import socket
import threading
import urllib.parse
from collections.abc import Callable


class _QuietHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    # Headers and body go out in separate writes; without this, Nagle's
    # algorithm and delayed ACKs add ~40ms to some responses.
    disable_nagle_algorithm = True

    def log_message(self, format, *args):  # noqa: A002 - signature from the base class
        pass

    def reply(self, status: int, body: bytes = b"", headers: dict[str, str] | None = None, head: bool = False):
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def read_body(self) -> bytes:
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""


class _Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 128


def _serve(handler: type) -> tuple[_Server, str]:
    server = _Server(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host, port = server.server_address[:2]
    return server, f"{host}:{port}"


# ---------------------------------------------------------------------------


class PageServer:
    def __init__(self, pages: dict[str, bytes]):
        self.pages = pages
        outer = self

        class Handler(_QuietHandler):
            def do_GET(self):
                if self.path == "/old":
                    self.reply(301, b"", {"location": "/page/0"})
                elif self.path == "/data.json":
                    self.reply(200, b"{}", {"content-type": "application/json"})
                elif (page := outer.pages.get(self.path)) is not None:
                    self.reply(200, page, {"content-type": "text/html; charset=utf-8"})
                else:
                    self.reply(404, b"not found")

        self.server, self.addr = _serve(Handler)

    def close(self):
        self.server.shutdown()


# ---------------------------------------------------------------------------


class FakeS3:
    PAGE_SIZE = 1000

    def __init__(self):
        self.store: dict[str, bytes] = {}
        self.lock = threading.Lock()
        outer = self

        class Handler(_QuietHandler):
            def handle_one(self, method: str):
                body = self.read_body()
                auth = self.headers.get("authorization", "")
                if not auth.startswith("AWS4-HMAC-SHA256 Credential=") or "Signature=" not in auth:
                    return self.reply(403, b"<Error><Code>AccessDenied</Code><Message>bad auth</Message></Error>")
                if len(self.headers.get("x-amz-content-sha256", "")) != 64 or "x-amz-date" not in self.headers:
                    return self.reply(400, b"<Error><Code>BadRequest</Code><Message>missing headers</Message></Error>")
                raw_path, _, query = self.path.partition("?")
                path = urllib.parse.unquote(raw_path.lstrip("/"))
                params = dict(urllib.parse.parse_qsl(query, keep_blank_values=True))
                head = method == "HEAD"
                with outer.lock:
                    if method == "PUT":
                        outer.store[path] = body
                        return self.reply(200, b"", {"etag": '"abc"'})
                    if method == "GET" and params.get("list-type") == "2":
                        return self.reply(200, outer.list_xml(path.rstrip("/"), params).encode())
                    if method in ("GET", "HEAD"):
                        data = outer.store.get(path)
                        if data is None:
                            msg = b"<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"
                            return self.reply(404, msg, head=head)
                        hdrs = {"etag": '"abc"', "last-modified": "Mon, 01 Jan 2024 00:00:00 GMT"}
                        return self.reply(200, data, hdrs, head=head)
                    if method == "DELETE":
                        outer.store.pop(path, None)
                        return self.reply(204)
                return self.reply(405)

            def do_GET(self):
                self.handle_one("GET")

            def do_HEAD(self):
                self.handle_one("HEAD")

            def do_PUT(self):
                self.handle_one("PUT")

            def do_DELETE(self):
                self.handle_one("DELETE")

        self.server, self.addr = _serve(Handler)

    def list_xml(self, bucket: str, params: dict[str, str]) -> str:
        prefix = f"{bucket}/{params.get('prefix', '')}"
        keys = sorted(k for k in self.store if k.startswith(prefix))
        start = int(params.get("continuation-token") or 0)
        page = keys[start : start + self.PAGE_SIZE]
        truncated = start + self.PAGE_SIZE < len(keys)
        xml = ["<ListBucketResult>"]
        for k in page:
            key = k[len(bucket) + 1 :].replace("&", "&amp;")
            xml.append(
                f"<Contents><Key>{key}</Key><LastModified>2024-01-01T00:00:00.000Z</LastModified>"
                f"<ETag>&quot;e&quot;</ETag><Size>{len(self.store[k])}</Size><StorageClass>STANDARD</StorageClass></Contents>"
            )
        xml.append(f"<IsTruncated>{'true' if truncated else 'false'}</IsTruncated>")
        if truncated:
            xml.append(f"<NextContinuationToken>{start + self.PAGE_SIZE}</NextContinuationToken>")
        xml.append("</ListBucketResult>")
        return "".join(xml)

    def reset(self, objects: dict[str, bytes] | None = None):
        with self.lock:
            self.store = dict(objects or {})

    def close(self):
        self.server.shutdown()


# ---------------------------------------------------------------------------

_MSG = re.compile(rb"^user(\d+): msg (\d+) (\d+)\n$")


def chat_session(addr: str, clients: int, messages: int, timeout: float = 60.0) -> bytes:
    """Connects `clients` clients, has each set a nick and send `messages`
    lines, waits until every client has received every other client's
    messages, asks `/who`, then quits. Returns a deterministic summary
    (arrival order between senders is not deterministic, so each client's
    lines are reported as a sorted digest, after checking that each sender's
    messages arrived in order)."""
    host, _, port = addr.rpartition(":")
    conns = []
    welcome = []
    for _ in range(clients):
        s = socket.create_connection((host, int(port)), timeout=timeout)
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        f = s.makefile("rb")
        welcome.append(f.readline())
        conns.append((s, f))

    expected = (clients - 1) * messages
    received: list[list[bytes]] = [[] for _ in range(clients)]
    errors: list[str] = []

    def reader(i: int):
        f = conns[i][1]
        got = received[i]
        n = 0
        renamed = f"you are now user{i}\n".encode()
        seen_rename = False
        try:
            while n < expected or not seen_rename:
                line = f.readline()
                if not line:
                    errors.append(f"client {i}: connection closed after {n} messages")
                    return
                got.append(line)
                if _MSG.match(line):
                    n += 1
                elif line == renamed:
                    seen_rename = True
        except OSError as e:
            errors.append(f"client {i}: {e}")

    threads = [threading.Thread(target=reader, args=(i,)) for i in range(clients)]
    for t in threads:
        t.start()
    for i, (s, _) in enumerate(conns):
        batch = [f"/nick user{i}\n"] + [f"msg {i} {j}\n" for j in range(messages)]
        s.sendall("".join(batch).encode())
    for t in threads:
        t.join()
    if errors:
        raise RuntimeError("; ".join(errors))

    s0, f0 = conns[0]
    s0.sendall(b"/who\n")
    who = f0.readline()
    for s, f in conns:
        s.sendall(b"/quit\n")
    for s, f in conns:
        f.close()
        s.close()

    out = [b"who: " + who]
    for i in range(clients):
        last_seen: dict[int, int] = {}
        for line in received[i]:
            if m := _MSG.match(line):
                sender, seq = int(m[1]), int(m[3])
                if seq != last_seen.get(sender, -1) + 1:
                    raise RuntimeError(f"client {i}: message {seq} from user{sender} out of order")
                last_seen[sender] = seq
        digest = hashlib.sha256(b"".join(sorted(received[i]))).hexdigest()[:16]
        out.append(b"client %d: %s %d lines, digest %s\n" % (i, welcome[i].strip(), len(received[i]), digest.encode()))
    return b"".join(out)


def wait_for_line(stream, prefix: bytes, timeout: float, on_timeout: Callable[[], str]) -> bytes:
    """Reads lines from a pipe until one starts with `prefix`."""
    result: list[bytes] = []

    def read():
        for line in iter(stream.readline, b""):
            if line.startswith(prefix):
                result.append(line)
                return

    t = threading.Thread(target=read, daemon=True)
    t.start()
    t.join(timeout)
    if not result:
        raise RuntimeError(on_timeout())
    return result[0]
