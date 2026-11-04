# Rust Backend Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Перенести весь production-backend Rich Studio на Rust с сохранением внешних контрактов, существующих данных и поведения при сбоях, без Python в production-runtime и Docker-образе.

**Architecture:** Один Cargo-пакет и один серверный бинарник; модули сохраняют слои `api/mcp_tools → services → telegram/store/spec`. Axum и rmcp являются только адаптерами, SQLite и SQL-миграции остаются прежними, Telegram payload хранится и передаётся как `serde_json::Value`. Существующие `app/static`, bundled-спецификация и браузерные тесты используются без переписывания.

**Tech Stack (фактически):** Rust 1.99.1, edition 2024, Tokio, Axum, Serde/serde_json, Reqwest, Rusqlite через конечные blocking workers, rmcp 3.8.0, Grammers 0.11.0, Utoipa 6, tracing, nextest. Точный набор и версии — `Cargo.toml` и `Cargo.lock`.

**Spec:** исходные контракты: `docs/openapi.json`, `docs/reference/mcp-tools.md`, `docs/ARCHITECTURE.md`.

## Выполнение на 1 ноября 2026

Все 18 срезов реализованы и прошли полную локальную приёмку: `make docs` и полный `make check` завершились с exit 0. Rust: 166/166 без пропусков; Python: 653 passed + 1 ожидаемый Windows skip; прежний JavaScript-набор зелёный. Браузер: все 47 сценариев, 136/136 проверок, плюс Swagger/ReDoc. Native Windows release, финальный Linux-образ и Litestream restore прошли проверку. Production не переключался; живой MTProto и GitHub CI не запускались.

Таблица ниже отражает фактическое выполнение; дальнейшие разделы сохраняют исходный план разработки, а не историю якобы выполненного red/green TDD. Некоторые первоначально предложенные тестовые файлы объединены в вертикальные срезы; актуальные имена приведены здесь. [Отчёт приёмки](../../RUST_MIGRATION_ACCEPTANCE.md) содержит команды, журналы и границы подтверждения.

| № | Реализованный срез | Фактические проверки |
|---|---|---|
| 1 | HTTP-каркас и quality gates | `contract_surface`, `architecture`, `make check-rust` |
| 2 | Settings, SQLite, AccountContext | `migrations`, `account_isolation`, `runtime` |
| 3 | Спецификация и черновики | `drafts_contract`, unit-тесты `spec::source`, `http_mcp_contract` |
| 4 | Медиа, статика, MCP-preview | `media_security`, `mcp_preview`, render/parity/shots |
| 5 | Bot API, каналы, raw | `telegram_payload`, `channels` |
| 6 | Публикация и идемпотентность | `publication_safety`, `idempotency_compat`, `post_service`; включая настоящий SQLITE_FULL после Telegram accept |
| 7 | Очередь, lease, остановка | `scheduler_recovery`, `events`; включая reschedule race и HTTP drain |
| 8 | Правки, версии, история | `editing`, браузерные сценарии |
| 9 | Telegram-вход, сессии и токены | `auth_security` (включая trusted proxy), `account_isolation`, браузерные 38/43 |
| 10 | Updates/webhook и события | `events`, `community` |
| 11 | Импорт, Telethon/Grammers | `import_contract` (24 сценария), CLI unit-тесты; живой MTProto отдельно не проверялся |
| 12 | Шаблоны | `templates`, браузерные сценарии |
| 13 | Invite links, suggested posts | `community`, `events`, браузерные сценарии |
| 14 | Диалог бота и уведомления | `events`, настоящий Loops с выключенным getUpdates |
| 15 | Логи, health, backup, housekeeping, CLI | `operations`, `runtime`, установленный контейнер, Litestream restore |
| 16 | Публичный сайт, PWA, agent docs | `public_site`, прежние браузерные сценарии RU/EN и mobile |
| 17 | Контракты и критические сбои | `http_mcp_contract`, весь nextest, прежние Python/JS проверки, 47 E2E-сценариев |
| 18 | Production без Python | финальный Docker image/smoke/restore, native CLI, настройки CI; рабочий сервис не переключался |

