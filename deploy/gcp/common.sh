#!/usr/bin/env bash
# Shared by create_ladder_vm.sh and create_build_vm.sh: argument parsing, the static IP, the
# firewall rules and the startup script. Sourced, not run. The engine repository's
# deploy/gcp/common.sh is the model; the two stay alike on purpose.

# Defaults; the scripts' flags override them (each script sets name and machine_type first).
project=""
zone=""
name=""
env_file=""
machine_type=""
network="default"

usage_common() {
  cat >&2 << EOF
  --project ID        GCP project (required)
  --zone ZONE         e.g. australia-southeast1-b (required); the region is the zone minus its suffix
  --name NAME         VM name (default: $name)
  --env-file FILE     a filled-in ladder.env (deploy/vm/ladder.env.example) to install as
                      /etc/kfm/ladder.env on first boot; without it, fill it in over ssh
  --machine-type T    default: $machine_type (no spot/preemptible: they stop mid-job)
  --network NET       VPC network (default: $network)
EOF
  exit 2
}

parse_common() {
  while [[ $# -gt 0 ]]; do
    [[ $# -ge 2 ]] || usage
    case "$1" in
      --project) project="$2" ;;
      --zone) zone="$2" ;;
      --name) name="$2" ;;
      --env-file) env_file="$2" ;;
      --machine-type) machine_type="$2" ;;
      --network) network="$2" ;;
      *) usage ;;
    esac
    shift 2
  done
  [[ -n "$project" && -n "$zone" && -n "$name" ]] || usage
  [[ -z "$env_file" || -f "$env_file" ]] || {
    echo "error: $env_file not found" >&2
    exit 2
  }
  region="${zone%-*}"
  gc=(gcloud --project "$project" --quiet)
}

# Reserves (or finds) the VM's static external IP; sets $address.
reserve_address() {
  if ! "${gc[@]}" compute addresses describe "$name-ip" --region "$region" > /dev/null 2>&1; then
    "${gc[@]}" compute addresses create "$name-ip" --region "$region"
  fi
  address="$("${gc[@]}" compute addresses describe "$name-ip" --region "$region" --format='value(address)')"
  echo "static IP $name-ip: $address"
}

# create_firewall NAME TARGET_TAG RULES DESCRIPTION [SOURCE_TAG]
# From anywhere, or with SOURCE_TAG only from the VMs carrying that tag (inside the network).
create_firewall() {
  local source=(--source-ranges 0.0.0.0/0)
  [[ -z "${5:-}" ]] || source=(--source-tags "$5")
  if ! "${gc[@]}" compute firewall-rules describe "$1" > /dev/null 2>&1; then
    "${gc[@]}" compute firewall-rules create "$1" --network "$network" --direction INGRESS \
      --action ALLOW --rules "$3" --target-tags "$2" "${source[@]}" \
      --description "$4"
  fi
}

# Writes the startup script to $1: it unpacks deploy/vm (embedded, so the VM needs no
# checkout and the repository may be private), installs the env file on first boot when one
# was given, and runs install.sh with the given flags. Runs on every boot; install.sh is
# idempotent.
write_startup_script() {
  local out="$1"
  shift
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  {
    echo '#!/usr/bin/env bash'
    echo 'set -euo pipefail'
    echo 'mkdir -p /opt/kfm-ladder-deploy && rm -rf /opt/kfm-ladder-deploy/vm'
    echo "base64 -d << 'KFM_TAR' | tar xzf - -C /opt/kfm-ladder-deploy"
    tar czf - --owner=0 --group=0 -C "$here/.." vm | base64
    echo 'KFM_TAR'
    if [[ -n "$env_file" ]]; then
      echo 'if [[ ! -f /etc/kfm/ladder.env ]]; then'
      echo '  install -d -m 0755 /etc/kfm'
      echo "  base64 -d > /etc/kfm/ladder.env << 'KFM_ENV'"
      base64 < "$env_file"
      echo 'KFM_ENV'
      echo '  chmod 0600 /etc/kfm/ladder.env'
      echo 'fi'
    fi
    printf '/opt/kfm-ladder-deploy/vm/install.sh'
    printf ' %q' "$@"
    echo
  } > "$out"
}
