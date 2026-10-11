"""A minimal Amazon S3 client using AWS Signature Version 4.

CPython port of examples/s3/s3.sand. Supports PutObject, GetObject,
HeadObject, DeleteObject and ListObjectsV2 (with pagination), against AWS or
any S3-compatible endpoint (path-style addressing).
"""

import hashlib
import hmac as _hmac
import http.client
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from urllib.parse import quote, urlsplit


class S3Error(Exception):
    def __init__(self, message: str, kind: str):
        super().__init__(f"{kind}: {message}")


@dataclass(slots=True)
class Credentials:
    access_key: str
    secret_key: str
    session_token: str | None = None


@dataclass(slots=True)
class ObjectInfo:
    key: str
    size: int
    etag: str
    last_modified: str


@dataclass(slots=True)
class SignedRequest:
    method: str
    host: str
    path: str
    query: list[tuple[str, str]]
    headers: dict[str, str]
    payload: bytes


@dataclass(slots=True)
class Response:
    status: int
    headers: dict[str, str]
    body: bytes

    def ok(self) -> bool:
        return 200 <= self.status <= 299

    def header(self, name: str) -> str | None:
        return self.headers.get(name.lower())

    def text(self) -> str:
        return self.body.decode("utf-8")


@dataclass(slots=True)
class Client:
    region: str
    credentials: Credentials
    # e.g. `https://s3.amazonaws.com` or `http://127.0.0.1:9000`
    endpoint: str = "https://s3.amazonaws.com"
    # Use `endpoint/bucket/key` instead of `bucket.endpoint/key`.
    path_style: bool = False
    timeout: float = 30.0

    def put_object(self, bucket: str, key: str, body: bytes, content_type: str = "application/octet-stream") -> None:
        check(self.send("PUT", bucket, key, [], {"content-type": content_type}, body))

    def get_object(self, bucket: str, key: str) -> bytes:
        return check(self.send("GET", bucket, key, [], {}, b"")).body

    def head_object(self, bucket: str, key: str) -> ObjectInfo | None:
        resp = self.send("HEAD", bucket, key, [], {}, b"")
        if resp.status == 404:
            return None
        r = check(resp)
        size = r.header("content-length")
        return ObjectInfo(
            key=key,
            size=int(size) if size and size.isdigit() else 0,
            etag=(r.header("etag") or "").replace('"', ""),
            last_modified=r.header("last-modified") or "",
        )

    def delete_object(self, bucket: str, key: str) -> None:
        check(self.send("DELETE", bucket, key, [], {}, b""))

    def list_objects(self, bucket: str, prefix: str = "") -> list[ObjectInfo]:
        """Lists all objects under `prefix`, following continuation tokens."""
        objects = []
        token = None
        while True:
            query = [("list-type", "2"), ("prefix", prefix)]
            if token is not None:
                query.append(("continuation-token", token))
            xml = check(self.send("GET", bucket, "", query, {}, b"")).text()
            for block in xml_blocks(xml, "Contents"):
                size = xml_text(block, "Size")
                objects.append(
                    ObjectInfo(
                        key=xml_unescape(xml_text(block, "Key") or ""),
                        size=int(size) if size and size.isdigit() else 0,
                        etag=(xml_text(block, "ETag") or "").replace("&quot;", "").replace('"', ""),
                        last_modified=xml_text(block, "LastModified") or "",
                    )
                )
            if xml_text(xml, "IsTruncated") != "true":
                break
            token = xml_text(xml, "NextContinuationToken")
            if token is None:
                break
        return objects

    def send(
        self,
        method: str,
        bucket: str,
        key: str,
        query: list[tuple[str, str]],
        extra: dict[str, str],
        body: bytes,
    ) -> Response:
        base = urlsplit(self.endpoint)
        host_base = base.netloc
        if self.path_style:
            host, path = host_base, "/" + bucket + ("" if not key else "/" + key)
        else:
            host, path = f"{bucket}.{host_base}", "/" + key
        request = SignedRequest(method, host, path, query, extra, body)
        headers = sign(request, self.credentials, self.region, datetime.now(timezone.utc))
        target = encode_path(path)
        if query:
            target += "?" + canonical_query(query)
        conn_class = http.client.HTTPSConnection if base.scheme == "https" else http.client.HTTPConnection
        conn = conn_class(host, timeout=self.timeout)
        try:
            conn.request(method, target, body=body, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
            return Response(resp.status, {k.lower(): v for k, v in resp.getheaders()}, data)
        except (OSError, http.client.HTTPException) as e:
            raise S3Error(f"{method} {host}{target}: {e}", "io") from None
        finally:
            conn.close()


def check(resp: Response) -> Response:
    if resp.ok():
        return resp
    try:
        text = resp.text()
    except UnicodeDecodeError:
        text = ""
    code = xml_text(text, "Code") or f"HTTP{resp.status}"
    message = xml_text(text, "Message") or f"request failed with status {resp.status}"
    raise S3Error(f"{code}: {message}", "http")


def encode_component(s: str) -> str:
    """Percent-encodes everything but the RFC 3986 unreserved characters."""
    return quote(s, safe="")


def encode_path(s: str) -> str:
    """Like `encode_component`, but keeps "/"."""
    return quote(s, safe="/")


def hex_sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def hmac(key: bytes, msg: str) -> bytes:
    return _hmac.new(key, msg.encode(), hashlib.sha256).digest()


def canonical_query(query: list[tuple[str, str]]) -> str:
    pairs = sorted((encode_component(k), encode_component(v)) for k, v in query)
    return "&".join(f"{k}={v}" for k, v in pairs)


def sign(req: SignedRequest, creds: Credentials, region: str, now: datetime) -> dict[str, str]:
    """Computes the SigV4 headers (including `authorization`) for a request."""
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date = now.strftime("%Y%m%d")
    payload_hash = hex_sha256(req.payload)

    headers: dict[str, str] = {}
    for k, v in req.headers.items():
        headers[k.lower()] = v.strip()
    headers["host"] = req.host
    headers["x-amz-date"] = amz_date
    headers["x-amz-content-sha256"] = payload_hash
    if creds.session_token is not None:
        headers["x-amz-security-token"] = creds.session_token

    names = sorted(headers)
    canonical_headers = "".join(f"{n}:{headers[n]}\n" for n in names)
    signed_headers = ";".join(names)
    canonical_request = "\n".join(
        [
            req.method,
            encode_path(req.path),
            canonical_query(req.query),
            canonical_headers,
            signed_headers,
            payload_hash,
        ]
    )

    scope = f"{date}/{region}/s3/aws4_request"
    string_to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hex_sha256(canonical_request.encode())])

    k_date = hmac(f"AWS4{creds.secret_key}".encode(), date)
    k_region = hmac(k_date, region)
    k_service = hmac(k_region, "s3")
    k_signing = hmac(k_service, "aws4_request")
    signature = hmac(k_signing, string_to_sign).hex()

    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={creds.access_key}/{scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}"
    )
    del headers["host"]
    return headers