## Исходный план разработки

## Global Constraints

- Изменения только в `D:/repos/Rich-Studio-rust-rewrite`; основная копия, действующий сервис и его данные не затрагиваются, push не выполняется. Файлы сборки и инструкции `deploy/` входят в перенос runtime.
- Настоящий Telegram не вызывается; все проверки используют mock или записанные протокольные данные.
- Неизвестные поля Telegram не отбрасываются и не отклоняются; транспорт меняет только `upload:<имя>` на `attach://` в файловых полях.
- `sending` записывается до сетевого вызова; `MayHaveGone`, отмена и неизвестный результат никогда не повторяются слепо.
- Старые миграции не редактируются; `Cargo.lock` обновляется только Cargo и проверки release выполняются с `--locked`.
- Контекст учётки передаётся явно; отсутствие контекста никогда не означает владельца.
- Каждый функциональный срез включает service + storage + HTTP + относящиеся MCP tools + тесты + документацию.
- Не ослаблять существующие Python/JS/contract-проверки на время переноса; финальный `make check` добавляет Rust-гейты, а не заменяет ими браузерные проверки.
- Не добавлять commits без отдельной просьбы пользователя; каждый срез заканчивается проверенным diff.

## Review Focus

- Старый `Idempotency-Key` после перехода: Python `json.dumps(..., ensure_ascii=False, sort_keys=True, default=str)` должен распознаваться повторно — Task 6, snapshot fixtures из Python.
- IPv6 Telethon StringSession и SOCKS5/HTTP proxy: сессия должна сохранить адрес, ключ и UID-изоляцию — Task 11, fixtures для 263/279 байт и unit-тесты proxy.
- SSRF через каждый redirect при скачивании медиа: приватный адрес запрещён даже после публичного ответа — Task 4, управляемый HTTP-server test.
- Отмена Rust future после отправки, но до записи результата: запись должна остаться `sending`/uncertain и не вернуться в очередь — Tasks 6–7, barrier-controlled mock test.
- Повреждённый JSON в старой строке SQLite: одна плохая запись не должна открыть доступ другой учётке или молча подменить payload пустым объектом — Tasks 2 и 17, explicit decode-error test.

## File Structure

- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml` — toolchain, зависимости и качество.
- `src/lib.rs`, `src/main.rs` — сборка приложения и небольшая точка входа.
- `src/settings.rs`, `src/errors.rs`, `src/account_context.rs`, `src/security.rs`, `src/logs.rs`, `src/metrics.rs` — общие инфраструктурные типы.
- `src/api/`, `src/mcp_tools/` — тонкие HTTP/MCP-адаптеры; `src/mcp_tools/tools/` повторяет доменное разбиение текущих tools.
- `src/services/` — бизнес-правила; `src/store/` — SQL и миграции; `src/telegram/` — Bot API и MTProto; `src/spec/` — спецификация, валидация и преобразования.
- `tests/rust/` — integration/contract tests; `tests/fixtures/` — записанные HTTP/MCP/Telegram/SQLite fixtures без секретов.
- `tests/mock_telegram.py` остаётся dev-эталоном до переноса mock; production-код и Docker от него не зависят.

---

### Task 1: Контрактный каркас и Rust quality gates

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml`, `src/lib.rs`, `src/main.rs`, `src/api/mod.rs`, `src/mcp_tools/mod.rs`, `tests/rust/contract_surface.rs`
- Modify: `Makefile`, `AGENTS.md`
- Test: `tests/rust/contract_surface.rs`

**Interfaces:**
- Produces: `pub fn build_router(state: AppState) -> Router`, `pub struct AppState`; `make check-rust` с полным набором команд качества.

