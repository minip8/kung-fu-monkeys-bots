#!/usr/bin/env bash
# Makes gVisor (runsc, https://gvisor.dev) Docker's default runtime on the build VM
# (architecture §8 "The build sandbox": a public service runs the build containers under
# gVisor; deploy/README.md "What gVisor protects against"). Installs runsc from gVisor's apt
# repository, writes the `runsc` runtime and `"default-runtime": "runsc"` into
# /etc/docker/daemon.json (keeping any other key there), restarts Docker and checks that a
# container runs under it. Idempotent; install.sh --build runs it on every boot.
#
#   sudo deploy/vm/install-gvisor.sh
#
# With runsc the default, every container the build worker starts is a gVisor sandbox
# whatever flags it passes; kfm-ladder-build.service asks for `--runtime runc` itself,
# because a gVisor sandbox cannot reach the host's Docker socket. Needs Docker first
# (install.sh installs it). runsc runs on a plain VM (the systrap platform: no KVM or nested
# virtualisation needed).
set -euo pipefail
[[ $EUID -eq 0 ]] || {
  echo "error: run as root" >&2
  exit 1
}
command -v docker > /dev/null || {
  echo "error: Docker is not installed (install.sh installs it first)" >&2
  exit 1
}
export DEBIAN_FRONTEND=noninteractive

# --- runsc (https://gvisor.dev/docs/user_guide/install/) ----------------------------------
if ! command -v runsc > /dev/null; then
  apt-get update -q
  apt-get install -y -q --no-install-recommends ca-certificates curl gnupg
  curl -fsSL https://gvisor.dev/archive.key \
    | gpg --dearmor --yes -o /usr/share/keyrings/gvisor-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg]" \
    "https://storage.googleapis.com/gvisor/releases release main" \
    > /etc/apt/sources.list.d/gvisor.list
  chmod a+r /usr/share/keyrings/gvisor-archive-keyring.gpg /etc/apt/sources.list.d/gvisor.list
  apt-get update -q
  apt-get install -y -q --no-install-recommends runsc
fi
runsc --version 2>&1 | sed -n 1p
runsc_path="$(command -v runsc)"

# --- /etc/docker/daemon.json ------------------------------------------------------------
# Merged, not overwritten: other keys (log options, a registry mirror) stay. python3 is in
# Debian's and Ubuntu's cloud images.
install -d -m 0755 /etc/docker
daemon=/etc/docker/daemon.json
new="$(mktemp)"
trap 'rm -f "$new"' EXIT
python3 - "$daemon" "$runsc_path" > "$new" << 'PY'
import json, os, sys
path, runsc = sys.argv[1], sys.argv[2]
conf = {}
if os.path.exists(path) and os.path.getsize(path) > 0:
    with open(path) as f:
        conf = json.load(f)
conf.setdefault("runtimes", {})["runsc"] = {"path": runsc}
conf["default-runtime"] = "runsc"
json.dump(conf, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
if ! cmp -s "$new" "$daemon"; then
  install -m 0644 "$new" "$daemon"
  echo "wrote $daemon; restarting Docker (its containers restart with it)"
  systemctl restart docker
fi

# --- Check ------------------------------------------------------------------------------
[[ "$(docker info --format '{{.DefaultRuntime}}')" == runsc ]] || {
  echo "error: Docker's default runtime is not runsc (docker info)" >&2
  exit 1
}
docker run --rm --runtime=runsc hello-world > /dev/null
# Inside gVisor, dmesg is gVisor's own boot log ("Starting gVisor..."); under runc it is the
# host's (or refused). This is the check deploy/README.md gives.
# Captured first: grep -q stopping early would fail the pipeline under pipefail.
docker run --rm --network none hello-world > /dev/null
log="$(docker run --rm --network none debian:trixie-slim dmesg 2> /dev/null || true)"
if grep -q gVisor <<< "$log"; then
  echo "gVisor: the default runtime is runsc, and a plain docker run is a gVisor sandbox"
else
  echo "error: a plain docker run does not run under gVisor" >&2
  exit 1
fi
