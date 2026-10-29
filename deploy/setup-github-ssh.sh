#!/usr/bin/env bash
# One-time setup on the VPS. Credentials go to GitHub via stdin, never to stdout.
set -euo pipefail
[[ "$(id -u)" == 0 ]] || { echo 'Запустите под root на VPS.' >&2; exit 1; }
richpost_repo=${1:?Usage: setup-github-ssh.sh OWNER/REPO SSH_HOST}
richpost_host=${2:?Usage: setup-github-ssh.sh OWNER/REPO SSH_HOST}
[[ "$richpost_repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || exit 1
[[ "$richpost_host" =~ ^[A-Za-z0-9.-]+$ ]] || exit 1
[[ -s /etc/ssh/ssh_host_ed25519_key.pub ]] || { echo 'Не найден публичный SSH-ключ VPS.' >&2; exit 1; }
richpost_host_key=$(cut -d ' ' -f 1,2 /etc/ssh/ssh_host_ed25519_key.pub)

if ! command -v gh >/dev/null; then
  apt-get update
  apt-get install -y gh
fi

umask 077
install -d -m 700 /root/.ssh
richpost_key=/root/.ssh/richpost-actions
if [[ ! -f "$richpost_key" ]]; then
  ssh-keygen -q -t ed25519 -N '' -C richpost-github-actions -f "$richpost_key"
fi
chmod 600 "$richpost_key"
richpost_public_key=$(ssh-keygen -y -f "$richpost_key")
richpost_authorized_key="restrict $richpost_public_key richpost-github-actions"
touch /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
if ! grep -Fqx -- "$richpost_authorized_key" /root/.ssh/authorized_keys; then
  printf '%s\n' "$richpost_authorized_key" >> /root/.ssh/authorized_keys
fi

# A temporary CLI profile keeps the GitHub login off the VPS after setup.
richpost_gh_dir=$(mktemp -d /tmp/richpost-gh.XXXXXX)
trap 'rm -rf -- "$richpost_gh_dir"' EXIT
export GH_CONFIG_DIR="$richpost_gh_dir"
printf '%s %s\n' "$richpost_host" "$richpost_host_key" > "$richpost_gh_dir/known_hosts"
if [[ -z "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]]; then
  echo 'GitHub покажет код: откройте https://github.com/login/device и разрешите разовую настройку.'
  gh auth login --hostname github.com --git-protocol https --web
fi
gh secret set RICHPOST_SSH_KEY --repo "$richpost_repo" < "$richpost_key"
printf '%s' "$richpost_host" | gh secret set RICHPOST_SSH_HOST --repo "$richpost_repo"
printf '%s' root | gh secret set RICHPOST_SSH_USER --repo "$richpost_repo"
gh secret set RICHPOST_SSH_KNOWN_HOSTS --repo "$richpost_repo" < "$richpost_gh_dir/known_hosts"
echo "SSH-секреты настроены в $richpost_repo. Временный профиль GitHub будет удалён."
echo 'Теперь workflow deploy → operation=check проверит доступ, operation=deploy обновит сайт.'
