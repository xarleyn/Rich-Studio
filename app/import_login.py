"""Interactive local setup; never expose Telegram authorization/session secrets through HTTP or logs."""

import argparse
import asyncio
import getpass
import os

from telethon.errors import SessionPasswordNeededError
from telethon.sessions import StringSession

import settings
from services import accounts, imports
from telegram.import_client import make_client


async def login(account: int | None) -> None:
    cfg = settings.get()
    if not cfg.import_api_id or not cfg.import_api_hash:
        raise SystemExit("Задайте TG_IMPORT_API_ID и TG_IMPORT_API_HASH (или TG_IMPORT_API_HASH_FILE).")
    with accounts.scope(account):
        path = imports.session_path()
        if path.exists():
            raise SystemExit(
                "Сессия уже настроена. Для замены сначала отзовите её в Telegram и удалите локальный файл."
            )
        client = make_client()
        try:
            await client.connect()
            phone = getpass.getpass("Телефон Telegram (ввод скрыт): ")
            await client.send_code_request(phone)
            try:
                await client.sign_in(phone, getpass.getpass("Код Telegram (ввод скрыт): "))
            except SessionPasswordNeededError:
                await client.sign_in(password=getpass.getpass("Пароль двухэтапной аутентификации: "))
            me = await client.get_me()
            if not me or me.bot or (account is not None and me.id != account):
                await client.log_out()
                raise SystemExit("Вход выполнен другой учёткой. Сессия отозвана и не сохранена.")
            path.parent.mkdir(parents=True, exist_ok=True)
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as file:
                file.write(StringSession.save(client.session))
            print("Подключение импорта сохранено. Секрет сессии не входит в экспорт студии.")
        finally:
            await client.disconnect()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--account", type=int, help="Telegram ID учётки; без параметра — личное пространство владельца")
    args = parser.parse_args()
    if args.account is not None and args.account <= 0:
        parser.error("--account должен быть положительным Telegram ID")
    asyncio.run(login(args.account))


if __name__ == "__main__":
    main()
