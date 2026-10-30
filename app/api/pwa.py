"""The public service worker needs root scope; account content is never cached by it."""

from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import FileResponse

router = APIRouter()
STATIC = Path(__file__).resolve().parent.parent / "static"


@router.get("/sw.js", include_in_schema=False)
def service_worker():
    return FileResponse(
        STATIC / "sw.js",
        media_type="text/javascript",
        headers={"Cache-Control": "no-cache", "Service-Worker-Allowed": "/"},
    )
