# Приёмка переноса backend на Rust

Дата: 1 ноября 2026. Рабочая ветка: `rust-rewrite`, отдельный worktree `D:/repos/Rich-Studio-rust-rewrite`, исходный коммит `bd47b327669f736fd0a42ebe42135a3570b66f2b`.

**Дополнение после длительного запуска локального стенда:** первоначальный образ ниже
содержал SQLite 3.51.1 с воспроизведённой Unix/WAL-взаимоблокировкой. Его первоначальная
приёмка не является подтверждением исправления. Зависимость обновлена; текущий образ,
повторные проверки и регрессия описаны в [SQLite WAL fix](RUST_SQLITE_WAL_FIX.md).

Основание: [план из 18 срезов](superpowers/plans/2026-10-31-rust-backend-rewrite.md). Рабочий checkout `D:/repos/Rich-Studio` в эту работу не включён. Полученные незакоммиченные изменения другого агента сохранены перед продолжением в отдельной временной копии с SHA-256 manifest.

## Что реализовано

Сервер и операторский CLI работают одним Rust-бинарником. Axum обслуживает HTTP и прежнюю статику, официальный rmcp — Streamable HTTP и самодостаточный MCP-preview, Grammers — MTProto-импорт и вход для импорта. В рабочем Docker-образе отсутствуют Python, pip и исходники Python-сервера. Python остаётся эталоном поведения и локальным mock Telegram для разработки.

Перенесены черновики, каналы и темы; публикация rich/poll/live-photo/paid-media; предпросмотр в личку; рассылка и действия после публикации; очередь, изменение времени и отмена; редактирование, кнопки, версии и история; Telegram-вход, сессии и роли, персональные токены; polling/webhook, callbacks, комментарии, реакции, результаты опросов; пригласительные ссылки и предложенные посты; диалог бота и уведомления; импорт по ссылке, виджету и пересылке; шаблоны; журнал, метрики, health/readiness, housekeeping, ZIP-экспорт, ежедневные копии, Litestream-восстановление; публичный сайт, RU/EN, PWA и документы для агентов.

Сохранены 71 HTTP-операция на 55 документированных путях и 39 MCP-инструментов, их схемы, описания и annotations. `make docs` получает контракты из Rust CLI; Python contract-тесты продолжают сверять прежний интерфейс. Swagger UI и ReDoc работают из локальных закреплённых файлов, с авторизацией и CSP без inline-скриптов. Исходные файлы редактора и 17 старых SQL-миграций не переписаны; два браузерных сценария получили только выбор Rust/Python процесса в тестовой фикстуре.

## Исправления, существенные для перехода

- Запись `sending` предшествует сетевой отправке; подтверждённый результат сохраняется до закрепа, комментария и следующего опроса. Потерянный ответ, отмена и ошибка записи после отправки сохраняют запрет повтора.
- Отпечатки ключей совместимы с Python-сериализацией, включая Unicode, пробелы, порядок ключей, значения по умолчанию модели и запись float. Незавершённые/неопределённые ключи не удаляются по возрасту.
- Взятие задания учитывает актуальное время в атомарном UPDATE: старый снимок очереди не отправляет уже перенесённый пост. Восстановление исключает отправки живого процесса.
- Остановка прекращает новую работу, удерживает lease во время завершения HTTP-запросов, затем присоединяет фоновые задачи. Уведомления доставляются и при выключенном getUpdates.
- HTTP и MCP используют явный контекст учётки и актуальные права Telegram. Неизвестные поля внутри Telegram payload сохраняются; собственные модели студии повторяют проверку и преобразования прежнего Pydantic-контракта.
- Лимиты входа за Caddy учитывают реальный адрес посетителя только при доверенном proxy из `FORWARDED_ALLOW_IPS`; произвольный заголовок от недоверенного клиента не меняет счётчик.
- Совместимость импорта включает Telethon StringSession IPv4/IPv6, привязку сессии к Telegram ID, SOCKS/HTTP-прокси, UTF-16 offsets, полные rich-сообщения, ограниченные загрузки и уборку файлов после отмены/ошибки.
- Обновление спецификации сохраняет рабочую копию при ошибке записи; повторная проверка без изменений не возвращает старый diff как новый.

Мотивы архитектуры и намеренные изменения политики описаны в [ADR 0024](decisions/0024-rust-runtime.md).

## Критические сценарии

| Сценарий | Проверки Rust |
|---|---|
| Повтор ключа и потерянный ответ после принятия Telegram | `publication_safety`, `idempotency_compat` |
| Настоящий SQLITE_FULL после принятия Telegram | `publication_safety::sqlite_full_after_telegram_accept_keeps_the_key_and_restart_marker` |
| Частичная рассылка без дублей в успешных каналах | `publication_safety` |
| Одновременные правки одной версии | `editing` |
| Разделение учёток, медиа, токенов и прав | `account_isolation`, `auth_security`, `media_security`, `community`, `events` |
| Перезапуск после sending, отмена, lease и reschedule race | `scheduler_recovery`, `events` |
| Старые БД, миграции, токены и черновики | `migrations`, `runtime`, `idempotency_compat`, `templates` |
| Неизвестные поля Telegram без потерь | `telegram_payload`, `publication_safety`, `drafts_contract`, `http_mcp_contract` |
| Emoji и UTF-16 offsets при импорте | `import_contract` |
| Повтор webhook и ошибка обработчика | `events` |
| Искусственные секреты не попадают в журнал | `operations`, `telegram_payload` |

