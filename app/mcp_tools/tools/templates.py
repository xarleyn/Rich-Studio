"""Reusable templates: agents share the editor's fields and safe substitution service."""

from typing import Annotated

from pydantic import Field

from mcp_tools.common import LOCAL, READ, PostArg, ToolDecorator, draft_links
from models import TemplateField
from services import templates


def register(tool: ToolDecorator) -> None:
    @tool(READ)
    async def list_post_templates() -> list:
        """Шаблоны текущей учётки: id, название, version и заполняемые поля."""
        return templates.list_templates()

    @tool(READ)
    async def get_post_template(template_id: str) -> dict:
        """Шаблон целиком: payload с {{key}}, fields и version перед правкой или заполнением."""
        return templates.get_template(template_id)

    @tool(LOCAL)
    async def save_post_template(
        title: str,
        post: PostArg,
        fields: list[TemplateField],
        template_id: str | None = None,
        version: int | None = None,
    ) -> dict:
        """Создать шаблон поста с полями key, label, type (text/multiline/url), required, default.
        В посте ставьте {{key}} в строковые значения. Для обновления нужны template_id и прочитанная version.
        Не публикует и не сохраняет _studio с альтернативными буферами редактора."""
        return templates.save_template(title, post, [field.model_dump() for field in fields], template_id, version)

    @tool(LOCAL)
    async def draft_from_template(
        template_id: str,
        version: int,
        values: Annotated[dict[str, str], Field(description="Заполняемые значения по key из fields")],
        title: str = "",
    ) -> dict:
        """Заполнить шаблон и создать отдельный черновик без публикации; пропуски берутся из default.
        Обязательные поля и URL проверяются, HTML/Markdown экранируются. Возвращает ссылку для открытия."""
        draft = templates.create_draft(template_id, values, version, title)
        return {**draft, **draft_links(draft["id"])}
