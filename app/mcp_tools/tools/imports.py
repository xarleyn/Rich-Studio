"""Telegram import tools."""

from mcp_tools.common import FETCH, READ, ToolDecorator, draft_links
from services import imports


def register(tool: ToolDecorator) -> None:
    @tool(READ)
    async def telegram_import_status() -> dict:
        """Способы импорта Telegram для текущей учётки; секреты подключения не возвращаются."""
        return imports.status()

    @tool(FETCH)
    async def import_telegram_post(url: str) -> dict:
        """Импортировать Telegram-сообщение по ссылке t.me в новый черновик: визуальные блоки, HTML и
        Rich Markdown. Ничего не публикует, исходный черновик не меняет. Проверяйте warnings: некоторые
        элементы могут быть заменены отметкой. Не считайте импорт успешным при ошибке доступа/подключения."""
        result = await imports.import_post(url)
        return {**result, **draft_links(result["draft"]["id"])}
