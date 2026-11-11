#!/usr/bin/env bash
# Restore drill for deploy/: the server setup (studio + Litestream, no Caddy) on a file replica gets a token and a
# draft; then the data volume is thrown away, Litestream restores studio.db, and the restored studio still knows
# both. CI runs it daily (.github/workflows/restore.yml). Locally: docker build -t rich-studio . && scripts/restore_check.sh
set -euo pipefail
IMAGE=${IMAGE:-rich-studio}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
P=rs-restore-check
cp "$ROOT"/deploy/{docker-compose.yml,litestream.yml,Caddyfile} "$WORK"/
mkdir -p "$WORK/secrets" "$WORK/replica" && chmod 777 "$WORK/replica"
echo "1:check" >"$WORK/secrets/tg_bot_token" && chmod 644 "$WORK/secrets/tg_bot_token"
cat >"$WORK/.env" <<ENV
STUDIO_DOMAIN=studio.test
STUDIO_TAG=restore-check
TG_UPDATES=off
STUDIO_BACKGROUND=off
ENV
echo "LITESTREAM_REPLICA_URL=file:///replica/studio" >"$WORK/litestream.env"
cat >"$WORK/docker-compose.check.yml" <<YML
services:
  litestream:
    volumes: ["$WORK/replica:/replica"]
YML
# the compose pulls STUDIO_TAG from its own image repo: tag the built image into exactly that one
IMAGE_REPO=$(grep -oP 'image: \K[^:\s]+(?=:\$\{STUDIO_TAG)' "$ROOT/deploy/docker-compose.yml")
docker tag "$IMAGE" "$IMAGE_REPO:restore-check"
dc() { docker compose -p "$P" --project-directory "$WORK" -f "$WORK/docker-compose.yml" -f "$WORK/docker-compose.check.yml" "$@"; }
cleanup() {
  dc down -v --remove-orphans >/dev/null 2>&1 || true
  # the replica belongs to uid 10001: a runner that is not root removes it through a container
  docker run --rm --user 0 -v "$WORK:/work" --entrypoint rm "$IMAGE" -rf /work/replica >/dev/null 2>&1 || true
  rm -rf "$WORK" || true
}
trap cleanup EXIT
# The native CLI reaches the service on loopback; the drill publishes no ports.
call() {
  dc exec -T -e STUDIO_TOKEN="$TOKEN" rich-studio rich-studio request "$@"
}
wait_ready() {
  for _ in $(seq 60); do
    dc exec -T rich-studio rich-studio healthcheck \
      >/dev/null 2>&1 && return 0
    sleep 1
  done
  dc logs rich-studio; return 1
}

echo "== first token (server mode does not start without one)"
TOKEN=$(dc run --rm --no-deps -T rich-studio rich-studio token add restore-check --level admin)
dc up -d rich-studio litestream
wait_ready
echo "== a draft to find after the restore"
call POST /api/drafts '{"title": "restore-check", "post": {"rich_message": {"blocks": []}}}' >/dev/null
sleep 5 # Litestream ships WAL changes every second
dc stop rich-studio litestream
echo "== data volume lost"
dc rm -f rich-studio litestream >/dev/null
docker volume rm "${P}_data" >/dev/null
echo "== restore from the replica"
dc run --rm --no-deps -T rich-studio true # a fresh volume takes /data with its owner (uid 10001) from the image
dc run --rm --no-deps -T litestream restore /data/studio.db
dc up -d rich-studio
wait_ready
drafts=$(call GET /api/drafts)
case "$drafts" in
  *restore-check*) echo "OK: token and draft came back from the replica" ;;
  *) echo "FAIL: the draft is not in the restored studio: $drafts"; exit 1 ;;
esac
