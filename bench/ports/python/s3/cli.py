"""Command line S3 client.

CPython port of examples/s3/cli.sand.

Usage: python3 cli.py COMMAND ARGS...

Commands:
    ls BUCKET [PREFIX]
    put BUCKET KEY FILE
    get BUCKET KEY [FILE]
    cat BUCKET KEY
    rm BUCKET KEY
    stat BUCKET KEY
"""

import os
import sys

import s3


def client() -> s3.Client:
    access_key = os.environ.get("AWS_ACCESS_KEY_ID")
    if access_key is None:
        raise s3.S3Error("AWS_ACCESS_KEY_ID is not set", "invalid")
    secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY")
    if secret_key is None:
        raise s3.S3Error("AWS_SECRET_ACCESS_KEY is not set", "invalid")
    credentials = s3.Credentials(access_key, secret_key, session_token=os.environ.get("AWS_SESSION_TOKEN"))
    region = os.environ.get("AWS_REGION", "us-east-1")
    endpoint = os.environ.get("S3_ENDPOINT")
    if endpoint is not None:
        return s3.Client(region, credentials, endpoint=endpoint, path_style=True)
    return s3.Client(region, credentials)


def human(n: int) -> str:
    if n < 1024:
        return f"{n}B"
    size = float(n)
    for unit in ["K", "M", "G", "T"]:
        size = size / 1024.0
        if size < 1024.0:
            return f"{size:.1f}{unit}"
    return f"{size:.1f}P"


def read_bytes(path: str) -> bytes:
    with open(path, "rb") as f:
        return f.read()


def write_bytes(path: str, data: bytes) -> None:
    with open(path, "wb") as f:
        f.write(data)


def run(args: list[str]) -> None:
    c = client()
    match args:
        case ["ls", bucket, *_]:
            prefix = args[2] if len(args) > 2 else ""
            objects = c.list_objects(bucket, prefix=prefix)
            for o in objects:
                print(f"{o.last_modified:<24} {human(o.size):>8}  {o.key}")
            print(f"{len(objects)} objects, {human(sum(o.size for o in objects))}")
        case ["put", bucket, key, file]:
            data = read_bytes(file)
            c.put_object(bucket, key, data)
            print(f"uploaded {file} to s3://{bucket}/{key} ({human(len(data))})")
        case ["get", bucket, key]:
            sys.stdout.write(c.get_object(bucket, key).decode("utf-8", errors="replace"))
        case ["get", bucket, key, file]:
            data = c.get_object(bucket, key)
            write_bytes(file, data)
            print(f"downloaded s3://{bucket}/{key} to {file} ({human(len(data))})")
        case ["cat", bucket, key]:
            sys.stdout.write(c.get_object(bucket, key).decode("utf-8"))
        case ["rm", bucket, key]:
            c.delete_object(bucket, key)
            print(f"deleted s3://{bucket}/{key}")
        case ["stat", bucket, key]:
            info = c.head_object(bucket, key)
            if info is not None:
                print(f"{info.key}: {info.size} bytes, etag {info.etag}, modified {info.last_modified}")
            else:
                print(f"not found: s3://{bucket}/{key}")
        case _:
            print(
                "usage: cli.py (ls BUCKET [PREFIX] | put BUCKET KEY FILE | get BUCKET KEY [FILE] "
                "| cat BUCKET KEY | rm BUCKET KEY | stat BUCKET KEY)",
                file=sys.stderr,
            )
            sys.exit(2)


try:
    run(sys.argv[1:])
except (s3.S3Error, OSError, UnicodeDecodeError) as e:
    print(f"error: {e}", file=sys.stderr)
    sys.exit(1)