- **Step 1: Write failing contract tests.** Проверить реальный liveness `/healthz` → `{"ok":true}`, статический `/` и GET `/mcp` → 405 с `Allow: POST`. Полные 55 путей/71 операция, 39 tools, health report и preview metadata проверяются в Tasks 4, 15 и 17 после появления реальных сервисов, без фиктивных ответов.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked contract_surface`; Expected: отсутствует crate/реализация.
- **Step 3: Implement minimal Axum shell and quality configuration.** Добавить lints, `unsafe_code=forbid`, static-file adapter, health route и заготовку MCP без регистрации фиктивных рабочих tools. `make check-rust` включает fmt, Clippy без warnings, nextest, doctests, deny, machete и release `--locked`; `make check` включает этот target и прежние проверки.
- **Step 4: Run foundation checks.** Run: `cargo fmt --all -- --check && CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked && cargo nextest run --workspace --locked --no-fail-fast && cargo test --workspace --doc --locked`; Expected: PASS, без изменения существующих OpenAPI/reference docs.

### Task 2: Settings, SQLite-миграции и явный AccountContext

**Files:**
- Create: `src/settings.rs`, `src/errors.rs`, `src/account_context.rs`, `src/store/{mod.rs,db.rs,accounts.rs}`, `tests/rust/{migrations.rs,account_isolation.rs}`
- Modify: `src/lib.rs`
- Test: старые базы из fixtures и все 17 SQL-файлов `app/store/migrations/`.

**Interfaces:**
- Produces: `Settings::from_env() -> Result<Settings, SettingsError>`, `AccountContext { account_id, role, root, media_dir, transport_policy }`, `Store::connect(&AccountContext) -> Result<Connection, StoreError>`, `Store::connect_root()`, `migrate(&mut Connection) -> Result<(), MigrationError>`.

- **Step 1: Write failing storage/settings tests.** Проверить `PRAGMA user_version`, порядок и транзакционность миграций, WAL/foreign_keys/busy_timeout, `ACCOUNT_DATABASE_MB`, root без account quota, `*_FILE`, агрегацию всех SettingsError, явный account path и запрет fallback к owner. Добавить malformed-old-JSON error test.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked migrations account_isolation`; Expected: FAIL по отсутствующим типам.
- **Step 3: Implement settings and storage.** Выполнять синхронный SQLite вне async workers; повторно использовать SQL без изменений; типизировать `StorageFull` отдельно от отказов до отправки. Не держать transaction через network await.
- **Step 4: Verify old data and slice.** Run: `cargo nextest run --locked migrations account_isolation && pytest -q tests/test_migrations.py tests/test_public_accounts.py`; Expected: PASS на копиях старых БД.

### Task 3: Спецификация, валидация и черновики — первый полный вертикальный срез

**Files:**
- Create: `src/spec/{mod.rs,source.rs,validator.rs,diff.rs}`, `src/store/drafts.rs`, `src/services/drafts.rs`, `src/api/{drafts.rs,specs.rs}`, `src/mcp_tools/tools/{drafts.rs,bot_api.rs}`, `tests/rust/drafts_contract.rs`
- Modify: `src/api/mod.rs`, `src/mcp_tools/mod.rs`

**Interfaces:**
- Consumes: `Store`, `AccountContext` из Task 2.
- Produces: `DraftService::{list,get,save}`, `Validator::check(&Value) -> Validation`, расширяемый `post: Value` без flatten-отбрасывания.

- **Step 1: Write failing draft/spec contract tests.** Перенести сценарии `test_api.py`, `test_spec_coverage.py`, `test_update_spec.py` и MCP draft tests: CRUD, version 409, `_studio`, chat_ids, `describe_type`, `validate_post`, `save_draft`, spec pin/cache/offline fallback и unknown Telegram fields round-trip.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked drafts_contract`; Expected: FAIL.
- **Step 3: Implement spec and draft slice.** Подключить HTTP operations и MCP tools с прежними JSON schemas, Russian summaries/docstrings и editor/preview links. Не кодировать список блоков вне `api.json`.
- **Step 4: Verify slice and OpenAPI.** Run: `cargo nextest run --locked drafts_contract && npm run --silent test:formats && npm run --silent test:source`; Expected: PASS; сравнение OpenAPI не показывает потерянных операций.

### Task 4: Медиа, статика и самодостаточный MCP-preview

**Files:**
- Create: `src/services/media.rs`, `src/api/{media.rs,preview.rs,pwa.rs}`, `src/mcp_tools/{preview_app.rs,tools/files.rs}`, `tests/rust/{media_security.rs,mcp_preview.rs}`
- Modify: `src/api/mod.rs`, `src/mcp_tools/mod.rs`

**Interfaces:**
- Produces: `PreviewPage::html() -> &'static str`, `MediaService::{upload,download,list,mark_used}`, прежний URI `ui://rich-studio/preview`.

