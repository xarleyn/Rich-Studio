"""Errors services raise. The API layer turns them into HTTP responses; background loops store them."""

from typing import Any


class StudioError(Exception):
    """A request the studio refuses. `detail` is a message or a dict like {"message", "errors"}."""

    status = 400

    def __init__(self, detail: Any, status: int | None = None):
        super().__init__(detail if isinstance(detail, str) else detail.get("message", str(detail)))
        self.detail = detail
        if status is not None:
            self.status = status


class StorageFull(RuntimeError):
    """An account database cannot grow. Not StudioError: an earlier send may already have succeeded.

    Idempotency must retain an uncertain key when this interrupts recording a Telegram result.
    """

    def __init__(self):
        super().__init__(
            "Лимит базы учётки исчерпан. Удалите ненужные черновики. "
            "Если выполнялась публикация, проверьте канал перед повтором: сообщение могло уйти."
        )


class NotFound(StudioError):
    status = 404


class Invalid(StudioError):
    """The payload did not pass validation against the Bot API spec."""

    status = 422


class Conflict(StudioError):
    status = 409


class Upstream(StudioError):
    """Telegram failed or could not be reached."""

    status = 502


class MayHaveGone(Upstream):
    """A network error during a send: the post may be in the channel. Never retried automatically."""
