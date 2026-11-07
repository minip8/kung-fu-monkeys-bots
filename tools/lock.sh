# shellcheck shell=bash disable=SC2034 # the variables are for the scripts that source this
# Sourced by the tools: reads engine.lock into $lock_tag and $lock_commit, and sets $repo and
# $engine_repo (the engine's GitHub repository, overridable with $KFM_ENGINE_REPO).
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
engine_repo="${KFM_ENGINE_REPO:-minip8/kung-fu-monkeys}"
lock_value() { sed -n "s/^$1=\\([^[:space:]#]*\\).*/\\1/p" "$repo/engine.lock" | tail -n 1; }
lock_tag="$(lock_value tag)"
lock_commit="$(lock_value commit)"
