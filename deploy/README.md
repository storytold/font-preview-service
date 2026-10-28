# Deploying to a Flatcar host

`deploy.sh` runs from any Linux machine with `ssh`, `tar`, `curl` and `docker`. The host needs Flatcar Container Linux, a user with passwordless sudo, and a DNS name proxied by Cloudflare.

```sh
deploy/deploy.sh digest docker.io/dylanmtaylorartcraft/font-preview-service:latest   # → image@sha256:…
deploy/deploy.sh setup    <ssh-dest> <domain> <image@sha256:…>   # first install; re-run to apply changed files
deploy/deploy.sh deploy   <ssh-dest> <image@sha256:…>            # new image; rolls back if unhealthy
deploy/deploy.sh rollback <ssh-dest>
deploy/deploy.sh status   <ssh-dest>
deploy/deploy.sh ignition <domain> <image@sha256:…> host.ign     # Ignition config for a new host
deploy/deploy.sh load     <ssh-dest> <local-image-ref>           # sideload an image, prints sha256:<id>
deploy/deploy.sh index    <ssh-dest> <commit> <sources-sha256>   # pin the font index, then sync
deploy/deploy.sh sync     <ssh-dest>                             # sync now, follow the journal
```

## What runs on the host

Minimum host RAM: 2 GB, no swap. Memory limits: service 1200 MB, proxy 200 MB, sync 640 MB (2040 MB if all were at their ceilings at once). A full sync at one thread peaks near 280 MB, and the sync container has OOM score 500, so under memory pressure the kernel kills it before the server or the proxy. A larger host gives headroom; raise `SYNC_CONTAINER_MEMORY` and `SYNC_THREADS` together only with more RAM.

| Unit | Does |
|---|---|
| `fps.service` | The service container on the internal `fps` network. Nonroot, read-only root, no capabilities, `--init`, 1.2 GB memory, 1 CPU, 256 pids. |
| `fps-sync.service`, `fps-sync.timer` | One-shot run of the same image with `sync`: fills the fonts volume, then sends SIGHUP to `fps`. The timer runs it daily (random delay up to 1 h) and 3 minutes after boot. Same hardening as `fps`; 640 MB memory (`SYNC_CONTAINER_MEMORY` in `sync.env`; empty means the default), OOM score 500, 1 CPU, `SYNC_THREADS=1`. Skipped while no index is pinned. |
| `fps-proxy.service` | Caddy on host ports 80/443 (IPv4; 8080/8443 inside, nonroot). Certificates from Let's Encrypt, kept in `/var/lib/font-preview/caddy`. Port 80 answers ACME challenges and redirects the rest. |
| `iptables-restore`, `ip6tables-restore` | Incoming: SSH, ICMP, and 80/443 from Cloudflare's address ranges only. IPv6 forwarding to containers is dropped. |
| `fps-cloudflare.timer` | Daily: fetches Cloudflare's ranges; when they changed, replaces the firewall's `DOCKER-USER` chain and Caddy's trusted proxies. A failed fetch keeps the current set. |

- Images are pinned by digest in `/etc/font-preview/release.env`; the previous one is kept in `release.env.prev`. Caddy's and Butane's digests are in `versions.env`. Units pull an image only when it is missing, so a registry outage doesn't keep the service down after a reboot.
- `deploy` pulls the new image before changing anything, rolls back when the restart or the health check fails, and afterwards removes images other than the current and previous ones.
- Container logs go to journald: `journalctl -u fps -u fps-proxy`, capped at 500 MB.
- Flatcar updates itself on the LTS channel and reboots only between 04:00 and 05:00 UTC. Everything is enabled, so it comes back after a reboot (about 35 s on a 1-vCPU host).

## Fonts: sync and the volume

The fonts live in `/var/lib/font-preview/fonts` (owner 65532, the container user), mounted at `/fonts` and `FONTS_DIR=/fonts`: read-write in `fps-sync`, read-only in `fps`. `sync` stores files content-addressed, builds a generation `gen-<commit>-<n>/`, and switches the `current` symlink atomically. A failed sync leaves `current` unchanged. About 4 GB; the sync container needs outbound HTTPS only (jsDelivr, raw.githubusercontent.com, github.com), and the firewall does not restrict outbound traffic.

`/etc/font-preview/sync.env` holds `INDEX_REPO`, `INDEX_COMMIT`, `INDEX_SHA256` and optional tuning (`SYNC_THREADS`, `SYNC_MEMORY_MB`, `SYNC_MAX_FAILED_PERCENT`, `SYNC_FORCE`, `SYNC_CONTAINER_MEMORY`). `setup` creates it when missing and never overwrites it. When it has no pin, `setup` and `ignition` write the `INDEX_COMMIT` and `INDEX_SHA256` from `versions.env`, so a rebuilt host comes up ready.

