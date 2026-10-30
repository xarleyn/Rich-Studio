"""Reusable post templates in the current account's database."""

import json
import time
import uuid

from errors import Conflict, NotFound, StudioError

from .db import conn, row

MAX_TEMPLATES = 200
MAX_BYTES = 1024 * 1024


def list_templates() -> list[dict]:
    with conn() as c:
        return [
            row(r, ("fields",))  # type: ignore[misc]
            for r in c.execute(
                "select id, title, fields, version, created_at, updated_at from post_templates order by updated_at desc, id"
            )
        ]


def get_template(tid: str) -> dict:
    with conn() as c:
        result = row(c.execute("select * from post_templates where id=?", (tid,)).fetchone(), ("payload", "fields"))
    if result is None:
        raise NotFound("Шаблон не найден.")
    return result


def save_template(
    title: str, payload: dict, fields: list[dict], tid: str | None = None, version: int | None = None
) -> dict:
    data, schema = (json.dumps(value, ensure_ascii=False) for value in (payload, fields))
    if len((title + data + schema).encode("utf-8")) > MAX_BYTES:
        raise StudioError("Шаблон больше 1 МБ.", status=413)
    now = int(time.time())
    with conn() as c:
        c.execute("begin immediate")
        if tid is None:
            if c.execute("select count(*) from post_templates").fetchone()[0] >= MAX_TEMPLATES:
                raise StudioError("Лимит шаблонов исчерпан. Удалите ненужные шаблоны.", status=413)
            tid = uuid.uuid4().hex[:12]
            c.execute(
                "insert into post_templates(id,title,payload,fields,created_at,updated_at) values(?,?,?,?,?,?)",
                (tid, title, data, schema, now, now),
            )
        else:
            if not c.execute("select 1 from post_templates where id=?", (tid,)).fetchone():
                raise NotFound("Шаблон не найден.")
            result = c.execute(
                "update post_templates set title=?, payload=?, fields=?, updated_at=?, version=version+1 where id=? and version=?",
                (title, data, schema, now, tid, version),
            )
            if result.rowcount != 1:
                raise Conflict("Шаблон изменился. Откройте его заново перед сохранением.")
    return get_template(tid)


def delete_template(tid: str, version: int) -> None:
    with conn() as c:
        result = c.execute("delete from post_templates where id=? and version=?", (tid, version))
        if result.rowcount != 1:
            if c.execute("select 1 from post_templates where id=?", (tid,)).fetchone():
                raise Conflict("Шаблон изменился. Откройте его заново перед удалением.")
            raise NotFound("Шаблон не найден.")


def templates_using(ref: str) -> list[dict]:
    """Templates needing an uploaded file; include defaults of configurable media fields."""
    with conn() as c:
        return [
            dict(r)
            for r in c.execute(
                "select id, title from post_templates where instr(payload, ?) > 0 or instr(fields, ?) > 0",
                (json.dumps(ref, ensure_ascii=False), json.dumps(ref, ensure_ascii=False)),
            )
        ]
