"""Personal API/MCP token management is available only to a Telegram browser session."""

from typing import Literal

from fastapi import APIRouter, Depends
from pydantic import BaseModel, ConfigDict, Field, field_validator

import security
from errors import StudioError
from services import account_tokens


def browser_owner() -> None:
    who = security.current()
    if who.telegram_id is None or not who.account_session:
        raise StudioError("Управление токенами доступно после входа через Telegram", status=403)


router = APIRouter(tags=["service"], dependencies=[Depends(browser_owner)])


class TokenReq(BaseModel):
    model_config = ConfigDict(extra="forbid")
    name: str = Field(min_length=1, max_length=80, description="Название подключения, например Claude laptop")
    level: Literal["read", "draft", "publish"] = Field(default="draft", description="Чтение, черновики или публикация")
    expires_in_days: int | None = Field(
        default=90, ge=1, le=365, strict=True, description="Срок от 1 до 365 дней; null — без срока"
    )

    @field_validator("name")
    @classmethod
    def clean_name(cls, value: str) -> str:
        value = value.strip()
        if not value or not value.isprintable():
            raise ValueError("Название должно содержать видимый текст без управляющих символов")
        return value


@router.get("/api/account/tokens", summary="Мои токены API/MCP и адрес подключения")
def listing():
    """Только Telegram-сессия владельца учётки. Секреты и хэши не возвращаются; даты — Unix seconds.
    При создании токена история сокращается до 100 последних неактивных; всего не более 120 записей.
    Токены других учёток и сервера не видны."""
    return account_tokens.listing()


@router.post("/api/account/tokens", status_code=201, summary="Создать персональный токен API/MCP")
def create(req: TokenReq):
    """Секрет возвращается только здесь, один раз. До 20 действующих токенов и 60 попыток создания в час.
    Уровень admin недоступен; выданный токен не может управлять токенами или входить в редактор."""
    return account_tokens.create(req.name, req.level, req.expires_in_days)


@router.delete("/api/account/tokens/{token_id}", summary="Отозвать свой токен API/MCP")
def revoke(token_id: str):
    """После отзыва новые запросы с этим токеном отклоняются. Уже принятые операции и расписание сохраняются.
    Повторный отзыв безопасен; чужой или неизвестный id даёт 404. Telegram-сессия остаётся открытой."""
    return account_tokens.revoke(token_id)
