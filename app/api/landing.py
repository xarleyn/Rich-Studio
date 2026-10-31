"""Public product page, served on the marketing host and at /landing for local preview."""

import html
import re
from pathlib import Path

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, Response

import public_site
import settings

router = APIRouter()
STATIC = Path(__file__).resolve().parent.parent / "static"


def page(lang: str = "ru", slug: str = "") -> str:
    cfg = settings.get()
    base = cfg.landing_url or cfg.public_url
    prefix = "/en" if lang == "en" else ""
    path = f"/features/{slug}/" if slug else "/"
    canonical = base + prefix + path
    template = STATIC / (f"pages/{slug}.{lang}.html" if slug else f"landing{'-en' if lang == 'en' else ''}.html")
    values = {
        "__APP_URL__": html.escape(cfg.public_url + ("?lang=en" if lang == "en" else "?lang=ru"), quote=True),
        "__LANDING_URL__": html.escape(canonical, quote=True),
        "__OG_IMAGE_URL__": html.escape(base + "/static/images/landing-editor.jpg", quote=True),
        "__RU_URL__": html.escape(base + path, quote=True),
        "__EN_URL__": html.escape(base + "/en" + path, quote=True),
        "__BOT_USERNAME__": html.escape(cfg.bot_username, quote=True),
        "__AGENT_GUIDE_URL__": html.escape(base + "/agents.md", quote=True),
    }
    content = re.sub(r"__[A-Z_]+__", lambda m: values[m.group(0)], template.read_text("utf-8"))
    return content.replace("</head>", '<link rel="describedby" href="/llms.txt" type="text/markdown">\n</head>')


@router.get("/landing", include_in_schema=False, response_class=HTMLResponse)
def preview():
    return page()


@router.get("/landing/en/", include_in_schema=False, response_class=HTMLResponse)
@router.get("/en/", include_in_schema=False, response_class=HTMLResponse)
def english():
    return page("en")


@router.get("/features/{slug}/", include_in_schema=False, response_class=HTMLResponse)
@router.get("/en/features/{slug}/", include_in_schema=False, response_class=HTMLResponse)
def guide(slug: str, request: Request):
    """Serve only editorial pages from the fixed inventory, never user-controlled filenames."""
    if slug not in public_site.GUIDES:
        raise HTTPException(status_code=404)
    return page("en" if request.url.path.startswith("/en/") else "ru", slug)


@router.get("/robots.txt", include_in_schema=False)
def robots(request: Request):
    cfg = settings.get()
    marketing = cfg.is_landing_host(request.headers.get("host", ""))
    # Crawlers must be able to read the app's noindex response; this is not access control.
    body = "User-agent: *\nAllow: /\n"
    if marketing:
        body += f"Sitemap: {cfg.landing_url}/sitemap.xml\n"
    return Response(body, media_type="text/plain")


@router.get("/sitemap.xml", include_in_schema=False)
def sitemap(request: Request):
    cfg = settings.get()
    if not cfg.is_landing_host(request.headers.get("host", "")):
        raise HTTPException(status_code=404)
    urls = "".join(
        f"<url><loc>{html.escape(cfg.landing_url + path)}</loc></url>" for path in sorted(public_site.PAGE_PATHS)
    )
    return Response(
        '<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
        + urls
        + "</urlset>",
        media_type="application/xml",
    )
