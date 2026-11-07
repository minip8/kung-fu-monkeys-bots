#!/usr/bin/env bash
# Sets up a ladder VM (deploy/README.md): Docker from Docker's apt repository, then per role
#   --ladder  kfm-ladder-1: Caddy from its apt repository, Postgres, the API and portal, N
#             match workers (KFM_MATCH_WORKERS in the env file, or --match-workers N) and
#             the nightly database dump
#   --build   kfm-build-1: gVisor as Docker's default runtime (install-gvisor.sh) and the
#             build worker
# running the GHCR images at KFM_IMAGE_TAG. Idempotent: safe to re-run (the GCP startup script
# runs it on every boot), and re-running with a smaller N stops the extra match workers.
#
#   sudo deploy/vm/install.sh --ladder [--match-workers N]
#   sudo deploy/vm/install.sh --build
#
# Configuration is /etc/kfm/ladder.env (ladder.env.example). When it is missing the example
# is installed and nothing is started: fill it in and run again.
set -euo pipefail

usage() {
  echo "usage: $0 --ladder [--match-workers N] | --build" >&2
  exit 2
}

role=""
workers=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ladder | --build)
      [[ -z "$role" ]] || usage
      role="${1#--}"
      ;;
    --match-workers)
      [[ $# -ge 2 && "$2" =~ ^[0-9]+$ ]] || usage
      workers="$2"
      shift
      ;;
    -h | --help) usage ;;
    *) usage ;;
  esac
  shift
done
[[ -n "$role" ]] || usage
[[ "$role" == ladder || -z "$workers" ]] || usage
[[ $EUID -eq 0 ]] || {
  echo "error: run as root" >&2
  exit 1
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
env_file=/etc/kfm/ladder.env
export DEBIAN_FRONTEND=noninteractive

# shellcheck source=/dev/null
. /etc/os-release # ID (debian|ubuntu), VERSION_CODENAME

apt_updated=0
apt_install() {
  if [[ $apt_updated -eq 0 ]]; then
    apt-get update -q
    apt_updated=1
  fi
  apt-get install -y -q --no-install-recommends "$@"
}

# --- Docker Engine (https://docs.docker.com/engine/install/) ------------------------------
if ! command -v docker > /dev/null; then
  apt_install ca-certificates curl gnupg
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc]" \
    "https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable" \
    > /etc/apt/sources.list.d/docker.list
  apt_updated=0
  apt_install docker-ce docker-ce-cli containerd.io
fi
systemctl enable --now docker

# --- Caddy (https://caddyserver.com/docs/install#debian-ubuntu-raspbian) ------------------
if [[ $role == ladder ]] && ! command -v caddy > /dev/null; then
  apt_install ca-certificates curl gnupg
  curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  chmod a+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
  apt_updated=0
  apt_install caddy
fi

# --- gVisor (build VM) ------------------------------------------------------------------
[[ $role == ladder ]] || "$here/install-gvisor.sh"

# --- Units and configuration -------------------------------------------------------------
if [[ $role == ladder ]]; then
  for unit in kfm-postgres.service kfm-postgres-backup.service kfm-postgres-backup.timer \
    kfm-ladder-api.service kfm-ladder-match@.service; do
    install -m 0644 "$here/$unit" "/etc/systemd/system/$unit"
  done
  install -m 0644 "$here/Caddyfile" /etc/caddy/Caddyfile
  install -d /etc/systemd/system/caddy.service.d
  printf '[Service]\nEnvironmentFile=/etc/kfm/ladder.env\n' \
    > /etc/systemd/system/caddy.service.d/kfm-ladder.conf
  install -d -m 0700 /var/lib/kfm/postgres
else
  install -m 0644 "$here/kfm-ladder-build.service" /etc/systemd/system/kfm-ladder-build.service
  install -d -m 0700 /var/lib/kfm-ladder/build
fi
systemctl daemon-reload

install -d -m 0755 /etc/kfm
if [[ ! -f $env_file ]]; then
  install -m 0600 "$here/ladder.env.example" "$env_file"
  echo "Installed $env_file from the example: fill it in (deploy/README.md), then run" >&2
  echo "  sudo $0 --$role" >&2
  exit 0
fi
chmod 0600 "$env_file"

