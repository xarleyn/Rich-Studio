# Одна команда проверки для людей и агентов: `make check`. Вывод короткий, ошибки в конце.
# Инструменты запускаются тем же Python, куда ставили requirements-dev.txt (.venv, если он есть).
PY ?= $(firstword $(wildcard .venv/bin/python .venv/Scripts/python.exe) python3)

LINT_IMPORTS = $(firstword $(wildcard .venv/bin/lint-imports .venv/Scripts/lint-imports.exe) lint-imports)
UV = $(firstword $(wildcard .venv/bin/uv .venv/Scripts/uv.exe) uv)

.PHONY: check check-fast check-rust check-rust-fast check-docs check-py check-js fmt test openapi docs e2e e2e-local e2e-rust e2e-tgfake mock dev dev-python spec-update token tokens token-revoke lock

check: check-rust check-docs check-py check-js

check-rust:
	cargo fmt --all -- --check
	CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo nextest run --workspace --locked --no-fail-fast
	cargo test --workspace --doc --locked
	cargo deny --workspace --locked check
	cargo machete
	cargo build --workspace --release --locked

check-rust-fast:
	cargo fmt --all -- --check
	CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo nextest run --workspace --locked --no-fail-fast
	cargo test --workspace --doc --locked

# docs/openapi.json и docs/reference/ остаются актуальными относительно Rust-бинарника.
check-docs:
	cargo build --locked --bin rich-studio
	node scripts/generate-rust-docs.mjs --check

# Быстрая проверка между шагами, без браузера. Перед PR всё равно нужен `make check`.
check-fast: check-rust-fast
	$(PY) -m ruff check .
	$(PY) -m ruff format --check .
	$(PY) -m mypy
	PYTHONPATH=app $(LINT_IMPORTS) --no-cache
	npx eslint .
	npx tsc -p .

check-py:
	$(PY) -m ruff check .
	$(PY) -m ruff format --check .
	$(PY) -m mypy
	PYTHONPATH=app $(LINT_IMPORTS) --no-cache

check-js:
	npx eslint .
	npx tsc -p .
	npm run --silent test:lock
	npm run --silent test:render
	npm run --silent test:formats
	npm run --silent test:source
	npm run --silent test:i18n
	npm run --silent test:parity
	npm run --silent test:shots

fmt:
	cargo fmt --all
	$(PY) -m ruff check --fix .
	$(PY) -m ruff format .
	npx eslint --fix .

test:
	cargo test --workspace --locked

# После намеренного изменения API: обновить docs/openapi.json и закоммитить его.
openapi: docs

# docs/openapi.json and docs/reference/ (HTTP routes and MCP tools) from the code
docs:
	cargo build --locked --bin rich-studio
	node scripts/generate-rust-docs.mjs

# Именные токены доступа (README → Безопасность). База — ./data, как у docker compose; другая: DATA=путь.
# В контейнере то же: docker compose exec rich-studio rich-studio token add NAME --level draft
DATA ?= ./data
LEVEL ?= draft
token:
	DATA_DIR="$(DATA)" cargo run --locked -- token add $(NAME) --level $(LEVEL)
tokens:
	DATA_DIR="$(DATA)" cargo run --locked -- token list
token-revoke:
	DATA_DIR="$(DATA)" cargo run --locked -- token revoke $(NAME)

# Студия на подставном Telegram: http://localhost:7424, без токена и сети.
mock:
	$(PY) -m uvicorn mock_telegram:app --app-dir tests --port 8081

dev:
	mkdir -p work/dev-data
	DATA_DIR=work/dev-data TG_BOT_TOKEN=1:x TG_API_BASE=http://127.0.0.1:8081 \
	  TG_DEFAULT_CHAT=@test TG_PREVIEW_CHAT=777 SPEC_URL=http://127.0.0.1:9/off \
	  cargo run --locked -- serve

# Python remains a development reference during the migration.
dev-python:
	mkdir -p work/dev-data-python
	DATA_DIR=work/dev-data-python TG_BOT_TOKEN=1:x TG_API_BASE=http://127.0.0.1:8081 \
	  TG_DEFAULT_CHAT=@test TG_PREVIEW_CHAT=777 SPEC_URL=http://127.0.0.1:9/off \
	  $(PY) -m uvicorn main:app --app-dir app --port 7424 --reload

# Браузерные проверки: сначала `make mock` и `make dev` в соседних терминалах.
e2e:
	npm run --silent test:e2e

# Браузерные проверки на своём стенде (Rust + C# mock): свои порты, пустой DATA_DIR.
# Одна проверка: make e2e-local ONLY=poll
e2e-local:
	cargo build --locked --bin rich-studio
	E2E_ONLY="$(ONLY)" node scripts/check-rust-browser.mjs

# Изолированный запуск тех же сценариев: временные данные, свободные порты, только подставной Telegram.
e2e-rust:
	cargo build --locked --bin rich-studio
	node scripts/check-rust-browser.mjs

# Ресурсоёмкая проверка с внешним tgfake: не входит в make check.
e2e-tgfake:
	@test -n "$(TGFAKE_BIN)" || { echo "Set TGFAKE_BIN to the tgfake binary; see docs/TGFAKE.md"; exit 1; }
	TGFAKE_BIN="$(TGFAKE_BIN)" cargo test --locked --test tgfake -- --ignored --test-threads=1

# requirements.lock: pinned dependencies. After a change in requirements.txt (and in a Dependabot PR)
# run `make lock`; `npm run test:lock` checks they agree.
lock:
	$(UV) pip compile requirements.txt --universal --python-version 3.12 --generate-hashes \
	  --annotation-style line --custom-compile-command "make lock" -o requirements.lock

# newer Bot API spec into app/bundled/api.json; exit 10 = updated, then make the Rust spec-gaps test green
spec-update:
	$(PY) scripts/update_spec.py
