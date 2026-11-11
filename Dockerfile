# Both stages are pinned; the final image contains no Python runtime or server source.
FROM rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ src/
COPY app/bundled/ app/bundled/
COPY app/static/ app/static/
COPY app/store/migrations/ app/store/migrations/
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked --bin rich-studio \
    && cp target/release/rich-studio /build/rich-studio

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin studio \
    && mkdir -p /data /opt/rich-studio \
    && chown 10001:10001 /data
COPY --from=build /build/rich-studio /usr/local/bin/rich-studio
COPY app/static/ /opt/rich-studio/static/
RUN chmod -R a+rX /opt/rich-studio
ENV DATA_DIR=/data STUDIO_BIND=0.0.0.0:8080 STUDIO_STATIC_DIR=/opt/rich-studio/static
WORKDIR /opt/rich-studio
USER 10001:10001
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=60s --timeout=6s --start-period=10s CMD ["rich-studio", "healthcheck"]
CMD ["rich-studio", "serve"]
