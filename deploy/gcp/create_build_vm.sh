#!/usr/bin/env bash
# The build VM (deploy/README.md "Day one"): kfm-build-1, running only the build worker, with
# Docker under gVisor (runsc as the default runtime) for the sandbox containers. It gets no
# inbound rule of its own (tag kfm-build only; the ladder VM's kfm-ladder-postgres rule lets it
# reach Postgres) and an ephemeral external IP for its outbound traffic (GHCR, the object
# store): an e2-medium on Debian 12 whose startup script runs deploy/vm/install.sh --build.
#
#   deploy/gcp/create_build_vm.sh --project my-proj --zone australia-southeast1-b \
#       --env-file build.env
#
# The env file is the build VM's own ladder.env: the [build] lines of ladder.env.example, with
# DATABASE_URL naming the ladder VM (postgres://kfm:...@kfm-ladder-1:5432/kfm), no OAuth
# secret and no session key. An existing VM of that name makes `instances create` fail.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
. "$here/common.sh"
name="kfm-build-1"
machine_type="e2-medium"

usage() {
  echo "usage: $0 --project ID --zone ZONE [options]" >&2
  usage_common
}
parse_common "$@"

startup="$(mktemp)"
trap 'rm -f "$startup"' EXIT
write_startup_script "$startup" --build

# 30 GB: the bot build image (emsdk) is a few GB, and an update holds two releases of it
# until the prune.
"${gc[@]}" compute instances create "$name" \
  --zone "$zone" \
  --machine-type "$machine_type" \
  --provisioning-model STANDARD \
  --image-family debian-12 --image-project debian-cloud \
  --boot-disk-size 30GB --boot-disk-type pd-balanced \
  --network "$network" \
  --tags kfm-build \
  --metadata-from-file startup-script="$startup"

cat << EOF

Created $name in $zone (no inbound port of its own).
Next (deploy/README.md "Day one"):
  1. ladder.env: $([[ -n "$env_file" ]] && echo "installed from $env_file" || echo "gcloud compute ssh $name --zone $zone --tunnel-through-iap, edit
     /etc/kfm/ladder.env, then sudo /opt/kfm-ladder-deploy/vm/install.sh --build")
  2. gVisor: gcloud compute ssh $name --zone $zone --tunnel-through-iap -- \\
       sudo docker run --rm debian:trixie-slim dmesg     # "Starting gVisor..."
  3. gcloud compute ssh $name --zone $zone --tunnel-through-iap -- sudo journalctl -u kfm-ladder-build
  Startup log: gcloud compute ssh $name --zone $zone --tunnel-through-iap -- sudo journalctl -u google-startup-scripts
EOF