`deploy` waits for `/healthz` (liveness) and then reports `/readyz`. A first deploy with an empty volume stays not ready until the first sync.

Pin a new index commit: edit `INDEX_COMMIT` and `INDEX_SHA256` in `versions.env` (for new hosts), then update live hosts:

```sh
deploy/deploy.sh index <ssh-dest> <40-hex commit> <sha256 of that commit's sources.json>
```

This rewrites the two `INDEX_*` lines, starts a sync and follows its journal. `deploy.sh sync <ssh-dest>` repeats a sync with the pinned index. Both start a new run of the unit (waiting first for one that is already running, such as the timer's) and poll with short SSH calls, so a dropped connection doesn't end the wait. A sync that does not succeed (fetch failures, a refused switch, an I/O error, another sync holding the lock) prints "sync failed, current unchanged (see journal)"; read the cause with `journalctl -u fps-sync` on the host. Both commands need `setup` to have run with this version first. `status` shows the current generation and the last sync result.

## Test builds without a registry

Sideload a locally built image instead of pushing it:

```sh
id=$(deploy/deploy.sh load <ssh-dest> <local-image-ref>)   # docker save | gzip | ssh … docker load; prints sha256:<id>
ALLOW_SIDELOAD=1 deploy/deploy.sh deploy <ssh-dest> "$id"  # or: ALLOW_SIDELOAD=1 … setup <ssh-dest> <domain> "$id"
```

A bare image ID is for test hosts only and needs `ALLOW_SIDELOAD=1` on the command line; `setup` with that variable also creates `/etc/font-preview/test-host` on the host, and `deploy` refuses an ID on a host without it (no variable: `setup` and `deploy` refuse every ID). `status` prints "sideloaded image" while an ID is running. Example: `ALLOW_SIDELOAD=1 deploy/deploy.sh setup <ssh-dest> <domain> "$id"`. With the variable set, `setup` and `deploy` accept a bare image ID in place of `name@sha256:<digest>`; they skip the pull and stop before changing anything if the image is not on the host. Rollback and image pruning treat both forms alike. Production uses registry digests; `ignition` takes digests only.

## Host-specific values

None are stored here. The SSH destination and domain are arguments; Cloudflare's ranges are fetched at `setup` and refreshed daily on the host.

## Cloudflare settings (on the zone's account)

1. SSL/TLS → Overview: encryption mode **Full (strict)**. The origin has a Let's Encrypt certificate, so strict validation works.
2. SSL/TLS → Origin Server: turn on **Authenticated Origin Pulls** (zone-level, Cloudflare's certificate).
3. Then re-run setup with origin pulls required, so only Cloudflare's origin-pull certificate is accepted on 443:

   ```sh
   ORIGIN_PULL=require deploy/deploy.sh setup <ssh-dest> <domain> <image@sha256:…>
   ```

For production, use Authenticated Origin Pulls with a per-zone certificate (the zone owner creates it and uploads it to Cloudflare; the origin trusts only that CA): Cloudflare's shared certificate proves only that a request came through Cloudflare, not through this zone.

Until step 3, Caddy verifies the client certificate when Cloudflare sends one and accepts connections without one, so the service keeps working whichever order the steps happen in.

## Container contract (Flatcar now, Kubernetes later)

The image runs the same way under these systemd units and under Kubernetes; the units here are one way to run it.

- Configuration comes only from environment variables. Numeric nonroot UID (65532), read-only root filesystem, logs to stdout/stderr.
- `font-preview-service` serves; `font-preview-service sync` fills the fonts volume and exits (a timer here; a CronJob, Job or initContainer elsewhere).
- The server is stateless apart from the fonts volume, which it mounts read-only, so several replicas can share one volume (ReadOnlyMany). Only `sync` writes to it, into a new generation directory, and switches the `current` symlink atomically (rename).
- Disk use peaks at the blobs (about 4 GB) plus up to 4 generations of about 85 MB each (about 340 MB): the one being built, `current`, and the two kept before it.
- The server notices a new generation itself by polling `current`; `SIGHUP` reloads at once.
- `/healthz` is liveness; `/readyz` fails until a manifest is loaded. `SIGTERM` drains open requests, then exits.
- Metrics are on their own port, not proxied.

## Building the image

The Dockerfile takes the base images as build arguments:

| Argument | Default | Notes |
|---|---|---|
| `BUILD_IMAGE` | `rust:1-bookworm` | Needs g++: the build compiles HarfBuzz (C++). |
| `RUNTIME_IMAGE` | `dhi.io/rust:1` | Needs glibc at least as new as the build image's (Debian 12 here) and libgcc_s. `gcr.io/distroless/cc-debian12:nonroot` works for test builds. |

```sh
docker build --build-arg RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot -t font-preview-service:test .
```