# ---------------------------------------------------------------------------
# Just enough XML for S3 responses.


def xml_blocks(xml: str, tag: str) -> list[str]:
    return re.findall(f"(?s)<{tag}>(.*?)</{tag}>", xml)


def xml_text(xml: str, tag: str) -> str | None:
    blocks = xml_blocks(xml, tag)
    return blocks[0] if blocks else None


def xml_unescape(s: str) -> str:
    return s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", '"').replace("&apos;", "'").replace("&amp;", "&")


def _self_test() -> None:
    """The test blocks of s3.sand."""
    creds = Credentials("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
    req = SignedRequest("GET", "examplebucket.s3.amazonaws.com", "/test.txt", [], {"Range": "bytes=0-9"}, b"")
    now = datetime(2013, 5, 24, tzinfo=timezone.utc)
    headers = sign(req, creds, "us-east-1", now)
    auth = headers["authorization"]
    assert auth.startswith("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request")
    assert "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date" in auth
    assert auth.endswith("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), auth
    assert headers["x-amz-date"] == "20130524T000000Z"
    assert canonical_query([("prefix", "a b"), ("list-type", "2")]) == "list-type=2&prefix=a%20b"
    xml = "<R><Contents><Key>a&amp;b</Key></Contents><Contents><Key>c</Key></Contents></R>"
    assert len(xml_blocks(xml, "Contents")) == 2
    assert xml_unescape(xml_text(xml, "Key")) == "a&b"


if __name__ == "__main__":
    _self_test()
    print("3 tests passed")
