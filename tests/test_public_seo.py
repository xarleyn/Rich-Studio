"""Search-facing routes must be public, canonical and separate from account data."""

import base64
import hashlib
import json
import re
from datetime import date
from pathlib import Path
from xml.etree import ElementTree

import pytest

import public_site
import settings
import static_assets
from api import landing


@pytest.fixture
def marketing(client, monkeypatch):
    cfg = settings.get()
    monkeypatch.setattr(cfg, "public", True)
    monkeypatch.setattr(cfg, "landing_url", "https://example.com")
    monkeypatch.setattr(cfg, "public_url", "https://app.example.com")
    monkeypatch.setattr(cfg, "allowed_hosts", {*cfg.allowed_hosts, "example.com", "app.example.com"})
    return client


def test_sitemap_contains_only_public_canonical_pages(marketing):
    r = marketing.get("/sitemap.xml", headers={"Host": "example.com"})
    assert r.status_code == 200
    root = ElementTree.fromstring(r.content)  # noqa: S314 - trusted test output
    urls = {loc.text for loc in root.iter("{http://www.sitemaps.org/schemas/sitemap/0.9}loc")}
    assert urls == {"https://example.com" + path for path in public_site.PAGE_PATHS}
    assert set(landing.metadata()) == public_site.PAGE_PATHS
    for entry in root:
        modified = entry.find("{http://www.sitemaps.org/schemas/sitemap/0.9}lastmod")
        assert modified is not None and date.fromisoformat(modified.text) <= date.today()
        alternates = entry.findall("{http://www.w3.org/1999/xhtml}link")
        assert {item.attrib["hreflang"] for item in alternates} == {"ru", "en", "x-default"}
    for url in urls:
        path = url.removeprefix("https://example.com")
        response = marketing.get(path, headers={"Host": "example.com"})
        assert response.status_code == 200
        assert response.headers.get("x-robots-tag") is None
        assert f'<link rel="canonical" href="{url}">' in response.text
        assert response.text.count("<h1") == 1
        assert 'hreflang="ru"' in response.text and 'hreflang="en"' in response.text
        assert "__APP_URL__" not in response.text
        assert "script-src 'self'" in response.headers["content-security-policy"]
        head = marketing.head(path, headers={"Host": "example.com"})
        assert head.status_code == 200 and head.content == b""
        assert head.headers["content-type"] == response.headers["content-type"]
        assert head.headers["content-length"] == response.headers["content-length"]


def test_languages_are_explicit_and_reciprocal(marketing):
    for path in ("/", "/features/telegram-tables/"):
        ru = marketing.get(path, headers={"Host": "example.com", "Accept-Language": "en"})
        en = marketing.get("/en" + path, headers={"Host": "example.com", "Accept-Language": "ru"})
        assert '<html lang="ru">' in ru.text and '<html lang="en">' in en.text
        assert re.findall(r'<link rel="alternate"[^>]+>', ru.text) == re.findall(
            r'<link rel="alternate"[^>]+>', en.text
        )
        assert '?lang=en"' in en.text
        assert not re.search("[а-яА-ЯёЁ]", en.text.replace('aria-label="Русская версия"', ""))


def test_app_and_local_previews_are_noindex(marketing):
    for path in ("/", "/en/", "/landing", "/landing/en/"):
        response = marketing.get(path, headers={"Host": "app.example.com"})
        assert response.status_code == 200
        assert response.headers["x-robots-tag"] == "noindex, nofollow"
    assert "data-nosnippet" in marketing.get("/", headers={"Host": "example.com"}).text
    private = marketing.get("/api/drafts", headers={"Host": "app.example.com"})
    assert private.status_code == 401 and private.headers["x-robots-tag"] == "noindex, nofollow"


def test_robots_allows_reading_noindex_and_advertises_only_marketing_sitemap(marketing):
    r = marketing.get("/robots.txt", headers={"Host": "example.com"})
    assert r.status_code == 200 and r.headers["content-type"].startswith("text/plain")
    assert "Allow: /" in r.text and "Sitemap: https://example.com/sitemap.xml" in r.text
    app = marketing.get("/robots.txt", headers={"Host": "app.example.com"})
    assert "Disallow: /" not in app.text and "Sitemap:" not in app.text
    assert marketing.get("/sitemap.xml", headers={"Host": "app.example.com"}).status_code == 404


def test_missing_marketing_pages_return_404_and_slashes_redirect(marketing):
    for path in (
        "/missing-page",
        "/mcp-missing",
        "/docs-missing",
        "/features/no-such-guide/",
        "/en/features/no-such-guide/",
    ):
        r = marketing.get(path, headers={"Host": "example.com"}, follow_redirects=False)
        assert r.status_code == 404 and "location" not in r.headers
        assert r.headers["content-type"].startswith("text/html") and r.text.count("<h1") == 1
        assert "<a" in r.text and "/features/" in r.text
        assert marketing.head(path, headers={"Host": "example.com"}).status_code == 404
    for path in public_site.PAGE_PATHS - {"/"}:
        r = marketing.get(
            path.rstrip("/") + "?utm_source=telegram&x=1", headers={"Host": "example.com"}, follow_redirects=False
        )
        assert r.status_code == 308 and r.headers["location"].endswith(path + "?utm_source=telegram&x=1")
        assert (
            marketing.head(path.rstrip("/"), headers={"Host": "example.com"}, follow_redirects=False).status_code == 308
        )
    assert marketing.get("/en/no-such-page", headers={"Host": "example.com"}).text.startswith(
        '<!doctype html>\n<html lang="en">'
    )


