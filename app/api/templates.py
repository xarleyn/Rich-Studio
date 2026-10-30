"""Account-local templates and new drafts filled from their form fields."""

from fastapi import APIRouter, Query

from models import TemplateDraftReq, TemplateReq
from services import templates

router = APIRouter(tags=["templates"])


@router.get("/api/templates", summary="Шаблоны постов текущей учётки и их заполняемые поля")
def list_templates():
    return templates.list_templates()


@router.get("/api/templates/{tid}", summary="Шаблон целиком: пост, поля и version")
def get_template(tid: str):
    return templates.get_template(tid)


@router.post("/api/templates", status_code=201, summary="Сохранить пост как шаблон с переменными {{key}}")
def create_template(req: TemplateReq):
    """Переменные заменяются только в строковых значениях. Все переменные должны иметь описание в fields."""
    return templates.save_template(req.title, req.post, [field.model_dump() for field in req.fields])


@router.put("/api/templates/{tid}", summary="Изменить шаблон; version защищает от перезаписи чужой правки")
def update_template(tid: str, req: TemplateReq):
    return templates.save_template(req.title, req.post, [field.model_dump() for field in req.fields], tid, req.version)


@router.delete("/api/templates/{tid}", summary="Удалить шаблон; созданные из него черновики остаются")
def delete_template(tid: str, version: int = Query(..., ge=1, description="Версия прочитанного шаблона")):
    templates.delete_template(tid, version)
    return {"ok": True}


@router.post("/api/templates/{tid}/drafts", status_code=201, summary="Заполнить поля шаблона и создать новый черновик")
def create_draft(tid: str, req: TemplateDraftReq):
    """Проверяет обязательные поля и URL; экранирует значения для HTML/Markdown. Не публикует пост и не копирует канал."""
    return templates.create_draft(tid, req.values, req.version, req.title)