- **Step 1: Write failing media/preview tests.** Воспроизвести `test_preview_app` точно: `_meta.ui.resourceUri`, readOnly, `resources/read`, mime, inline RichRender/ui-initialize и escaped `</script`. Добавить upload limits, path traversal, content names, public-address check на каждом redirect, system proxy и account media isolation.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked media_security mcp_preview`; Expected: FAIL.
- **Step 3: Implement media and rmcp resource/tool adapter.** Использовать существующие CSS/JS в прежнем порядке; никаких внешних загрузок из sandbox preview. Подключить upload/list HTTP и `upload_media`, `list_media`, `preview_post` без отправки в Telegram.
- **Step 4: Verify preview and browser units.** Run: `cargo nextest run --locked media_security mcp_preview && npm run --silent test:render && npm run --silent test:parity && npm run --silent test:shots`; Expected: PASS.

### Task 5: Bot API transport, каналы и raw API

**Files:**
- Create: `src/telegram/{mod.rs,client.rs,uploads.rs,chat.rs}`, `src/services/channels.rs`, `src/store/channels.rs`, `src/api/{channels.rs,raw.rs}`, `src/mcp_tools/tools/chats.rs`, `tests/rust/{telegram_payload.rs,channels.rs}`
- Modify: `src/errors.rs`, `src/api/mod.rs`, `src/mcp_tools/mod.rs`

**Interfaces:**
- Produces: `TelegramTransport::call(&AccountContext, method, payload, files) -> Result<Value, TelegramError>`; variants `Refused`, `RateLimited`, `Unreachable`, `NoAnswer`; `check_channel` с сохранением прав/topics.

- **Step 1: Write failing transport tests.** Snapshot multipart/JSON: неизвестные поля и числа/Unicode сохраняются, только пять файловых полей получают `attach://`; токен не попадает в errors/logs. Проверить connect-fail vs response-lost vs 429 vs 400 vs 5xx, каналы/форумы/права и transport policy каждой учётки.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked telegram_payload channels`; Expected: FAIL.
- **Step 3: Implement Reqwest transport and channel slice.** Без автоматических HTTP retries; применять policy до вызова; ограничить uploads/downloads; сохранить HTTP/MCP mapping ошибок 400→422, прочие Telegram→502.
- **Step 4: Verify against mock.** Run: `cargo nextest run --locked telegram_payload channels && pytest -q tests/test_telegram_client.py tests/test_channels.py`; Expected: одинаковые payload в mock и PASS.

### Task 6: Публикация и идемпотентность

**Files:**
- Create: `src/services/{publisher.rs,posts.rs,idempotency.rs,kinds.rs}`, `src/store/{posts.rs,idempotency.rs}`, `src/api/posts.rs`, `src/mcp_tools/tools/{publishing.rs,media_posts.rs}`, `tests/rust/{publication_safety.rs,idempotency_compat.rs}`
- Modify: `src/errors.rs`, `src/api/mod.rs`, `src/mcp_tools/mod.rs`

**Interfaces:**
- Produces: `SendError::{Refused,Retryable,MayHaveGone}`, `Idempotency::run(key, scope, body, actor, action)`, `fingerprint(scope, body) -> String`, preview-only routing.

