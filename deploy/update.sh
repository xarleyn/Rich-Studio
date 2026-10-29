#!/usr/bin/env bash
# Invoked over SSH with a git archive already uploaded to /tmp. Existing secrets and volumes stay in place.
set -euo pipefail
[[ "$(id -u)" == 0 ]] || { echo 'Обновление запускается под root.' >&2; exit 1; }
richpost_sha=${1:-}
[[ "$richpost_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Нужен полный SHA коммита.' >&2; exit 1; }
richpost_archive="/tmp/richpost-$richpost_sha.tar.gz"
richpost_root=/opt/richpost
[[ -s "$richpost_archive" ]] || { echo 'Архив релиза не загружен.' >&2; exit 1; }
[[ -s "$richpost_root/deploy/.env" && -s "$richpost_root/deploy/secrets/tg_bot_token" ]] || {
  echo 'Сначала выполните первоначальную установку RichPost.' >&2; exit 1;
}
exec 9>/var/lock/richpost-update.lock
flock -n 9 || { echo 'Другое обновление ещё выполняется.' >&2; exit 1; }
richpost_stage=$(mktemp -d /opt/richpost-stage.XXXXXX)
trap 'rm -rf -- "$richpost_stage"; rm -f -- "$richpost_archive"' EXIT
tar -xzf "$richpost_archive" -C "$richpost_stage"
[[ -f "$richpost_stage/Dockerfile" && -f "$richpost_stage/deploy/compose.simple.yml" ]] || exit 1

# Build before touching the running service. A failed build leaves the current version running.
docker build -t "richpost-rich-studio:$richpost_sha" "$richpost_stage"
ln -s "$richpost_root/deploy/.env" "$richpost_stage/deploy/.env"
ln -s "$richpost_root/deploy/secrets" "$richpost_stage/deploy/secrets"
docker compose -f "$richpost_stage/deploy/compose.simple.yml" config --quiet
docker compose -f "$richpost_stage/deploy/compose.simple.yml" run --rm --no-deps caddy \
  caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
cd "$richpost_root"
richpost_compose=(docker compose -f deploy/compose.simple.yml)
richpost_stamp="$(date -u +%Y%m%dT%H%M%SZ)-$richpost_sha"

# Consistent SQLite snapshots before the new version can run migrations, for all accounts.
# Media stays in the same volume. Restoring a snapshot is a separate, manual operation.
"${richpost_compose[@]}" exec -T rich-studio python - "$richpost_stamp" <<'PY'
import sqlite3
import sys
from pathlib import Path

root = Path('/data')
destination = root / 'deploy-backups' / sys.argv[1]
databases = [root / 'studio.db', *sorted((root / 'accounts').glob('*/studio.db'))]
for source in databases:
    if not source.is_file():
        continue
    target = destination / source.relative_to(root)
    target.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(source.as_uri() + '?mode=ro', uri=True, timeout=30) as src:
        with sqlite3.connect(target) as dst:
            src.backup(dst)
print('Снимки баз перед обновлением:', destination)
PY

install -d -m 700 /opt/richpost-releases
tar --exclude=deploy/.env --exclude=deploy/litestream.env --exclude=deploy/secrets \
  -czf "/opt/richpost-releases/$richpost_stamp.tar.gz" app Dockerfile .dockerignore requirements.lock deploy

# Do not copy .env, tokens or volumes from a release, even if they were accidentally added to Git.
mv "$richpost_root/app" "$richpost_stage/previous-app"
mv "$richpost_stage/app" "$richpost_root/app"
tar --exclude=app --exclude=deploy/.env --exclude=deploy/litestream.env --exclude=deploy/secrets \
  -xzf "$richpost_archive" -C "$richpost_root"
"${richpost_compose[@]}" config --quiet
docker tag "richpost-rich-studio:$richpost_sha" richpost-rich-studio:latest
"${richpost_compose[@]}" up -d --no-build --force-recreate

richpost_container=$("${richpost_compose[@]}" ps -q rich-studio)
for ((richpost_attempt=0; richpost_attempt<60; richpost_attempt++)); do
  richpost_health=$(docker inspect --format '{{.State.Health.Status}}' "$richpost_container")
  if [[ "$richpost_health" == healthy ]]; then
    richpost_caddy=$("${richpost_compose[@]}" ps -q caddy)
    [[ -n "$richpost_caddy" && "$(docker inspect --format '{{.State.Status}}' "$richpost_caddy")" == running ]] || {
      echo 'Приложение healthy, но Caddy не работает.' >&2; exit 1;
    }
    printf '%s\n' "$richpost_sha" > .deployed-sha
    echo "RichPost обновлён до $richpost_sha; контейнер healthy."
    exit 0
  fi
  if [[ "$richpost_health" == unhealthy ]]; then break; fi
  sleep 2
done
"${richpost_compose[@]}" ps
echo 'Новая версия не стала healthy. Снимки баз и предыдущие исходники сохранены; проверьте журнал.' >&2
exit 1
