# font-preview-service

A small Rust web service that serves font previews: the font files and licence
files themselves, the subsets precomputed for a generation, and WOFF2 subsets
built on demand for an arbitrary list of codepoints.

> **Status:** serving. The service reads a *generation* built by its own `sync`
> subcommand; `FONTS_DIR` points at the volume that holds it.

## Image

Published to GitHub Packages as `ghcr.io/storytold/font-preview-service`, with a mirror at
`docker.io/storytold/font-preview-service`.

- Built for `linux/amd64` and `linux/arm64`, each on a native Ubuntu 26.04 runner, then merged into one multi-arch manifest.
- Builds on the [Docker Hardened Images](https://docs.docker.com/dhi/) Rust images (`dhi.io/rust`). The runtime stage runs as a nonroot user (uid 65532).
- A single `ENTRYPOINT` runs the binary. Pass configuration through environment variables, not arguments.

Tags:

| Trigger | Tags |
| --- | --- |
| Push to `main` | `main`, `latest` |
| Push of `v*` tag | semver tags (for example `1.2.3`, `1.2`) |
| Manual run | branch tag |

## How it serves

The binary never reads the font sets baked into the image. It serves whatever
`FONTS_DIR` holds, which is a volume `sync` writes:

```
$FONTS_DIR/
  current -> gen-<index commit>-<n>   symlink, switched atomically
  files/<sha256>                      every font the index lists
  licences/<sha256>                   their licence and notice files
  gen-.../static/<sha256>/<inst>.woff2  precomputed subsets
  gen-.../faces.json                    what the server loads
  gen-.../manifest.json                 how the generation was built
```

The server resolves `current` every 30 seconds and loads the generation behind
it, so a new one appears without a restart. It also re-reads on `SIGHUP`.

## Sync

`sync` mirrors every font and licence the pinned index lists into the
content-addressed store, precomputes the static subsets, and only then switches
`current`:

```sh
FONTS_DIR=/data \
INDEX_COMMIT=995ef2000a0159d70da245e22cfa0797a0756ae8 \
INDEX_SHA256=743cfb4c57176befe8190108efcba43f315de33e747b15ec8694c0625a14c24f \
/font-preview-service sync
```

| Variable | Default | Purpose |
| --- | --- | --- |
| `FONTS_DIR` | required | The store and the generations. Must be writable. |
| `INDEX_REPO` | `aiongg/craft-font-index` | Index repository, `owner/name`. |
| `INDEX_COMMIT` | required | 40 lowercase hex digits. |
| `INDEX_SHA256` | required | sha256 of `sources.json` at that commit, 64 lowercase hex digits. |
| `SYNC_THREADS` | available parallelism, at most 4 | Workers. |
| `SYNC_MEMORY_MB` | `512` | Summed size of the font files being subset at once. |
| `SYNC_MAX_FAILED_PERCENT` | `1` | Above this share of faces whose subset failed, the run keeps `current` and exits non-zero. |
| `SYNC_FORCE` | unset | `1` builds a generation even when the index is unchanged. |

Behaviour worth knowing:

- **The index must be format 1.2.** Format 1.1 has no `samples` table, so every
  `sample` and `var` subset fails with *"the index gives no sample text"* and the
  run refuses to switch `current`. The server reads the extra 1.2 fields through
  its own view; do not pin an older commit to "fix" a failure.
- **A run with nothing to do is cheap.** It reports `unchanged: current -> … stays`
  and exits 0 in about 20 seconds, after fetching `sources.json` and the indexes.
- **Failures keep the old generation.** Fetch failures, or more failed subsets
  than `SYNC_MAX_FAILED_PERCENT`, write `failed.json` and leave `current` where it
  was, so the service keeps serving the previous generation. The sync only exits
  0 once `current` holds this run's index.
- **Old generations are pruned** after a successful switch, but font files are
  content-addressed and shared, so a re-run usually fetches nothing.

On the craft cluster the first run on an empty volume took about four minutes
(5,399 files, 3.9 GB, then subsets for 3,910 faces); later runs are no-ops.

## Configuration

Server variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `PORT` | `8000` | TCP port the server binds on all interfaces. Keep it above 1024 for the nonroot runtime. |
| `METRICS_PORT` | `9100` | Prometheus metrics, never on the API port. |
| `FONTS_DIR` | required | The volume `sync` fills. Read only at runtime. |
| `ONDEMAND_RATE` | `50` | Per-IP token refill per second on on-demand routes; `0` turns the limit off. |
| `ONDEMAND_BURST` | `200` | Per-IP bucket size. |
| `ONDEMAND_CONCURRENCY` | available parallelism | Subsets computed at once. |
| `ONDEMAND_QUEUE` | 4 × concurrency | Requests that may wait for a slot; beyond it, 503. |
| `FACE_CACHE` | `500` | Font files the subsetter keeps in memory. |
| `FACE_CACHE_MB` | `256` | Summed size of those font files. |
| `RESPONSE_CACHE_MB` | `256` | On-demand results. |
| `FILES_BR_CACHE_MB` | `64` | Brotli-compressed file downloads. |
| `FILES_CONCURRENCY` | `64` | File downloads sent at once. |
| `FILES_QUEUE` | 16 × `FILES_CONCURRENCY` | Downloads that may wait up to 2 s for a slot; beyond that, 503 with `Retry-After`. |
| `CLIENT_IP_HEADER` | unset | Header carrying the client address, set by the proxy in front (`CF-Connecting-IP` behind Cloudflare). |

## Endpoints

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/healthz` | `200 ok`. Liveness: it says nothing about whether a generation is loaded. |
| `GET` | `/readyz` | `200 ready`, or `503` while shutting down or before a generation loads. |
| `GET` | `/v1/manifest.json` | Generation name, index pin, face count and limits. |
| `GET` | `/v1/files/{sha256}` | The font file itself. |
| `GET` | `/v1/licences/{sha256}` | The licence or notice file. |
| `GET` | `/v1/faces/{sha256}/{inst}/sample-{hash}.woff2` | Precomputed sample subset; `hash` is the sample text's hash from `faces.json`. |
| `GET` | `/v1/faces/{sha256}/{inst}/ascii.woff2` | Precomputed printable-ASCII subset. |
| `GET` | `/v1/faces/{sha256}/{inst}/u/{codepoints}.woff2` | Subset built on demand. |

`{inst}` is one of:

- `list` — the instance a font list shows, and the one the static subsets are
  precomputed for;
- `var` — the variable-font instance (`list` on a static face);
- a **named instance key**, exactly as the generation's `faces.json` has it
  (`regular`, `wght-400`-style keys are derived by `precompute::naming`, so read
  the key out of `faces.json` rather than guessing).

`{codepoints}` is canonical: sorted lowercase hex, ranges joined with `,` (for
example `20,41-5a,e9`), at most 256 codepoints. The `.woff2` suffix is part of the
URL for `u/` and `sample-`; without it the route answers 404.

## Running

```sh
docker run --rm -p 8000:8000 \
  -v /path/to/fonts:/data \
  docker.io/storytold/font-preview-service:latest
curl http://localhost:8000/readyz
```

Without a volume holding a generation, `/healthz` answers `ok` and every `/v1`
route answers 503.

## Development

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo run                 # listens on :8000; needs FONTS_DIR
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Fonts

The image still carries these font sets, each with its licence files, though the
server no longer reads them: `sync` fetches the same files from the URLs the index
lists. CI fetches them at pinned upstream commits, so the repository does not
store them.

| Source | Pinned commit | Contents | Licence files |
| --- | --- | --- | --- |
| [google/fonts](https://github.com/google/fonts) | `e7db6ca` | `ofl/`, `apache/`, `ufl/` families | Per-family licence files in each directory |
| [notofonts/noto-fonts](https://github.com/notofonts/noto-fonts) | `ffebf8c` | `unhinted/` fonts | `LICENSE` |
| [notofonts/noto-cjk](https://github.com/notofonts/noto-cjk) | `f8d1575` | `Sans/OTF/` fonts | `LICENSE` |

The sets are large — about 4 GB on disk in the image — which is why the image is
large. Google Fonts families are licensed under the SIL Open Font License, Apache
2.0 and the Ubuntu Font License. Keep the licence files with the fonts when
redistributing; `sync` mirrors them into `licences/` for the same reason.

To change a pin, edit the `*_SHA` variables in the "Fetch font sources (pinned)"
step of `.github/workflows/docker-publish.yml`.

## Deployment

The craft cluster runs one replica with `sync` as an init container against a
20Gi `ReadWriteOnce` claim, readiness on `/readyz` and liveness on `/healthz`; see
`apps/production/font-preview-service` in `craft-service-infrastructure`, which
also documents the index pin. Two replicas are not possible while the generation
lives on a `ReadWriteOnce` volume, and the Deployment uses `strategy: Recreate`
for the same reason.

## CI

| Workflow | Runs on | Does |
| --- | --- | --- |
| `docker-publish.yml` | push to `main`, `v*` tags, manual | Fetches fonts, builds both architectures, pushes the multi-arch image to GHCR and Docker Hub. |
| `sonar.yml` | push to `main`, same-repo pull requests | Runs the shared SonarCloud scan from [`storytold/craft-common-actions`](https://github.com/storytold/craft-common-actions). |

Required secrets: `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN` (for pulling Docker Hardened base images from `dhi.io` and pushing to Docker Hub), `SONAR_TOKEN` (or org-wide `SONAR_CLOUD_PAT`), plus the built-in `GITHUB_TOKEN` for GHCR.

## Not yet implemented

- Rendering preview images (raster or SVG) of a family.
- Publishing a generation as a versioned artifact: every runtime builds it with
  `sync` instead.
- Dropping the raw font sets from the image now that the server reads the
  generation instead; they are only dead weight at runtime.
