"""Public product page, served on the marketing host and at /landing for local preview."""

import html
import re
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import HTMLResponse

import settings

router = APIRouter()
TEMPLATE = Path(__file__).resolve().parent.parent / "static" / "landing.html"


def page() -> str:
    cfg = settings.get()
    values = {
        "__APP_URL__": html.escape(cfg.public_url, quote=True),
        "__LANDING_URL__": html.escape(cfg.landing_url or cfg.public_url + "/landing", quote=True),
        "__BOT_USERNAME__": html.escape(cfg.bot_username, quote=True),
    }
    return re.sub(r"__(APP_URL|LANDING_URL|BOT_USERNAME)__", lambda m: values[m.group(0)], TEMPLATE.read_text("utf-8"))


@router.get("/landing", include_in_schema=False, response_class=HTMLResponse)
def preview():
    return page()