def test_structured_data_social_previews_and_visible_metadata_agree(marketing):
    for path in sorted(public_site.PAGE_PATHS):
        response = marketing.get(path, headers={"Host": "example.com"})
        content = response.text
        entry = landing.metadata()[path]
        assert f"<title>{entry['title']}</title>" in content
        assert f'<meta name="description" content="{entry["description"]}">' in content
        assert '<meta name="twitter:card" content="summary_large_image">' in content
        assert re.search(
            r'<meta property="og:image" content="https://example.com/static/images/social-preview.png\?v=[a-f0-9]{12}">',
            content,
        )
        data = re.findall(r'<script type="application/ld\+json">(.*?)</script>', content, re.DOTALL)
        assert len(data) == 1
        graph = json.loads(data[0])
        assert graph["@context"] == "https://schema.org" and graph["@graph"]
        assert not re.search(r"__[A-Z_]+__", content)
        digest = base64.b64encode(hashlib.sha256(data[0].encode()).digest()).decode()
        script_policy = response.headers["content-security-policy"].split("script-src ", 1)[1].split(";", 1)[0]
        assert f"'sha256-{digest}'" in script_policy and "'unsafe-inline'" not in script_policy
        assert response.headers["cache-control"] == "no-cache"
        if "/features/" in path:
            assert any(item["@type"] == "BreadcrumbList" for item in graph["@graph"])


def test_json_ld_cannot_break_out_of_script_from_a_configured_url(marketing, monkeypatch):
    monkeypatch.setattr(
        settings.get(), "public_url", "https://app.example.com/?x=</script><script>alert(1)</script>&y=2"
    )
    content = marketing.get("/", headers={"Host": "example.com"}).text
    data = re.findall(r'<script type="application/ld\+json">(.*?)</script>', content, re.DOTALL)
    assert len(data) == 1 and "<" not in data[0] and "&" not in data[0]
    assert "<script>alert(1)</script>" not in content
    assert "</script>" in json.dumps(json.loads(data[0]))


def test_icons_discovery_head_and_fingerprint_cache_are_usable(marketing):
    for path in ("/favicon.ico", "/robots.txt", "/sitemap.xml", *public_site.AGENT_PATHS):
        response = marketing.get(path, headers={"Host": "example.com"})
        head = marketing.head(path, headers={"Host": "example.com"})
        assert response.status_code == head.status_code == 200
        assert head.content == b"" and head.headers["content-type"] == response.headers["content-type"]
    favicon = marketing.get("/favicon.ico", headers={"Host": "example.com"})
    assert favicon.content.startswith(b"\x00\x00\x01\x00")
    asset = "/static/landing.css"
    digest = static_assets.fingerprint(asset)
    for query, immutable in (
        ("", False),
        ("?v=old", False),
        (f"?v={digest}", True),
        (f"?v={digest}&v={digest}", False),
    ):
        response = marketing.get(asset + query, headers={"Host": "example.com", "Accept-Encoding": "identity"})
        assert response.status_code == 200
        assert ("immutable" in response.headers["cache-control"]) is immutable
        assert response.content == (landing.STATIC / "landing.css").read_bytes()
    assert not static_assets.fingerprint("/static/../../.env")
    assert not static_assets.fingerprint("/static/%2e%2e/%2e%2e/.env")
    assert not static_assets.fingerprint("/api/drafts")
    assert not static_assets.fingerprint("/static/%00")
    for encoding in ("identity", "br"):
        malformed = marketing.get("/static/%00", headers={"Host": "example.com", "Accept-Encoding": encoding})
        assert malformed.status_code == 404


def test_precompressed_css_negotiates_without_changing_source_or_private_caching(marketing):
    asset = "/static/landing.css"
    with marketing.stream("GET", asset, headers={"Host": "example.com", "Accept-Encoding": "gzip, br"}) as response:
        assert response.status_code == 200 and response.headers["content-encoding"] == "br"
        assert response.headers["content-type"].startswith("text/css")
        assert "Accept-Encoding" in response.headers["vary"]
        assert b"".join(response.iter_raw()) == (landing.STATIC / "landing.css.br").read_bytes()
    response = marketing.get(asset, headers={"Host": "example.com", "Accept-Encoding": "br;q=0, *;q=1"})
    assert "content-encoding" not in response.headers
    assert response.content == (landing.STATIC / "landing.css").read_bytes()
    # Multipart boundaries are not a Brotli stream; range responses use identity.
    response = marketing.get(
        asset,
        headers={"Host": "example.com", "Accept-Encoding": "br", "Range": "bytes=0-9,30-39"},
    )
    assert response.status_code == 206 and "content-encoding" not in response.headers
    assert response.headers["content-type"].startswith("multipart/byteranges")
    source = (landing.STATIC / "landing.css").read_bytes()
    assert source[:10] in response.content and source[30:40] in response.content
    private = marketing.get("/api/drafts", headers={"Host": "app.example.com"})
    assert private.status_code == 401 and "immutable" not in private.headers.get("cache-control", "")


def test_checked_in_public_derivatives_match_their_sources():
    manifest = json.loads(
        (Path(__file__).resolve().parent.parent / "app/bundled/public-assets.json").read_text("utf-8")
    )

    def sha(path):
        return hashlib.sha256((landing.STATIC / path).read_bytes()).hexdigest()

    for path, entry in manifest["compressed"].items():
        assert sha(path) == entry["source_sha256"], "Regenerate Brotli sidecars with scripts/optimize-public-assets.py"
        assert sha(path + ".br") == entry["brotli_sha256"]
    for path, entry in manifest["derived"].items():
        assert sha(entry["source"]) == entry["source_sha256"] and sha(path) == entry["sha256"]
    assert sha("fonts/manrope.ttf") == manifest["font"]["source_sha256"]
    assert sha("fonts/manrope.woff2") == manifest["font"]["sha256"]