- **Step 1: Write failing failure-matrix tests.** Два одинаковых ключа; response lost после accept; DB full после accept; partial multi-channel; pin/comment/poll_after после sent; no channel succeeded; dry_run; preview never defaults to channel; unknown fields survive. Добавить Python-generated fingerprint fixtures, включая Unicode, вложенные объекты и `default=str` cases.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked publication_safety idempotency_compat`; Expected: FAIL.
- **Step 3: Implement durable publication flow.** Записать `sending` до вызова, не держать SQLite transaction во время сети, завершить sent до post-steps, не освобождать key при uncertain/StorageFull/bug/cancel. Перенести rich/poll/live_photo/paid_media registry и все publishing MCP annotations.
- **Step 4: Verify no duplicate under real mock calls.** Run: `cargo nextest run --locked publication_safety idempotency_compat && pytest -q tests/test_safe_send.py tests/test_reliability.py tests/test_media_posts.py`; Expected: PASS и ровно один accept в сценариях uncertain.

### Task 7: Очередь, lease и остановка

**Files:**
- Create: `src/services/{scheduler.rs,loops.rs}`, `src/store/meta.rs`, `tests/rust/{scheduler_recovery.rs,loop_lease.rs}`
- Modify: `src/main.rs`, `src/services/posts.rs`

**Interfaces:**
- Produces: управляемые `Clock`, `Scheduler::run_due`, `LoopLease`, cooperative shutdown с `SHUTDOWN_GRACE`.

- **Step 1: Write failing timing/cancellation tests.** Schedule/reschedule/cancel/missed; crash after `sending`; only one lease leader; renewal failure longer than TTL-RENEW; no new work during shutdown; barrier-controlled cancellation after Telegram accept; retry только Unreachable/429.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked scheduler_recovery loop_lease`; Expected: FAIL.
- **Step 3: Implement persistent scheduler and owned tasks.** Каждая задача имеет owner/result/shutdown; started spawn_blocking учитывается при завершении; время и гонки управляются clocks/barriers, не sleeps.
- **Step 4: Verify restart semantics.** Run: `cargo nextest run --locked scheduler_recovery loop_lease && pytest -q tests/test_loops.py`; Expected: PASS без retries/flaky settings.

### Task 8: Редактирование, версии и история постов

**Files:**
- Create: `src/store/stats.rs`, `src/services/listing.rs`, `src/mcp_tools/tools/published.rs`, `tests/rust/post_editing.rs`
- Modify: `src/store/posts.rs`, `src/services/posts.rs`, `src/api/posts.rs`

**Interfaces:**
- Produces: `claim_version`/`release_version` с SQLite `UPDATE … RETURNING`, `post_versions`, post listing/get/history и edit tools.

- **Step 1: Write failing edit tests.** Concurrent same-version edits; version release on definite refusal; retained version on network uncertainty; old payload snapshot; scheduled/missed atomic guard; “message is not modified”; callbacks/comment preservation; delete failed only as прежде.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked post_editing`; Expected: FAIL.
- **Step 3: Implement edit/list/history slice.** Подключить HTTP и `list_posts`, `get_post`, `post_history`, `edit_post`, publish-by-draft version guards и `actor` из `X-Studio-Client`.
- **Step 4: Verify regression suite.** Run: `cargo nextest run --locked post_editing && pytest -q tests/test_post_editing.py tests/test_regressions.py`; Expected: PASS.

### Task 9: Telegram-вход, сессии, права и персональные токены

**Files:**
- Create: `src/security.rs`, `src/store/{tokens.rs,account_tokens.rs}`, `src/services/account_tokens.rs`, `src/api/{session.rs,telegram_auth.rs,account_tokens.rs,deps.rs}`, `tests/rust/{access.rs,account_tokens.rs}`
- Modify: `src/account_context.rs`, `src/api/mod.rs`, `src/mcp_tools/mod.rs`

**Interfaces:**
- Produces: `Principal`, `Role::{Read,Draft,Publish,Admin}`, cookie/Telegram/bearer authentication и per-request `AccountContext`.

- **Step 1: Write failing security tests.** Host/CSRF/Content-Type/CSP; owner login; Telegram auth expiry/signature; named tokens read/draft/publish/admin boundaries; `MCP_PUBLISH=off`; token cannot create token/browser session; parallel account isolation including background actor.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked access account_tokens`; Expected: FAIL.
- **Step 3: Implement auth middleware and explicit scopes.** Проверять method-level baseline и publish/admin на маршрутах/tools; constant-time secrets; old browser sessions/tokens readable; no cross-account filesystem or Telegram policy access.
- **Step 4: Verify access matrix.** Run: `cargo nextest run --locked access account_tokens && pytest -q tests/test_access.py tests/test_owner_login.py tests/test_account_tokens.py`; Expected: PASS.

