#!/usr/bin/env bash
# Fresh Ubuntu VPS only; follows https://docs.docker.com/engine/install/ubuntu/#install-using-the-repository
set -euo pipefail
if command -v docker >/dev/null && docker compose version >/dev/null 2>&1; then exit 0; fi
if [[ "$(id -u)" != 0 ]]; then
  echo 'Для установки Docker запустите скрипт от root: sudo bash deploy/start.sh' >&2
  exit 1
fi
# shellcheck disable=SC1091
source /etc/os-release
if [[ "${ID:-}" != ubuntu ]]; then
  echo 'Автоустановка рассчитана на Ubuntu. Для другой ОС установите Docker Engine и Compose вручную.' >&2
  exit 1
fi
richpost_codename=${UBUNTU_CODENAME:-${VERSION_CODENAME:-}}
richpost_arch=$(dpkg --print-architecture)
[[ "$richpost_codename" =~ ^[a-z]+$ ]] || { echo 'Не удалось определить версию Ubuntu.' >&2; exit 1; }

apt-get update
apt-get install -y ca-certificates curl
install -d -m 0755 /etc/apt/keyrings
curl --fail --silent --show-error --location --retry 3 https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
printf 'Types: deb\nURIs: https://download.docker.com/linux/ubuntu\nSuites: %s\nComponents: stable\nArchitectures: %s\nSigned-By: /etc/apt/keyrings/docker.asc\n' \
  "$richpost_codename" "$richpost_arch" > /etc/apt/sources.list.d/docker.sources
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
systemctl enable --now docker
docker compose version
