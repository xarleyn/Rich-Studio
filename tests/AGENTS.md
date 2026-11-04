# Тесты

- **Rust** — основной набор: `cargo nextest run --workspace --locked` (или `cargo test --workspace --locked`). Контракты HTTP/MCP, SQLite, права, очередь и сбои лежат в `tests/rust/`.
- **JS** — `npm run test:render`, `test:formats`, `test:source`, `test:i18n`, `test:parity`, `test:shots`. Враждебные примеры санитайзера — в `tests/render_unit.mjs`.
- **Браузер** — `make e2e-rust`: временная база, свободные порты, Rust-сервер и подставной Telegram. Сценарии — `tests/e2e/NN-имя.mjs`; новая проверка — новый файл со следующим номером.
- `tests/mock_telegram.py` — только dev-стенд для браузерных сценариев; production и Rust-тесты от него не зависят. tgfake-проверка описана в `docs/TGFAKE.md`.
- Найденный баг сначала получает падающий Rust-тест (обычно в `tests/rust/regressions.rs`), потом исправление.
- `scripts/generate-rust-docs.mjs --check` (или `make check-docs`) сверяет `docs/openapi.json` и `docs/reference/` с Rust-бинарником; после намеренного изменения API запускайте `make docs`.
- `npm run test:lock` сверяет `requirements.txt` и `requirements.lock`.