# ladder.env is systemd EnvironmentFile syntax, not shell: read keys, never source it.
env_get() { sed -n "s/^$1=//p" "$env_file" | tail -n 1; }
KFM_IMAGE_REPO="$(env_get KFM_IMAGE_REPO)"
KFM_IMAGE_TAG="$(env_get KFM_IMAGE_TAG)"
KFM_GHCR_USER="$(env_get KFM_GHCR_USER)"
KFM_GHCR_TOKEN="$(env_get KFM_GHCR_TOKEN)"
required=(KFM_IMAGE_REPO KFM_IMAGE_TAG DATABASE_URL KFM_LADDER_S3_ENDPOINT KFM_LADDER_S3_BUCKET
  KFM_LADDER_S3_ACCESS_KEY KFM_LADDER_S3_SECRET_KEY KFM_LADDER_S3_REGION)
[[ $role == build ]] || required+=(POSTGRES_PASSWORD KFM_POSTGRES_LISTEN KFM_LADDER_DOMAIN
  KFM_LADDER_LISTEN KFM_LADDER_PUBLIC_URL KFM_LADDER_SESSION_KEY KFM_LADDER_GITHUB_CLIENT_ID
  KFM_LADDER_GITHUB_CLIENT_SECRET KFM_LADDER_GOOGLE_CLIENT_ID KFM_LADDER_GOOGLE_CLIENT_SECRET)
for key in "${required[@]}"; do
  if [[ -z "$(env_get "$key")" ]]; then
    echo "error: $key is empty in $env_file" >&2
    exit 1
  fi
done
if [[ $role == ladder ]]; then
  [[ "$(env_get KFM_LADDER_SESSION_KEY)" =~ ^[0-9a-fA-F]{64}$ ]] || {
    echo "error: KFM_LADDER_SESSION_KEY must be 64 hex characters (openssl rand -hex 32)" >&2
    exit 1
  }
  [[ -n "$workers" ]] || workers="$(env_get KFM_MATCH_WORKERS)"
  workers="${workers:-2}"
  [[ "$workers" =~ ^[0-9]+$ ]] || {
    echo "error: KFM_MATCH_WORKERS is not a number" >&2
    exit 1
  }
else
  # The build VM holds no portal secret (deploy/README.md "What runs where").
  for key in KFM_LADDER_SESSION_KEY KFM_LADDER_GITHUB_CLIENT_SECRET KFM_LADDER_GOOGLE_CLIENT_SECRET \
    POSTGRES_PASSWORD; do
    [[ -z "$(env_get "$key")" ]] || echo "warning: $key is set in $env_file; the build VM does not need it" >&2
  done
fi

# --- Images ----------------------------------------------------------------------------
if [[ -n "$KFM_GHCR_TOKEN" ]]; then
  echo "$KFM_GHCR_TOKEN" | docker login ghcr.io -u "${KFM_GHCR_USER:?set with KFM_GHCR_TOKEN}" --password-stdin
fi
# An image already present (built on this machine, or loaded) is used as is; update.sh is
# what pulls a newer build of the same tag.
pull_unless_present() {
  docker image inspect "$1" > /dev/null 2>&1 || docker pull "$1"
}
pull_unless_present "$KFM_IMAGE_REPO/kfm-ladder:$KFM_IMAGE_TAG"
if [[ $role == ladder ]]; then
  pull_unless_present postgres:17
  pull_unless_present rclone/rclone:1.71.2
else
  pull_unless_present "$KFM_IMAGE_REPO/kfm-bot-build:$KFM_IMAGE_TAG"
fi

# --- Start -----------------------------------------------------------------------------
if [[ $role == build ]]; then
  systemctl enable --now kfm-ladder-build.service
  echo "kfm-ladder: build worker running $KFM_IMAGE_REPO/kfm-ladder:$KFM_IMAGE_TAG under gVisor builds"
  exit 0
fi

systemctl enable --now kfm-postgres.service kfm-ladder-api.service kfm-postgres-backup.timer
systemctl enable caddy
systemctl restart caddy
for ((i = 1; i <= workers; i++)); do
  systemctl enable --now "kfm-ladder-match@$i.service"
done
# Instances beyond N from an earlier run (running or enabled).
shopt -s nullglob
for unit in $(systemctl list-units --all --plain --no-legend 'kfm-ladder-match@*.service' | awk '{print $1}') \
  /etc/systemd/system/multi-user.target.wants/kfm-ladder-match@*.service; do
  n="${unit##*/kfm-ladder-match@}"
  n="${n#kfm-ladder-match@}"
  n="${n%.service}"
  if [[ "$n" =~ ^[0-9]+$ ]] && ((n > workers)); then
    systemctl disable --now "kfm-ladder-match@$n.service"
  fi
done

echo "kfm-ladder: Postgres, the API, Caddy and $workers match worker(s) running $KFM_IMAGE_REPO/kfm-ladder:$KFM_IMAGE_TAG"