### Task 10: Updates, webhook, кнопки, комментарии и реакции

**Files:**
- Create: `src/services/updates.rs`, `src/api/webhook.rs`, `tests/rust/{updates.rs,webhook.rs}`
- Modify: `src/store/{posts.rs,channels.rs,meta.rs}`, `src/services/loops.rs`

**Interfaces:**
- Produces: `ALLOWED_UPDATES`, `handle_update(AccountContext, Value)`, atomic root update_id claim и Telegram secret verification.

- **Step 1: Write failing event tests.** Callback answers, first comment, reactions, poll results, forum topics, join events, suggested outcomes; duplicate webhook; handler failure after claim; wrong secret; revoked channel rights; one bad account does not stop others; uncertain comment не повторяется.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked updates webhook`; Expected: FAIL.
- **Step 3: Implement getUpdates/webhook slice.** Сохранить STATUS и registration/deleteWebhook rules; не использовать общий bypass transport policy; сохранить статистику в той же БД.
- **Step 4: Verify events against recorded updates.** Run: `cargo nextest run --locked updates webhook && pytest -q tests/test_webhook.py`; Expected: PASS.

### Task 11: Импорт и совместимость Telethon/Grammers

**Files:**
- Create: `src/services/{imports.rs,import_links.rs,import_web.rs,import_forward.rs}`, `src/telegram/import_client.rs`, `src/spec/{import_content.rs,convert.rs,parse.rs}`, `src/api/imports.rs`, `src/mcp_tools/tools/imports.rs`, `tests/rust/{import_content.rs,import_session.rs,import_isolation.rs}`
- Modify: `src/main.rs` для import CLI entry point.

**Interfaces:**
- Produces: `TelethonSession::decode() -> Result<DcOption, ImportSessionError>`, `ImportTransport::get_rich_message`, account-bound session path/UID check и CLI login без web prompts.

- **Step 1: Write failing import fixtures.** Перенести link cases, web fallback, forward one-time arming, albums/media, tables/nested formatting/unsupported warnings, emoji UTF-16 offsets, `part=true` → `GetRichMessage`, private-link access, account UID mismatch. Добавить Telethon IPv4/IPv6 fixtures и proxy parsing; не использовать `grammers-stringsession` (его формат несовместим).
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked import_content import_session import_isolation`; Expected: FAIL.
- **Step 3: Implement conversion and Grammers adapter.** Использовать `grammers-tl-types 0.11.0` (`messages.getRichMessage`, `RichMessage.part`), собственный Telethon decoder, no request retries, account-scoped sessions вне media/export. Bot mode не заимствует пользовательскую сессию.
- **Step 4: Verify offline protocol and existing fixtures.** Run: `cargo nextest run --locked import_ && pytest -q tests/test_imports.py`; Expected: PASS. Живой импорт не отмечать проверенным без отдельной разрешённой приёмки.

### Task 12: Шаблоны

**Files:**
- Create: `src/{store,services}/templates.rs`, `src/api/templates.rs`, `src/mcp_tools/tools/templates.rs`, `tests/rust/templates.rs`

**Interfaces:**
- Produces: typed template fields/version, literal substitution и создание отдельного draft через Task 3.

