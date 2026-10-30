"""Explicit public page inventory, shared by routing, the guard and sitemap."""

GUIDES = ("telegram-tables", "telegram-markdown", "scheduled-posts", "ai-agents")
PAGE_PATHS = {"/", "/en/"} | {f"{prefix}/features/{slug}/" for prefix in ("", "/en") for slug in GUIDES}
PREVIEW_PATHS = {"/landing", "/landing/en/"}
AGENT_PATHS = {"/llms.txt", "/llms-full.txt", "/agents.txt", "/agents.json", "/agents.md", "/skills/richpost/SKILL.md"}
PUBLIC_PATHS = PAGE_PATHS | PREVIEW_PATHS | AGENT_PATHS | {"/robots.txt", "/sitemap.xml"}


def is_page(path: str) -> bool:
    return path in PAGE_PATHS | PREVIEW_PATHS
