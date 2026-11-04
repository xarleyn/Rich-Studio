# SQLite WAL fix после локального запуска

1 ноября 2026 года продолжительный запуск Rust-контейнера обнаружил зависание
`/api/health`, `/api/drafts` и фоновых операций SQLite. `/healthz` продолжал отвечать:
это была блокировка хранилища, а не завершение HTTP-процесса.

## Причина и изменение

Первоначальная цепочка `rusqlite 0.38.0 → libsqlite3-sys 0.36.0` включала SQLite 3.51.1.
Снятый с зависшего Linux-процесса стек показал `pthread_mutex_lock → findReusableFd →
unixOpen` у новых соединений и `unixLock → sqlite3WalClose` у закрывающегося соединения
планировщика. В исходнике SQLite 3.51.1 `unixLock` удерживает mutex inode и вызывает
`unixIsSharingShmNode`, который берёт глобальный Unix mutex; `findReusableFd` получает
их в обратном порядке.

SQLite [подтвердила исправление этого deadlock в 3.51.2](https://www.sqlite.org/releaselog/3_51_2.html).
Обновлена существующая зависимость: `rusqlite 0.41.1 → libsqlite3-sys 0.39.1 → SQLite
3.54.1`, также обновился `hashlink`. В `decode_row` адаптирован только новый API
`column_names()`, который возвращает iterator. Политика транзакций, WAL, SQL-миграции,
схема базы и wire-контракты не изменены.

## Регрессии

- `bundled_sqlite_contains_the_unix_wal_deadlock_fix`: на прежней зависимости сначала
  получен FAIL с фактической SQLite 3.51.1; запрещает возврат уязвимого исполнения.
- `wal_connections_open_and_close_concurrently_without_hanging`: четыре потока,
  4000 открытий/чтений/закрытий WAL-базы, один из потоков также пишет. Постоянного
  удерживающего соединения нет — проверяется закрытие последнего WAL-соединения.
  Отдельный дочерний процесс ограничен 45 секундами: нативная взаимоблокировка не
  может повесить весь test runner.
- Реальный Linux-стенд проверяется отдельным конкурентным HTTP-прогоном и затем
  остаётся под запросами здоровья и чтения черновиков не менее шести минут, вместе
  с активными polling, планировщиком, lease и housekeeping.

## Повторная приёмка

Повторная локальная приёмка завершена успешно:

| Проверка | Результат |
|---|---|
| Полный `make check` на Windows через GNU make / Git Bash | PASS, exit 0 |
| fmt, Clippy с `-D warnings`, nextest, doctests, deny, machete, release | PASS; 168 тестов, 0 skipped |
| Python-эталон | 653 passed, 1 ожидаемый Windows skip для POSIX 0600 |
| JavaScript / типы / санитайзер / preview | PASS; прежнее известное `md-nested` расхождение сохраняется |
| Исходные браузерные сценарии на Windows Rust-бинарнике | 47 сценариев, 136/136 проверок |
| Swagger / ReDoc | 71 операция, 1400/390 px, без внешних загрузок и ошибок JS/CSP |
| Установка локального Linux-стенда | 22/22: авторизация, публикация в mock, идемпотентность, MCP, браузер и сохранность после остановки |
| Конкурентный и длительный прогон Linux-стенда | 928 запросов за 6 минут, включая 364 health; без зависаний |
| Проверка установленного Docker-образа | PASS: uid 10001, без Python, read-only root, token/draft persistence, SIGTERM exit 0 |
| Восстановление Litestream | PASS: токен и черновик восстановлены из изолированной реплики |

Текущий образ — `rich-studio-local:rust-sqlitefix-20261101`, также обновлён локальный
алиас `rich-studio-rust-candidate`. Его image ID:
`sha256:61c82e1b614a33f7db1814415880840b813c4d06216d7f8bcdd8db20b3af64bf`.
Windows release SHA-256:
`e0f0cdf8f7d97dbab518ab82ce9df2e803b6d9c035e023e769284ea41df3ca9e`.

Сводный машинный отчёт: `work/rust-sqlite-fix-acceptance.json`. Исходный манифест
содержит 346 файлов; относительно прежнего исполнения изменились только Cargo.toml,
Cargo.lock, src/store/db.rs и tests/rust/runtime.rs. Документация дополнена отдельно.
Первоначальные артефакты и зависший образ сохранены; первоначальный digest
из `RUST_MIGRATION_ACCEPTANCE.md` не заменён задним числом. GitHub CI и production
deployment для этих незакоммиченных изменений не выполнялись.

Продовый контейнер и его конфигурация не менялись. Реальные Telegram-сообщения,
настройка отдельного тестового бота и продовый переход в эту проверку не входят.