- **Step 1: Write failing template tests.** CRUD/version conflicts, required/default/url, HTML/Markdown escaping, multiline, unknown placeholders, `_studio` exclusion, media usage cleanup и account isolation.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked templates`; Expected: FAIL.
- **Step 3: Implement storage + service + HTTP + four template MCP tools.** Сохранить schemas/docstrings и отсутствие публикации.
- **Step 4: Verify slice.** Run: `cargo nextest run --locked templates && pytest -q tests/test_templates.py`; Expected: PASS.

### Task 13: Invite links и suggested posts

**Files:**
- Create: `src/{store,services}/{invite_links.rs,suggested_posts.rs}`, `src/api/{invite_links.rs,suggested_posts.rs}`, `src/mcp_tools/tools/{invite_links.rs,suggested_posts.rs}`, `tests/rust/{invite_links.rs,suggested_posts.rs}`
- Modify: `src/services/updates.rs`

**Interfaces:**
- Produces: invite create/edit/revoke/list; suggested `pending → approving/declining` durable claim и outcomes.

- **Step 1: Write failing interaction tests.** Invite defaults/edit clearing/subscription rules/counters; repeated approve/decline click; definite refusal releases state; uncertain waits for service message; publish annotations и explicit confirmation contract.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked invite_links suggested_posts`; Expected: FAIL.
- **Step 3: Implement both vertical slices.** Сохранить, что Telegram публикует accepted suggested post, а не Studio posts flow; обновления из Task 10 заполняют counters/outcomes.
- **Step 4: Verify interactions.** Run: `cargo nextest run --locked invite_links suggested_posts && pytest -q tests/test_invite_links.py tests/test_suggested_posts.py`; Expected: PASS.

### Task 14: Bot dialog и уведомления

**Files:**
- Create: `src/{store,services}/{bot_dialog.rs,bot_notifications.rs}`, `tests/rust/bot_dialog.rs`
- Modify: `src/services/{updates.rs,loops.rs}`

**Interfaces:**
- Produces: личные команды, root dialog settings, owner-queue reports и leased delivery loop.

- **Step 1: Write failing dialog/delivery tests.** Commands never publish; per-owner queue; account rights; dedup; definite failure vs uncertain delivery; no blind resend; isolated settings.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked bot_dialog`; Expected: FAIL.
- **Step 3: Implement dialog and notification slice.** Подключить update handler и background factory с прежними lease/shutdown semantics.
- **Step 4: Verify dialog behavior.** Run: `cargo nextest run --locked bot_dialog && pytest -q tests/test_bot_dialog.py`; Expected: PASS.

### Task 15: Журнал, health, backup, housekeeping и эксплуатационный CLI

**Files:**
- Create: `src/{logs.rs,metrics.rs}`, `src/services/{health.rs,backup.rs,housekeeping.rs}`, `src/store/backup.rs`, `src/api/{health.rs,backup.rs,service.rs}`, `src/mcp_tools/tools/studio.rs`, `src/bin/studio-cli.rs`, `tests/rust/{observability.rs,backup.rs,cli.rs}`
- Modify: `Makefile`, `scripts/restore_check.sh`

**Interfaces:**
- Produces: structured redacted logs, metrics/health, zip backup/restore, token CLI и server lifecycle commands без Python.

- **Step 1: Write failing ops tests.** Secret URL/bearer/session/post redaction; health ok/warn/fail/action; metrics; backup with old DB/media excluding import sessions; retention; token add/list/revoke; disk/full errors; housekeeping only within configured retention.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked observability backup cli`; Expected: FAIL.
- **Step 3: Implement operations and MCP studio tools.** Сохранить JSON fields/request_id, `studio_status`, `studio_health`, `studio_logs`, daily SQLite online backup и restore script.
- **Step 4: Verify restore on copied legacy data.** Run: `cargo nextest run --locked observability backup cli && pytest -q tests/test_observability.py tests/test_backup.py && scripts/restore_check.sh`; Expected: PASS, Rust CLI читает старые токены и состояние.

### Task 16: Публичный сайт, PWA и agent docs

**Files:**
- Create: `src/{public_site.rs}`, `src/api/{landing.rs,agent_docs.rs}`, `tests/rust/{public_seo.rs,agent_docs.rs,pwa.rs}`
- Modify: `src/api/mod.rs`

