# syntax=docker/dockerfile:1

# The build stage compiles HarfBuzz (C++), so its image needs g++. The binary links only
# glibc and libgcc_s; the runtime image must have a glibc at least as new as the build image's.
ARG BUILD_IMAGE=rust:1-bookworm
ARG RUNTIME_IMAGE=dhi.io/rust:1

################################################################################
# Build stage: Rust toolchain with g++.
FROM ${BUILD_IMAGE} AS build
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
RUN cargo build --locked --release && \
    cp target/release/font-preview-service /build/font-preview-service

################################################################################
# Runtime stage: Docker Hardened Rust runtime image by default, runs as nonroot.
FROM ${RUNTIME_IMAGE} AS final
WORKDIR /app
COPY --from=build /build/font-preview-service /app/font-preview-service

# Full font sets, fetched by CI at pinned upstream commits (see docker-publish.yml).
# Each source keeps its own licence files alongside the fonts.
COPY fonts-src/google/ofl /opt/fonts/google/ofl
COPY fonts-src/google/apache /opt/fonts/google/apache
COPY fonts-src/google/ufl /opt/fonts/google/ufl
COPY fonts-src/noto/unhinted /opt/fonts/noto/unhinted
COPY fonts-src/noto/LICENSE /opt/fonts/noto/LICENSE
COPY fonts-src/noto-cjk/Sans/OTF /opt/fonts/noto-cjk/Sans/OTF
COPY fonts-src/noto-cjk/Sans/LICENSE /opt/fonts/noto-cjk/LICENSE

ENV PORT=8000
ENV FONTS_DIR=/opt/fonts
EXPOSE 8000

ENTRYPOINT ["/app/font-preview-service"]
