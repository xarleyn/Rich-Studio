"""Public product page, served on the marketing host and at /landing for local preview."""

import base64
import hashlib
import html
import json
import re
from functools import lru_cache
from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, Response

import public_site
import security
import settings
import static_assets

router = APIRouter()
STATIC = Path(__file__).resolve().parent.parent / "static"
SEO = STATIC.parent / "bundled" / "public-site-seo.json"


@lru_cache(maxsize=1)
def metadata() -> dict:
    """Editorial dates and structured data, shared with the native runtime."""
    return json.loads(SEO.read_text("utf-8"))["pages"]


def _structured_data(path: str, base: str, app_url: str) -> str:
    entry = metadata().get(path)
    if entry is None:
        return ""
    values = {"__SITE_URL__": base, "__PAGE_URL__": base + path, "__APP_URL__": app_url}

    def expand(value):
        if isinstance(value, str):
            for key, replacement in values.items():
                value = value.replace(key, replacement)
            return value
        if isinstance(value, list):
            return [expand(item) for item in value]
        if isinstance(value, dict):
            return {key: expand(item) for key, item in value.items()}
        return value

    data = json.dumps(
        {"@context": "https://schema.org", "@graph": expand(entry["schemas"])},
        ensure_ascii=False,
        separators=(",", ":"),
    )
    # JSON lives in a raw-text script element: a configured URL must never close it.
    for char, escaped in (
        ("<", "\\u003c"),
        (">", "\\u003e"),
        ("&", "\\u0026"),
        ("\u2028", "\\u2028"),
        ("\u2029", "\\u2029"),
    ):
        data = data.replace(char, escaped)
    return '<script type="application/ld+json">' + data + "</script>"


def page(lang: str = "ru", slug: str = "") -> str:
    cfg = settings.get()
    base = cfg.landing_url or cfg.public_url
    prefix = "/en" if lang == "en" else ""
    path = "/" if slug in ("", "not-found") else "/features/" if slug == "features" else f"/features/{slug}/"
    canonical = base + prefix + path
    template = STATIC / (f"pages/{slug}.{lang}.html" if slug else f"landing{'-en' if lang == 'en' else ''}.html")
    values = {
        "__APP_URL__": html.escape(cfg.public_url + ("?lang=en" if lang == "en" else "?lang=ru"), quote=True),
        "__LANDING_URL__": html.escape(canonical, quote=True),
        "__OG_IMAGE_URL__": html.escape(base + "/static/images/social-preview.png", quote=True),
        "__RU_URL__": html.escape(base + path, quote=True),
        "__EN_URL__": html.escape(base + "/en" + path, quote=True),
        "__BOT_USERNAME__": html.escape(cfg.bot_username, quote=True),
        "__AGENT_GUIDE_URL__": html.escape(base + "/agents.md", quote=True),
        "__SITE_URL__": html.escape(base, quote=True),
        "__HUB_URL__": html.escape(base + prefix + "/features/", quote=True),
        "__STRUCTURED_DATA__": _structured_data(prefix + path, base, cfg.public_url) if slug != "not-found" else "",
    }
    content = re.sub(r"__[A-Z_]+__", lambda m: values[m.group(0)], template.read_text("utf-8"))
    content = content.replace("</head>", '<link rel="describedby" href="/llms.txt" type="text/markdown">\n</head>')
    return static_assets.version_urls(content)


def response(lang: str = "ru", slug: str = "", status_code: int = 200) -> HTMLResponse:
    content = page(lang, slug)
    csp = security.CSP
    for data in re.findall(r'<script type="application/ld\+json">(.*?)</script>', content, re.DOTALL):
        digest = base64.b64encode(hashlib.sha256(data.encode()).digest()).decode()
        csp = csp.replace("script-src 'self'", f"script-src 'self' 'sha256-{digest}'", 1)
    headers = {"Content-Security-Policy": csp, "Cache-Control": "no-cache"}
    if status_code == 404:
        headers["X-Robots-Tag"] = "noindex, nofollow"
    return HTMLResponse(content, status_code=status_code, headers=headers)


@router.api_route("/landing", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
def preview():
    return response()


@router.api_route("/landing/en/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
@router.api_route("/en/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
def english():
    return response("en")


@router.api_route("/features/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
@router.api_route("/en/features/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
def features(request: Request):
    return response("en" if request.url.path.startswith("/en/") else "ru", "features")


@router.api_route("/features/{slug}/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
@router.api_route("/en/features/{slug}/", methods=["GET", "HEAD"], include_in_schema=False, response_class=HTMLResponse)
def guide(slug: str, request: Request):
    """Serve only editorial pages from the fixed inventory, never user-controlled filenames."""
    if slug not in public_site.GUIDES:
        raise HTTPException(status_code=404)
    return response("en" if request.url.path.startswith("/en/") else "ru", slug)


@router.api_route("/robots.txt", methods=["GET", "HEAD"], include_in_schema=False)
def robots(request: Request):
    cfg = settings.get()
    marketing = cfg.is_landing_host(request.headers.get("host", ""))
    # Crawlers must be able to read the app's noindex response; this is not access control.
    body = "User-agent: *\nAllow: /\n"
    if marketing:
        body += f"Sitemap: {cfg.landing_url}/sitemap.xml\n"
    return Response(body, media_type="text/plain", headers={"Cache-Control": "public, max-age=300"})


@router.api_route("/sitemap.xml", methods=["GET", "HEAD"], include_in_schema=False)
def sitemap(request: Request):
    cfg = settings.get()
    if not cfg.is_landing_host(request.headers.get("host", "")):
        raise HTTPException(status_code=404)
    entries = []
    for path in sorted(public_site.PAGE_PATHS):
        original = path.removeprefix("/en") if path.startswith("/en/") else path
        alternates = "".join(
            f'<xhtml:link rel="alternate" hreflang="{lang}" href="{html.escape(cfg.landing_url + alternate, quote=True)}"/>'
            for lang, alternate in (("ru", original), ("en", "/en" + original), ("x-default", original))
        )
        lastmod = metadata()[path]["lastmod"]
        entries.append(
            f"<url><loc>{html.escape(cfg.landing_url + path)}</loc><lastmod>{html.escape(lastmod)}</lastmod>{alternates}</url>"
        )
    return Response(
        '<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" '
        'xmlns:xhtml="http://www.w3.org/1999/xhtml">' + "".join(entries) + "</urlset>",
        media_type="application/xml",
        headers={"Cache-Control": "public, max-age=300"},
    )