**Interfaces:**
- Produces: fixed-inventory RU/EN pages, canonical/hreflang/robots/sitemap, service worker scope `/`, bundled llms/agents docs.

- **Step 1: Write failing public-content tests.** Unknown slug cannot become file path; no private content/secrets/account caching; URLs from settings; offline screen only; CSP and static assets match existing editor.
- **Step 2: Run tests to verify they fail.** Run: `cargo nextest run --locked public_seo agent_docs pwa`; Expected: FAIL.
- **Step 3: Implement fixed public routes and asset serving.** Не добавлять offline editing или write retries; сохранить SEARCH domain boundaries.
- **Step 4: Verify public contracts.** Run: `cargo nextest run --locked public_ && pytest -q tests/test_public_seo.py tests/test_agent_docs.py tests/test_pwa.py`; Expected: PASS.

### Task 17: Полная сверка HTTP/MCP и критических сценариев

**Files:**
- Create: `tests/rust/{openapi_parity.rs,mcp_parity.rs,critical_migration.rs}`, `tests/fixtures/legacy_state/`
- Modify: `docs/openapi.json` и `docs/reference/` только для исправления расхождений генератора, не для ослабления контракта.

**Interfaces:**
- Consumes: все предыдущие slices.
- Produces: black-box contract suite против Rust server и mock Telegram.

- **Step 1: Write full parity tests.** Сравнить все 71 операцию/55 путей, 39 tools, schemas/annotations/errors/roles; прогнать 12 критических сценариев и malformed legacy JSON. Проверить `MCP_PUBLISH=off` как отсутствие publish tools, а не только отказ вызова.
- **Step 2: Run parity to expose gaps.** Run: `cargo nextest run --locked parity critical_migration`; Expected: FAIL по каждому неперенесённому контракту; составить явный список, не удалять assertions.
- **Step 3: Close every compatibility gap in owning slice.** Никаких заглушек, skipped routes/tools или изменения expected JSON ради зелёного результата.
- **Step 4: Run complete regression and browser scenarios.** Run: `make check` и `make e2e` против Rust `make dev` + mock; Expected: все Python-reference, Rust, JS и 47 браузерных сценариев PASS.

### Task 18: Удаление Python из production и финальная приёмка

**Files:**
- Modify: `Dockerfile`, `docker-compose.yml`, `deploy/`, `Makefile`, `README.md`, `docs/ARCHITECTURE.md`, `.github/workflows/`
- Test: `tests/rust/docker_runtime.rs` или эквивалентный существующий container smoke script без добавления нового языка в runtime.

**Interfaces:**
- Produces: единственный Rust server/CLI, образ без Python interpreter/site-packages, прежние volume/env/healthcheck/backup commands.

- **Step 1: Write production-runtime audit.** Проверить entrypoints, healthcheck, token/import-login/restore commands и отсутствие вызовов Python; зафиксировать, что `.py` вне production image остаются только dev-эталоном.
- **Step 2: Build and run the image to verify the old runtime is gone.** Run: `docker build -t rich-studio-rust:acceptance .`, затем запустить контейнер на copied legacy fixture и выполнить audit `python`/`python3` внутри image; Expected: до переноса audit FAIL, после — image builds, контейнер стартует и interpreters отсутствуют.
- **Step 3: Implement final image/CI/docs transition.** Multi-stage Rust build с `--locked`, non-root uid 10001/read-only совместимость, healthcheck через Rust binary или available non-Python tool, Litestream volumes и operational docs. Не менять старые миграции или data format.
- **Step 4: Run final acceptance.** Run последовательно: `make check`; `make e2e`; `cargo fmt --all -- --check`; `CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked`; `cargo nextest run --workspace --locked --no-fail-fast`; `cargo test --workspace --doc --locked`; `cargo deny --workspace --locked check`; `cargo machete`; `cargo build --workspace --release --locked`; container healthcheck. Expected: всё PASS, git diff не содержит временных spike/cache/log/secrets файлов, ни одна production-функция не вызывает Python.
