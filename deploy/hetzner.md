# The same deployment on Hetzner Cloud

As the engine's deployment (its `deploy/hetzner.md`): Compute Engine first, Hetzner Cloud
when the credits end. Nothing in `deploy/vm/` is provider-specific, so the switch is two
`hcloud` VMs running the same startup script, a private network for the database, a dump
restored into the new Postgres and a DNS change; `update.sh` works as before. Install the CLI
(`hcloud context create kfm` with a project API token) and an SSH key
(`hcloud ssh-key create --name me --public-key-from-file ~/.ssh/id_ed25519.pub`).

Differences from GCP:

- The build VM reaches Postgres over a Hetzner private network instead of GCP's VPC and the
  `kfm-ladder-postgres` rule. Cloud Firewalls do not filter private-network traffic, and
  Postgres (`KFM_POSTGRES_LISTEN=*`) also listens on the public interface, where the
  `kfm-ladder` firewall below has no rule for 5432, so it is closed there.
- Firewalls attach to servers by name (or label), not network tags.
- A primary IP created on its own outlives the server, like GCP's reserved address.
- Traffic is included up to a quota per server, so replay downloads are not a cost (they come
  from R2 anyway).
- User data runs once, on first boot (cloud-init); the units it installs persist. Re-run
  `install.sh` by hand to change the number of match workers.
- There is no IAP: SSH is port 22 on each server's public IP, opened below; the release
  workflow's `vms` job is GCP-only, so updates are by hand (`update.sh TAG` over ssh, ladder
  VM first) until it grows an ssh path.

## Network and firewalls

```sh
hcloud network create --name kfm-ladder --ip-range 10.10.0.0/16
hcloud network add-subnet kfm-ladder --network-zone eu-central --type cloud --ip-range 10.10.0.0/24

hcloud firewall create --name kfm-ladder
hcloud firewall add-rule kfm-ladder --direction in --protocol tcp --port 22  --source-ips 0.0.0.0/0 --source-ips ::/0
hcloud firewall add-rule kfm-ladder --direction in --protocol tcp --port 80  --source-ips 0.0.0.0/0 --source-ips ::/0
hcloud firewall add-rule kfm-ladder --direction in --protocol tcp --port 443 --source-ips 0.0.0.0/0 --source-ips ::/0

hcloud firewall create --name kfm-build
hcloud firewall add-rule kfm-build --direction in --protocol tcp --port 22 --source-ips 0.0.0.0/0 --source-ips ::/0
```

Narrow port 22 to your own address when you can (`--source-ips <ip>/32`).

## The startup scripts

The GCP scripts embed `deploy/vm/` and the env file into one shell script; reuse it as user
data:

```sh
bash -c '. deploy/gcp/common.sh; env_file=ladder.env; write_startup_script ladder.sh --ladder'
bash -c '. deploy/gcp/common.sh; env_file=build.env;  write_startup_script build.sh --build'
```

`build.env`'s `DATABASE_URL` names the ladder VM's private address, which the next step fixes
with `--ip`: `postgres://kfm:<password>@10.10.0.2:5432/kfm`.

## The ladder VM (Caddy, the API, Postgres, match workers)

```sh
hcloud primary-ip create --name kfm-ladder-1-ip --type ipv4 --datacenter fsn1-dc14
hcloud server create --name kfm-ladder-1 --type cx22 --image debian-12 --location fsn1 \
  --ssh-key me --primary-ipv4 kfm-ladder-1-ip --firewall kfm-ladder \
  --user-data-from-file ladder.sh
hcloud server attach-to-network kfm-ladder-1 --network kfm-ladder --ip 10.10.0.2
hcloud primary-ip describe kfm-ladder-1-ip -o format='{{.IP}}'
```

Attached after creation so the address is the fixed one `build.env` names; Postgres listens on
every interface (`*`), so the one added after it started is covered.

## The build VM (the build worker under gVisor)

```sh
hcloud server create --name kfm-build-1 --type cx22 --image debian-12 --location fsn1 \
  --ssh-key me --firewall kfm-build --network kfm-ladder --user-data-from-file build.sh
```

It needs no primary IP of its own (an ephemeral one carries its outbound traffic). runsc runs
on Hetzner's VMs as on GCP's (the systrap platform, no nested virtualisation); `install.sh
--build` checks it: `ssh root@<build ip> docker run --rm debian:trixie-slim dmesg` prints
"Starting gVisor...".

## Switching over

1. Stop the GCP API and workers so nothing writes (`systemctl stop kfm-ladder-api
   'kfm-ladder-match@*'` on the ladder VM, `kfm-ladder-build` on the build VM) and take a last
   dump: `sudo /opt/kfm-ladder-deploy/vm/postgres-backup.sh dump`.
2. Create both Hetzner VMs above with the same env values (same session key, OAuth clients,
   bucket and image tag; a new `POSTGRES_PASSWORD` is fine).
3. Restore the dump on the new ladder VM: `sudo /opt/kfm-ladder-deploy/vm/postgres-backup.sh
   list`, then `restore KEY --yes` (README.md "Backups and restore"). The object store does not
   move: R2 is not on either provider.
4. Lower the ladder record's TTL a day ahead, then point the A record at the new IP; Caddy gets
   a certificate once DNS resolves (`journalctl -u caddy`).
5. Check `https://<ladder name>/healthz`, then delete the GCP VMs and release the address
   (`gcloud compute instances delete`, `gcloud compute addresses delete kfm-ladder-1-ip`).