## Проверки

Результаты на одном замороженном наборе исходников:

| Проверка | Результат | Локальный журнал |
|---|---|---|
| `make docs` | PASS: 71 HTTP-операция, 39 MCP tools; контракт не изменён | `work/rust-make-docs-final.log` |
| Rust fmt, Clippy `--all-targets -D warnings`, nextest | PASS: 166/166, 0 skipped | `work/rust-make-check-final.log` |
| Doctests, cargo-deny, cargo-machete | PASS; doctests: 0 примеров. Deny: advisories/bans/licenses/sources OK, предупреждения о повторных версиях транзитивных crates | тот же журнал |
| Полный `make check` | PASS, exit 0; native Windows release собран с `--locked` | тот же журнал |
| Python-эталон | PASS: Ruff, format, mypy (105 файлов), 2 контракта слоёв; pytest: 653 passed + 1 ожидаемый Windows skip из 654 | тот же журнал, `work/rust-python-collection-final.log` |
| JavaScript | PASS: ESLint, tsc, render 43, formats 55, source editor 15, English 913 строк, preview screenshots 9 | `work/rust-make-check-final.log` |
| Сверка предпросмотра с записанными ответами Telegram | 28/29 совпадают; один ранее документированный `md-nested` known gap сохранён | тот же журнал |
| 47 исходных браузерных сценариев против Rust | PASS: 136/136 | `work/rust-e2e-final.log` |
| Swagger UI и ReDoc | PASS: 71 операция в каждом, 1400/390 px, авторизация, нет внешних запросов и JS/CSP ошибок | тот же журнал |
| Финальный Docker build и установленный образ | PASS: Linux amd64, uid 10001, нет Python/pip, read-only root, native CLI/readiness, состояние после restart, SIGTERM exit 0 | `work/rust-docker-build-final.log`, `work/rust-docker-smoke-final.log` |
| Litestream restore | PASS: token и draft восстановлены из файловой реплики после удаления только временного data volume | `work/rust-restore-final.log` |

Идентификатор финального образа из `docker image inspect`: `sha256:35f58f04c5a98a554f22b28738fff8620bca67549b9bb803f5e4d47e7c36508f`; метаданные сохранены в `work/rust-docker-image-final.json`. `work/rust-final-source-manifest.json` фиксирует хэши 346 файлов исходников, ресурсов, фикстур и сборки. Финальная сверка: 0 пропавших/добавленных путей и 0 несовпадений хэшей; SHA-256 manifest — `999b635e5976e8f7de850fb18e15948bf3b27c2bdfa33eca506921473f74d618`.

Native-бинарник `target/release/rich-studio.exe`: 30 205 952 байта, SHA-256 `6d5589557d331de532a6097eb49d61b7c96a8d07e3807a813019eca829defe84`, `--version` → `rich-studio 0.1.0`. Машиночитаемый итог: `work/rust-acceptance-final.json`.

Единственный pytest skip — `test_log_is_readable_only_by_its_owner`: проверка POSIX 0600 неприменима к Windows ACL. Исходный `md-nested` known gap относится к порядку вложенных тегов форматирования в предпросмотре; перенос backend его не менял. Эти исключения не добавлялись ради успешного прогона.

Команды воспроизведения:

```sh
make docs
make check
make e2e-rust
docker build -t rich-studio-rust-candidate .
IMAGE=rich-studio-rust-candidate node scripts/check-rust-container.mjs
IMAGE=rich-studio-rust-candidate scripts/restore_check.sh
```

`make check` включает Rust fmt/Clippy/nextest/doctests/deny/machete/release с `--locked`, Python lint/format/mypy/layers/pytest и прежние JS/type/render/parity/screenshot-проверки. `make e2e-rust` создаёт временные данные и запускает прежние 47 сценариев против Rust и mock Telegram; затем проверяет Swagger/ReDoc. На этой Windows-машине сборка ограничена `CARGO_BUILD_JOBS=1`, чтобы не исчерпать системный commit limit при параллельной линковке тестовых бинарников.

Локальные логи находятся в игнорируемой папке `work/`, скриншоты — в `test-results/ui/` и `test-results/preview/`. Проверка контейнера использует отдельные временные тома и локальную сеть; восстановление Litestream — отдельные проект, том и файловую реплику. После проверок эти процессы и временные контейнерные ресурсы убираются.

## Границы подтверждения

Проверки отправки используют только fake Telegram. Настоящие публикации, вход в пользовательский Telegram и живой MTProto-импорт не выполнялись. Для MTProto проверены код адаптера, реальные TL-конструкторы на синтетических данных, совместимость сессий и локальные прокси; это не подтверждение входа и скачивания на действующей учётке.

Production-сервис не переключался. GitHub CI и сборка arm64 требуют отдельного запуска после публикации ветки; локальная Linux amd64-проверка образа их не заменяет. Коммит, push, PR и merge этой работой не выполняются. Перед реальным переключением нужно остановить старый обработчик отправок, сохранить резервную копию и запускать только один новый лидер на выбранных данных.

Команды запуска и эксплуатации: [Rust runtime](RUST_RUNTIME.md).
