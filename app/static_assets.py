"""Public asset fingerprints and precompressed files, without runtime build tools."""

import hashlib
import mimetypes
import re
from functools import lru_cache
from pathlib import Path
from urllib.parse import parse_qsl, unquote, urlsplit

from fastapi.staticfiles import StaticFiles
from starlette.datastructures import Headers

STATIC = Path(__file__).resolve().parent / "static"
ASSET_URL = re.compile(r"/static/[A-Za-z0-9_./%\-]+(?:\?[^\s\"'<> ,]*)?")


def _file(path: str) -> Path | None:
    if not path.startswith("/static/"):
        return None
    try:
        candidate = (STATIC / unquote(path.removeprefix("/static/"))).resolve()
        return candidate if candidate.is_relative_to(STATIC.resolve()) and candidate.is_file() else None
    except (OSError, ValueError):
        return None


@lru_cache(maxsize=256)
def _digest(path: Path, modified: int, size: int) -> str:
    # mtime and size are part of the cache key, including during local editing.
    return hashlib.sha256(path.read_bytes()).hexdigest()[:12]


def fingerprint(path: str) -> str:
    file = _file(path)
    if file is None:
        return ""
    try:
        stat = file.stat()
        return _digest(file, stat.st_mtime_ns, stat.st_size)
    except OSError:
        # A local update may replace a file between lookup and hashing.
        return ""


def version_urls(content: str) -> str:
    """Version only existing public files; never turn a URL into an arbitrary file read."""

    def replace(match: re.Match[str]) -> str:
        url = match.group()
        parts = urlsplit(url)
        digest = fingerprint(parts.path) if not parts.query else ""
        return f"{url}?v={digest}" if digest else url

    return ASSET_URL.sub(replace, content)


def cache_control(path: str, query: str) -> str:
    digest = fingerprint(path)
    if digest and parse_qsl(query, keep_blank_values=True) == [("v", digest)]:
        return "public, max-age=31536000, immutable"
    return "public, max-age=0, must-revalidate"


def _accepts_brotli(value: str) -> bool:
    qualities = {}
    for part in value.lower().split(","):
        name, *parameters = part.strip().split(";")
        quality = 1.0
        for parameter in parameters:
            key, _, raw = parameter.strip().partition("=")
            if key == "q":
                try:
                    quality = float(raw)
                except ValueError:
                    quality = 0.0
        qualities[name] = quality if 0 <= quality <= 1 else 0.0
    return qualities.get("br", qualities.get("*", 0.0)) > 0


class PublicStaticFiles(StaticFiles):
    """Serve checked-in Brotli sidecars when accepted, retaining identity fallback."""

    async def get_response(self, path: str, scope):
        headers = Headers(scope=scope)
        public_path = "/static/" + path
        compressed = (
            "range" not in headers
            and _accepts_brotli(headers.get("accept-encoding", ""))
            and _file(public_path) is not None
            and _file(public_path + ".br") is not None
        )
        response = await super().get_response(path + ".br" if compressed else path, scope)
        if compressed:
            response.headers["Content-Encoding"] = "br"
            response.headers["Content-Type"] = mimetypes.guess_type(path)[0] or "application/octet-stream"
        if response.status_code in (200, 206, 304):
            response.headers["Cache-Control"] = cache_control(
                public_path, scope.get("query_string", b"").decode("latin-1")
            )
        response.headers.setdefault("Vary", "Accept-Encoding")
        return response
