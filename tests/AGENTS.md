# Тесты

# Тесты

- **Rust** — основной набор: `cargo nextest run --workspace --locked` (или `cargo test --workspace --locked`). Контракты HTTP/MCP, SQLite, права, очередь и сбои лежат в `tests/rust/`.
- **JS** — `npm run test:render`, `test:formats`, `test:source`, `test:i18n`, `test:parity`, `test:shots`. Враждебные примеры санитайзера — в `tests/render_unit.mjs`.
- **Браузер** — `make e2e-local` (Rust-сервер + `tests/mock_telegram.cs`, временная база, свободные порты). Сценарии — `tests/e2e/NN-имя.mjs`; новая проверка — новый файл со следующим номером. Одна проверка: `make e2e-local ONLY=poll`.
- `tests/mock_telegram.cs` — только dev-стенд для браузерных сценариев; production и Rust-тесты от него не зависят. tgfake-проверка описана в `docs/TGFAKE.md`.
- `e2e/harness.mjs` не начинает, если студия по `BASE` ходит не в подставной Telegram: рабочая студия на 7424 опубликовала бы проверки в канал. Ждите событие, а не фиксированную паузу: `waitForFunction` для страницы, `until(() => …)` из `e2e/harness.mjs` для лога mock и API (сравнивайте с числом вызовов до действия: лог хранит всю сессию). Проверка, что ждёт события: `E2E_SLOW=500 make e2e-local ONLY=<проверка>` задерживает каждый запрос страницы, проверка с паузой на этом падает.
- `preview_shots.mjs`: каждый пример из `examples/` в светлой и тёмной теме, проверка вёрстки (нет ошибок, у блоков есть размер, ничего не вылезает из пузыря) и скриншоты в `test-results/preview/` (в CI это артефакт). Пиксели не сравниваются: шрифты на разных машинах разные. Новый блок в спецификации: добавь пример в `examples/`, и он попадёт сюда сам.
- Найденный баг сначала получает падающий Rust-тест (обычно в `tests/rust/regressions.rs`), потом исправление.
- `scripts/generate-rust-docs.mjs --check` (или `make check-docs`) сверяет `docs/openapi.json` и `docs/reference/` с Rust-бинарником; после намеренного изменения API запускайте `make docs`.
- `npm run test:lock` сверяет `requirements.txt` и `requirements.lock`.
