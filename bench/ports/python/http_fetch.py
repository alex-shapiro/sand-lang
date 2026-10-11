"""Fetches web pages and reports their title, headings and links.

CPython port of examples/http_fetch.sand. Pages are fetched concurrently on a
thread pool (one thread per URL, like Sand's one task per URL).

Usage: python3 http_fetch.py URL...
"""

import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass


class FetchError(Exception):
    def __init__(self, message: str, kind: str):
        super().__init__(f"{kind}: {message}")


@dataclass(slots=True)
class Page:
    url: str
    status: int
    title: str | None
    headings: list[str]
    links: list[str]


def unescape(s: str) -> str:
    """Decodes the handful of HTML entities that commonly appear in text."""
    return s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", '"').replace("&#39;", "'").replace("&amp;", "&")


def strip_tags(s: str) -> str:
    return unescape(re.sub(r"\s+", " ", re.sub(r"<[^>]*>", "", s)).strip())


def parse_page(url: str, status: int, html: str) -> Page:
    title_re = re.compile(r"(?is)<title[^>]*>(.*?)</title>")
    heading_re = re.compile(r"(?is)<h([1-3])[^>]*>(.*?)</h[1-3]>")
    link_re = re.compile(r"""(?i)<a\s[^>]*href\s*=\s*["']([^"']+)["']""")

    m = title_re.search(html)
    title = strip_tags(m.group(1)) if m else None
    headings = []
    for m in heading_re.finditer(html):
        level = int(m.group(1))
        headings.append("  " * (level - 1) + strip_tags(m.group(2)))
    links = []
    seen = set()
    for m in link_re.finditer(html):
        href = unescape(m.group(1))
        if href.startswith("#") or href.startswith("javascript:") or href.startswith("mailto:"):
            continue
        resolved = urllib.parse.urljoin(url, href)
        if resolved not in seen:
            seen.add(resolved)
            links.append(resolved)
    return Page(url, status, title, headings, links)


def fetch(url: str) -> Page:
    request = urllib.request.Request(url, headers={"User-Agent": "sand-fetch/0.1"})
    try:
        with urllib.request.urlopen(request, timeout=10) as resp:
            status = resp.status
            final_url = resp.url
            content_type = resp.headers.get("content-type", "text/html")
            body = resp.read()
    except urllib.error.HTTPError as e:
        raise FetchError(f"{url} returned status {e.code}", "http") from None
    except urllib.error.URLError as e:
        raise FetchError(f"{url}: {e.reason}", "io") from None
    if "html" not in content_type:
        raise FetchError(f"{url} is not HTML ({content_type})", "invalid")
    return parse_page(final_url, status, body.decode("utf-8"))


def main() -> None:
    urls = sys.argv[1:]
    if not urls:
        print("usage: http_fetch.py URL...", file=sys.stderr)
        sys.exit(2)

    # Fetch all pages concurrently.
    with ThreadPoolExecutor(max_workers=len(urls)) as pool:
        tasks = [pool.submit(fetch, u) for u in urls]
        for url, t in zip(urls, tasks):
            try:
                page = t.result()
            except FetchError as e:
                print(f"{url}: {e}")
                continue
            print(f"{page.url} [{page.status}]")
            print(f"title: {page.title if page.title is not None else '(none)'}")
            print("headings:")
            for h in page.headings:
                print(f"  {h}")
            print(f"links ({len(page.links)}):")
            for link in page.links:
                print(f"  {link}")


main()
