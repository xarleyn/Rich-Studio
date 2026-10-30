#!/usr/bin/env bash
# Activate only after a private candidate proxy passes live, read-only endpoint checks.
set -euo pipefail
[[ "$(id -u)" == 0 ]] || { echo 'Запустите под root на VPS.' >&2; exit 1; }
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
[[ -s secrets/trojan.url ]] || { echo 'Сохраните ссылку узла в deploy/secrets/trojan.url.' >&2; exit 1; }
[[ -s .env ]] || { echo 'Сначала настройте deploy/.env.' >&2; exit 1; }
exec 9>/var/lock/richpost-update.lock
flock -n 9 || { echo 'Другое обновление ещё выполняется.' >&2; exit 1; }
umask 077
richpost_probe="richpost-proxy-probe-$$"
richpost_backup=$(mktemp -d ./secrets/proxy-backup.XXXXXX)
cp -p .env "$richpost_backup/env"
richpost_had_config=0
if [[ -f secrets/egress.json ]]; then
  cp -p secrets/egress.json "$richpost_backup/config"
  richpost_had_config=1
fi
richpost_promoted=0
richpost_success=0
richpost_compose=(docker compose -f compose.simple.yml)
cleanup() {
  docker rm -f "$richpost_probe" > /dev/null 2>&1 || true
  if [[ "$richpost_promoted" == 1 && "$richpost_success" != 1 ]]; then
    cp -p "$richpost_backup/env" .env
    if [[ "$richpost_had_config" == 1 ]]; then
      cp -p "$richpost_backup/config" secrets/egress.json
    else
      rm -f secrets/egress.json
      "${richpost_compose[@]}" --profile proxy stop egress > /dev/null 2>&1 || true
    fi
    "${richpost_compose[@]}" up -d --no-build --force-recreate > /dev/null 2>&1 || true
    echo 'Прокси не включён; настройки возвращены. Проверьте состояние контейнеров.' >&2
  fi
  rm -f secrets/egress.next.json
  rm -rf -- "$richpost_backup"
}
trap cleanup EXIT
python3 proxy_config.py --input secrets/trojan.url --output secrets/egress.next.json
if ! EGRESS_CONFIG_FILE=./secrets/egress.next.json "${richpost_compose[@]}" --profile proxy run \
    --rm --no-deps egress -c /run/secrets/egress.json check > /dev/null 2>&1; then
  echo 'sing-box не принял профиль. Секреты не выводятся; проверьте формат узла.' >&2
  exit 1
fi
EGRESS_CONFIG_FILE=./secrets/egress.next.json "${richpost_compose[@]}" --profile proxy run \
  -d --rm --no-deps --name "$richpost_probe" egress > /dev/null 2>&1
for ((richpost_attempt=0; richpost_attempt<15; richpost_attempt++)); do
  if [[ "$(docker inspect --format '{{.State.Health.Status}}' "$richpost_probe")" == healthy ]]; then break; fi
  sleep 1
done
# The only Telegram request is the bot's read-only getMe. No messages are sent.
if ! "${richpost_compose[@]}" run --rm --no-deps -T \
    -e "TG_PROXY_URL=http://$richpost_probe:3128" -e TG_PROXY_URL_FILE= rich-studio python - <<'PY' > /dev/null 2>&1
import asyncio
import urllib.request

import settings
import telegram

proxy = settings.get().telegram_proxy
opener = urllib.request.build_opener(urllib.request.ProxyHandler({'https': proxy}))
with opener.open('https://acme-v02.api.letsencrypt.org/directory', timeout=20) as response:
    assert response.status == 200
asyncio.run(telegram.call('getMe', timeout=20))
PY
then
  echo 'Профиль не прошёл проверку Telegram/ACME. Рабочие маршруты не изменены.' >&2
  exit 1
fi
richpost_promoted=1
mv secrets/egress.next.json secrets/egress.json
python3 - <<'PY'
from pathlib import Path

path = Path('.env')
values = {'TG_PROXY_URL': 'http://egress:3128', 'ACME_PROXY_URL': 'http://egress:3128'}
lines = path.read_text().splitlines()
profiles = []
for line in lines:
    if line.startswith('COMPOSE_PROFILES='):
        profiles += [value.strip() for value in line.split("=", 1)[1].strip("\"'").split(",") if value.strip()]
values['COMPOSE_PROFILES'] = ','.join(dict.fromkeys([*profiles, 'proxy']))
lines = [line for line in lines if line.split("=", 1)[0].strip() not in {*values, "TG_PROXY_URL_FILE"}]
path.write_text("\n".join(lines + [key + "=" + value for key, value in values.items()]) + "\n")
path.chmod(0o600)
PY
"${richpost_compose[@]}" up -d --no-build --force-recreate --wait --wait-timeout 120
richpost_success=1
echo 'Telegram и Let’s Encrypt подключены через Trojan; остальные маршруты сохранены.'
