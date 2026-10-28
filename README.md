# font-preview-service

A small Rust web service that will render font previews. It currently exposes a health endpoint and ships as a container image with the full Google Fonts and Noto font sets on disk.

> **Status:** early scaffold. The only endpoint is `GET /healthz`. The font sets are packaged into the image, but the service does not read or render them yet.

## Image

Published to Docker Hub as `dylanmtaylorartcraft/font-preview-service`.

- Built for `linux/amd64` and `linux/arm64`, each on a native Ubuntu 26.04 runner, then merged into one multi-arch manifest.
- Builds on the [Docker Hardened Images](https://docs.docker.com/dhi/) Rust images (`dhi.io/rust`). The runtime stage runs as a nonroot user.
- A single `ENTRYPOINT` runs the binary. Pass configuration through environment variables, not arguments.

Tags:

| Trigger | Tags |
| --- | --- |
| Push to `main` | `main`, `latest` |
| Push of `v*` tag | semver tags (for example `1.2.3`, `1.2`) |
| Manual run | branch tag |

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `PORT` | `8000` | TCP port the server binds on all interfaces. Keep it above 1024 for the nonroot runtime. |
| `FONTS_DIR` | `/opt/fonts` (image) | Location of the font sets inside the image. Set by the Dockerfile; the app does not read it yet. |
| `FILES_CONCURRENCY` | `64` | File downloads (`/v1/files/...`) sent at once. Bounds the memory that downloads hold. |
| `FILES_QUEUE` | 16 × `FILES_CONCURRENCY` | Downloads that may wait up to 2 s for a slot; beyond that the server answers 503 with `Retry-After`. |

## Running

```sh
docker run --rm -p 8000:8000 dylanmtaylorartcraft/font-preview-service:latest
curl http://localhost:8000/healthz   # ok
```

## Endpoints

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/healthz` | `200 ok` |

## Development

Requires a recent stable Rust toolchain (edition 2024).

```sh
cargo run                 # listens on :8000
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Fonts

The image includes these font sets, each with its licence files. CI fetches them at pinned upstream commits, so the repository does not store them.

| Source | Pinned commit | Contents | Licence files |
| --- | --- | --- | --- |
| [google/fonts](https://github.com/google/fonts) | `e7db6ca` | `ofl/`, `apache/`, `ufl/` families | Per-family licence files in each directory |
| [notofonts/noto-fonts](https://github.com/notofonts/noto-fonts) | `ffebf8c` | `unhinted/` fonts | `LICENSE` |
| [notofonts/noto-cjk](https://github.com/notofonts/noto-cjk) | `f8d1575` | `Sans/OTF/` fonts | `LICENSE` |

The sets are large: about 7.3 GB of font files in total, which is why the image is large. Google Fonts families are licensed under the SIL Open Font License, Apache 2.0 and the Ubuntu Font License. Keep the licence files with the fonts when redistributing.

To change a pin, edit the `*_SHA` variables in the "Fetch font sources (pinned)" step of `.github/workflows/docker-publish.yml`.

## CI

| Workflow | Runs on | Does |
| --- | --- | --- |
| `docker-publish.yml` | push to `main`, `v*` tags, manual | Fetches fonts, builds both architectures, pushes the multi-arch image. |
| `sonar.yml` | push to `main`, same-repo pull requests | Runs the shared SonarCloud scan from [`storytold/craft-common-actions`](https://github.com/storytold/craft-common-actions). |

Required repository secrets: `DOCKERHUB_USERNAME`, `DOCKERHUB_TOKEN`, `SONAR_TOKEN`.

## Not yet implemented

- Reading the font files from `FONTS_DIR`.
- Rendering preview images or any font-related endpoints.
- Subsetting. The idea is to render only the glyphs needed for each preview string; this is not built.
