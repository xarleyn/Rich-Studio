"""Public, credential-free discovery and setup instructions for service-consuming agents."""

import re
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import JSONResponse, Response

import settings

router = APIRouter()
DOCS = Path(__file__).resolve().parent.parent / "bundled" / "agent-docs"


def document(name: str) -> str:
    """Only fixed names from our routes; URLs come from configuration, never request headers."""
    cfg = settings.get()
    public = cfg.landing_url or cfg.public_url
    if cfg.public:
        credential_instructions = (
            f"Open {cfg.public_url}/ and sign in through Telegram. Open your account menu → "
            "‘Токены API и MCP’ (English: ‘API and MCP tokens’). Create a separate token for this client, "
            "choose ‘Черновики’ / ‘Drafts’ (draft) and an appropriate expiry. The secret is shown once. "
            "Use read for read-only work; choose publish only when you want the agent to publish. "
            "Keep the token in your client's secret setting or the RICHPOST_TOKEN environment variable."
        )
    else:
        credential_instructions = (
            "Ask the operator of this private installation for a separate named token with the draft role "
            "(or read for read-only work). Only the operator manages these tokens with the studio CLI. "
            "Keep the token in your client's secret setting or the RICHPOST_TOKEN environment variable."
        )
    values = {
        "__SITE_URL__": public,
        "__APP_URL__": cfg.public_url,
        "__TOKEN_STEPS__": credential_instructions,
    }
    return re.sub(r"__[A-Z_]+__", lambda m: values[m.group(0)], (DOCS / name).read_text("utf-8"))


def text_response(body: str, *, markdown: bool = False) -> Response:
    return Response(
        body,
        media_type="text/markdown" if markdown else "text/plain",
        headers={"Cache-Control": "public, max-age=300", "Link": '</llms.txt>; rel="describedby"'},
    )


@router.api_route("/llms.txt", methods=["GET", "HEAD"], include_in_schema=False)
def llms():
    return text_response(document("llms.txt"), markdown=True)


@router.api_route("/agents.md", methods=["GET", "HEAD"], include_in_schema=False)
def setup():
    return text_response(document("setup.md"), markdown=True)


@router.api_route("/skills/richpost/SKILL.md", methods=["GET", "HEAD"], include_in_schema=False)
def skill():
    return text_response(document("SKILL.md"), markdown=True)


@router.api_route("/llms-full.txt", methods=["GET", "HEAD"], include_in_schema=False)
def full():
    # Both views share their source, so the short and full instructions cannot drift.
    return text_response(document("setup.md") + "\n---\n\n" + document("SKILL.md"), markdown=True)


@router.api_route("/agents.txt", methods=["GET", "HEAD"], include_in_schema=False)
def agents():
    cfg = settings.get()
    public = cfg.landing_url or cfg.public_url
    return text_response(
        "# RichPost agent discovery\n# Format: https://agents-txt.com (draft)\n"
        f"# JSON: {public}/agents.json\n# Setup and human-issued token: {public}/agents.md\n"
        f"MCP: {cfg.public_url}/mcp\n\nSkills: {public}/skills/richpost/SKILL.md\n"
    )


@router.api_route("/agents.json", methods=["GET", "HEAD"], include_in_schema=False)
def catalog():
    cfg = settings.get()
    public = cfg.landing_url or cfg.public_url
    return JSONResponse(
        {
            "$schema": "https://agents-txt.com/schema/agents-json/v1.0.json",
            "version": "1.0",
            "standard": "https://agents-txt.com",
            "site": {"name": "RichPost", "url": public, "description": f"Setup instructions: {public}/agents.md"},
            "mcp": [
                {
                    "url": cfg.public_url + "/mcp",
                    "type": "streamable-http",
                    "description": "Telegram post drafts, previews and templates. Requires a human-issued bearer token; "
                    "read the setup instructions before connecting. Publishing depends on token permissions.",
                }
            ],
            "skills": [
                {"url": public + "/skills/richpost/SKILL.md", "description": "Connect and use RichPost safely."}
            ],
        },
        headers={"Cache-Control": "public, max-age=300", "Link": '</agents.md>; rel="describedby"'},
    )
