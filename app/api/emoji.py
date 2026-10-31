"""Custom emoji packs for the editor panel and preview pictures."""

from fastapi import APIRouter
from fastapi.responses import FileResponse
from pydantic import BaseModel

from services import emoji

router = APIRouter()


class PackReq(BaseModel):
    name: str


@router.get("/api/emoji/packs", tags=["emoji"], summary="Паки кастомных эмодзи учётки для панели редактора")
def packs_list():
    return {"packs": emoji.list_packs()}


@router.post(
    "/api/emoji/packs", tags=["emoji"], summary="Добавить пак: короткое имя набора или ссылка t.me/addemoji/<код>"
)
async def packs_add(req: PackReq):
    added = await emoji.add_pack(req.name)
    return {"packs": emoji.list_packs(), "added": added}


@router.delete(
    "/api/emoji/packs/{name}",
    tags=["emoji"],
    summary="Убрать пак из панели; скачанные картинки остаются в кэше на случай возврата",
)
def packs_remove(name: str):
    emoji.remove_pack(name)
    return {"packs": emoji.list_packs()}


def _picture(path: str, media_type: str) -> FileResponse:
    r = FileResponse(path, media_type=media_type)
    # the picture never renders studio markup, and the id is unique forever
    r.headers["Content-Security-Policy"] = "sandbox; default-src 'none'"
    r.headers["Cache-Control"] = "private, immutable, max-age=31536000"
    return r


@router.get(
    "/api/emoji/img/{custom_emoji_id}",
    tags=["emoji"],
    summary="Статичная картинка эмодзи (webp-миниатюра); скачивается при первом запросе",
)
async def emoji_img(custom_emoji_id: str):
    path, media_type = await emoji.static_file(custom_emoji_id)
    return _picture(path, media_type)


@router.get(
    "/api/emoji/file/{custom_emoji_id}",
    tags=["emoji"],
    summary="Полный файл эмодзи: webm-видео, tgs-анимация или webp",
)
async def emoji_file(custom_emoji_id: str):
    path, media_type = await emoji.full_file(custom_emoji_id)
    return _picture(path, media_type)
