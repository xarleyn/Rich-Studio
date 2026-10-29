#!/usr/bin/env bash
# Run on the chosen VPS with Docker installed. Never prints or passes the bot token as a command argument.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
if ! command -v docker >/dev/null; then bash ./install-docker.sh; fi
docker compose version >/dev/null

if [[ ! -f .env ]]; then
  cp -- .env.example .env
  chmod 600 .env
  echo 'Создан deploy/.env: перед первым запуском задайте свои домены и имя бота.'
fi
if grep -q '^STUDIO_DOMAIN=app.example.com$' .env; then
  echo 'Замените примеры доменов и TG_BOT_USERNAME в deploy/.env на свои, затем повторите запуск.' >&2
  exit 1
fi

# Keep the first launch on a small VPS within its memory budget; preserve explicit settings.
richpost_memory_kb=$(awk '/MemTotal:/ {print $2}' /proc/meminfo)
if (( richpost_memory_kb < 1500000 )); then
  if ! grep -q '^APP_MEMORY_LIMIT=' .env; then printf '\nAPP_MEMORY_LIMIT=384m\n' >> .env; fi
  if ! grep -q '^PROXY_MEMORY_LIMIT=' .env; then printf 'PROXY_MEMORY_LIMIT=96m\n' >> .env; fi
  echo 'Включены ограничения памяти для небольшого VPS (приложение 384 МБ, прокси 96 МБ).'
fi

if [[ ! -s secrets/tg_bot_token ]]; then
  if [[ ! -t 0 ]]; then
    echo 'Запустите в терминале: токен бота нужно ввести скрыто или заранее положить в deploy/secrets/tg_bot_token.' >&2
    exit 1
  fi
  read -rsp 'Токен бота из BotFather (ввод скрыт): ' richpost_bot_token
  printf '\n'
  if [[ ! "$richpost_bot_token" =~ ^[0-9]+:[A-Za-z0-9_-]+$ ]]; then
    echo 'Токен не похож на токен BotFather.' >&2
    exit 1
  fi
  mkdir -p secrets
  (umask 077; printf '%s\n' "$richpost_bot_token" > secrets/tg_bot_token)
  unset richpost_bot_token
fi

if [[ "$(id -u)" == 0 ]]; then
  chown 10001:10001 secrets/tg_bot_token
  chmod 400 secrets/tg_bot_token
else
  sudo chown 10001:10001 secrets/tg_bot_token
  sudo chmod 400 secrets/tg_bot_token
fi

docker compose -f compose.simple.yml config --quiet
docker compose -f compose.simple.yml up -d --build
echo 'Контейнеры запущены. После настройки DNS и выпуска сертификатов:'
echo 'Адреса лендинга и приложения заданы в deploy/.env.'
echo 'В BotFather через /setdomain привяжите к боту домен приложения.'
echo 'Проверка: docker compose -f deploy/compose.simple.yml logs --tail=50'
