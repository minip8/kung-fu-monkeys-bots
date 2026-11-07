#!/usr/bin/env bash
# The ladder VM (deploy/README.md "Day one"): kfm-ladder-1, running Caddy, the API and portal,
# Postgres, the match workers and the nightly dump. Reserves a static external IP, opens TCP
# 80/443 to tag kfm-ladder (rule kfm-ladder-web) and TCP 5432 from tag kfm-build only (rule
# kfm-ladder-postgres: the build worker's database connection, inside the network), and
# creates an e2-small on Debian 12 (the host only runs containers) whose startup script runs
# deploy/vm/install.sh --ladder.
#
#   deploy/gcp/create_ladder_vm.sh --project my-proj --zone australia-southeast1-b \
#       --env-file ladder.env
#
# Idempotent up to the instance: existing addresses and firewall rules are reused; an existing
# VM of that name makes `instances create` fail.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
. "$here/common.sh"
name="kfm-ladder-1"
machine_type="e2-small"
match_workers=""

usage() {
  echo "usage: $0 --project ID --zone ZONE [--match-workers N] [options]" >&2
  echo "  --match-workers N   kfm-ladder-match@ instances (default: KFM_MATCH_WORKERS, else 2)" >&2
  usage_common
}
args=()
while [[ $# -gt 0 ]]; do
  if [[ "$1" == --match-workers ]]; then
    [[ $# -ge 2 && "$2" =~ ^[0-9]+$ ]] || usage
    match_workers="$2"
    shift 2
  else
    args+=("$1")
    shift
  fi
done
parse_common "${args[@]}"

reserve_address
create_firewall kfm-ladder-web kfm-ladder tcp:80,tcp:443 \
  "Caddy in front of the Kung Fu Monkeys ladder (HTTP for ACME, HTTPS)"
create_firewall kfm-ladder-postgres kfm-ladder tcp:5432 \
  "The ladder's Postgres, from the build VM only" kfm-build

startup="$(mktemp)"
trap 'rm -f "$startup"' EXIT
if [[ -n "$match_workers" ]]; then
  write_startup_script "$startup" --ladder --match-workers "$match_workers"
else
  write_startup_script "$startup" --ladder
fi

"${gc[@]}" compute instances create "$name" \
  --zone "$zone" \
  --machine-type "$machine_type" \
  --provisioning-model STANDARD \
  --image-family debian-12 --image-project debian-cloud \
  --boot-disk-size 20GB --boot-disk-type pd-balanced \
  --network "$network" --address "$address" \
  --tags kfm-ladder \
  --metadata-from-file startup-script="$startup"

cat << EOF

Created $name in $zone at $address.
Next (deploy/README.md "Day one"):
  1. DNS: an A record for the ladder name (KFM_LADDER_DOMAIN) -> $address, DNS only (grey cloud)
  2. ladder.env: $([[ -n "$env_file" ]] && echo "installed from $env_file" || echo "gcloud compute ssh $name --zone $zone, edit /etc/kfm/ladder.env, then
     sudo /opt/kfm-ladder-deploy/vm/install.sh --ladder")
  3. curl https://<ladder name>/healthz
  Startup log: gcloud compute ssh $name --zone $zone -- sudo journalctl -u google-startup-scripts
EOF
