#!/usr/bin/env bash
# The ladder database's backups (deploy/README.md "Backups and restore"): a pg_dump of the
# running kfm-postgres container streamed into the object store under backups/postgres/,
# with rclone in its own container (no tool installed on the host), and the restore.
#
#   sudo postgres-backup.sh dump              # one dump now, then prune older than KFM_BACKUP_KEEP_DAYS
#   sudo postgres-backup.sh list              # the dumps in the bucket
#   sudo postgres-backup.sh restore KEY --yes # stop the API and match workers, restore KEY, start them
#
# KEY is a name from `list` (kfm-20261107T033000Z.dump). kfm-postgres-backup.timer runs `dump`
# nightly. The store is the service's own (KFM_LADDER_S3_* in /etc/kfm/ladder.env); rclone
# gets it through RCLONE_CONFIG_STORE_* variables, passed to its container by name so no key
# appears on a command line.
set -euo pipefail

usage() {
  echo "usage: $0 dump | list | restore KEY --yes" >&2
  exit 2
}
[[ $# -ge 1 ]] || usage
[[ $EUID -eq 0 ]] || {
  echo "error: run as root" >&2
  exit 1
}
env_file=/etc/kfm/ladder.env
[[ -f $env_file ]] || {
  echo "error: $env_file missing; run install.sh first" >&2
  exit 1
}
# ladder.env is systemd EnvironmentFile syntax, not shell: read keys, never source it.
env_get() { sed -n "s/^$1=//p" "$env_file" | tail -n 1; }

rclone_image=rclone/rclone:1.71.2
prefix=backups/postgres
endpoint="$(env_get KFM_LADDER_S3_ENDPOINT)"
bucket="$(env_get KFM_LADDER_S3_BUCKET)"
keep_days="$(env_get KFM_BACKUP_KEEP_DAYS)"
keep_days="${keep_days:-30}"
[[ -n "$endpoint" && -n "$bucket" ]] || {
  echo "error: KFM_LADDER_S3_ENDPOINT and KFM_LADDER_S3_BUCKET must be set in $env_file" >&2
  exit 1
}
[[ "$keep_days" =~ ^[0-9]+$ ]] || {
  echo "error: KFM_BACKUP_KEEP_DAYS is not a number of days" >&2
  exit 1
}
export RCLONE_CONFIG_STORE_TYPE=s3
case "$endpoint" in
  *.r2.cloudflarestorage.com*) export RCLONE_CONFIG_STORE_PROVIDER=Cloudflare ;;
  *) export RCLONE_CONFIG_STORE_PROVIDER=Other ;; # MinIO and the rest
esac
RCLONE_CONFIG_STORE_ENDPOINT="$endpoint"
RCLONE_CONFIG_STORE_ACCESS_KEY_ID="$(env_get KFM_LADDER_S3_ACCESS_KEY)"
RCLONE_CONFIG_STORE_SECRET_ACCESS_KEY="$(env_get KFM_LADDER_S3_SECRET_KEY)"
RCLONE_CONFIG_STORE_REGION="$(env_get KFM_LADDER_S3_REGION)"
export RCLONE_CONFIG_STORE_ENDPOINT RCLONE_CONFIG_STORE_ACCESS_KEY_ID \
  RCLONE_CONFIG_STORE_SECRET_ACCESS_KEY RCLONE_CONFIG_STORE_REGION
# An object read/write token cannot create buckets; never try.
export RCLONE_CONFIG_STORE_NO_CHECK_BUCKET=true
export RCLONE_CONFIG_STORE_FORCE_PATH_STYLE=true

rclone() {
  docker run --rm -i --network host --log-driver none \
    -e RCLONE_CONFIG_STORE_TYPE -e RCLONE_CONFIG_STORE_PROVIDER -e RCLONE_CONFIG_STORE_ENDPOINT \
    -e RCLONE_CONFIG_STORE_ACCESS_KEY_ID -e RCLONE_CONFIG_STORE_SECRET_ACCESS_KEY \
    -e RCLONE_CONFIG_STORE_REGION -e RCLONE_CONFIG_STORE_NO_CHECK_BUCKET \
    -e RCLONE_CONFIG_STORE_FORCE_PATH_STYLE \
    "$rclone_image" "$@"
}

case "$1" in
  dump)
    [[ $# -eq 1 ]] || usage
    key="kfm-$(date -u +%Y%m%dT%H%M%SZ).dump"
    # The custom format (-Fc): compressed, and pg_restore can --clean into a live database.
    # pipefail: a failed pg_dump fails the unit even though rcat stored what it was given.
    docker exec kfm-postgres pg_dump -U kfm -d kfm -Fc \
      | rclone rcat "store:$bucket/$prefix/$key"
    echo "dumped to $bucket/$prefix/$key"
    rclone delete --min-age "${keep_days}d" "store:$bucket/$prefix/"
    ;;
  list)
    [[ $# -eq 1 ]] || usage
    rclone lsl "store:$bucket/$prefix/"
    ;;
  restore)
    [[ $# -eq 3 && "$3" == --yes && "$2" =~ ^kfm-[0-9TZ]+\.dump$ ]] || usage
    key="$2"
    # The dump must exist before anything stops.
    found="$(rclone lsf "store:$bucket/$prefix/$key" 2> /dev/null || true)"
    grep -qxF "$key" <<< "$found" || {
      echo "error: no $bucket/$prefix/$key (postgres-backup.sh list)" >&2
      exit 1
    }
    mapfile -t workers < <(systemctl list-units --plain --no-legend --state=active \
      'kfm-ladder-match@*.service' | awk '{print $1}')
    echo "stopping kfm-ladder-api ${workers[*]} (stop kfm-ladder-build on the build VM too)"
    systemctl stop kfm-ladder-api.service "${workers[@]}"
    # --clean --if-exists drops each object before recreating it, so the database ends up as
    # the dump has it; --single-transaction makes a failed restore change nothing.
    rclone cat "store:$bucket/$prefix/$key" \
      | docker exec -i kfm-postgres pg_restore -U kfm -d kfm --clean --if-exists \
        --no-owner --single-transaction
    systemctl start kfm-ladder-api.service "${workers[@]}"
    echo "restored $key; start kfm-ladder-build on the build VM again"
    ;;
  *) usage ;;
esac
