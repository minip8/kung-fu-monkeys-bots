#!/usr/bin/env bash
# Moves a ladder VM to a new release (deploy/README.md "Updating"): pulls the images at TAG,
# refreshes the systemd units and the Caddyfile from this directory, records TAG in
# /etc/kfm/ladder.env and restarts the service's units. Postgres is never restarted or pulled
# here (deploy/README.md "Postgres").
#
#   sudo deploy/vm/update.sh v0.2.0
#
# kfm-ladder-1: the API first (its ExecStartPre runs `kfm-ladder migrate`, so the schema is
# the new release's before any new worker starts), then the match workers one at a time.
# kfm-build-1: the build worker. Update kfm-ladder-1 first (release.yml takes KFM_LADDER_VMS
# in order). A worker stopped mid-job is safe: its claim goes stale and the job is re-queued.
set -euo pipefail

[[ $# -eq 1 && "$1" != -* ]] || {
  echo "usage: $0 TAG" >&2
  exit 2
}
tag="$1"
[[ $EUID -eq 0 ]] || {
  echo "error: run as root" >&2
  exit 1
}
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
env_file=/etc/kfm/ladder.env
[[ -f $env_file ]] || {
  echo "error: $env_file missing; run install.sh first" >&2
  exit 1
}
repo="$(sed -n 's/^KFM_IMAGE_REPO=//p' "$env_file" | tail -n 1)"
[[ -n $repo ]] || {
  echo "error: KFM_IMAGE_REPO is empty in $env_file" >&2
  exit 1
}

if systemctl is-enabled --quiet kfm-ladder-api.service 2> /dev/null; then
  role=ladder
elif systemctl is-enabled --quiet kfm-ladder-build.service 2> /dev/null; then
  role=build
else
  echo "error: neither kfm-ladder-api nor kfm-ladder-build is enabled; run install.sh first" >&2
  exit 1
fi

# Pull first: a missing tag fails here, before anything restarts.
docker pull "$repo/kfm-ladder:$tag"
[[ $role == ladder ]] || docker pull "$repo/kfm-bot-build:$tag"

# Refresh the units and the Caddyfile (the workflow ships deploy/vm before running this, so a
# release can change them); install.sh keeps the rest.
if [[ $role == ladder ]]; then
  for unit in kfm-postgres.service kfm-postgres-backup.service kfm-postgres-backup.timer \
    kfm-ladder-api.service kfm-ladder-match@.service; do
    install -m 0644 "$here/$unit" "/etc/systemd/system/$unit"
  done
  if ! cmp -s "$here/Caddyfile" /etc/caddy/Caddyfile; then
    install -m 0644 "$here/Caddyfile" /etc/caddy/Caddyfile
    systemctl reload caddy || systemctl restart caddy
  fi
else
  install -m 0644 "$here/kfm-ladder-build.service" /etc/systemd/system/kfm-ladder-build.service
fi
systemctl daemon-reload

if grep -q '^KFM_IMAGE_TAG=' "$env_file"; then
  sed -i "s|^KFM_IMAGE_TAG=.*|KFM_IMAGE_TAG=$tag|" "$env_file"
else
  echo "KFM_IMAGE_TAG=$tag" >> "$env_file"
fi

# restart_checked UNIT: restart it, give it time to settle, fail the update if it did not stay up.
restart_checked() {
  echo "restarting $1"
  systemctl restart "$1"
  sleep 5
  systemctl is-active --quiet "$1" || {
    echo "error: $1 did not stay up; journalctl -u $1" >&2
    exit 1
  }
}

if [[ $role == build ]]; then
  restart_checked kfm-ladder-build.service
  docker image prune -f > /dev/null
  echo "kfm-ladder: build worker now at $tag"
  exit 0
fi

# The API: migrate (ExecStartPre), then serve.
restart_checked kfm-ladder-api.service
listen="$(sed -n 's/^KFM_LADDER_LISTEN=//p' "$env_file" | tail -n 1)"
curl -fsS --max-time 5 "http://${listen:-127.0.0.1:8090}/healthz" > /dev/null \
  || echo "warning: http://${listen:-127.0.0.1:8090}/healthz did not answer yet"

mapfile -t units < <(systemctl list-units --plain --no-legend --state=active 'kfm-ladder-match@*.service' \
  | awk '{print $1}' | sort -V)
for unit in "${units[@]}"; do
  restart_checked "$unit"
done

docker image prune -f > /dev/null
echo "kfm-ladder: now at $tag (API and ${#units[@]} match worker(s))"
