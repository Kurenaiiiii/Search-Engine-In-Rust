# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock .cargo/ rustfmt.toml ./
COPY se-core/ se-core/
COPY se-server/ se-server/
COPY se-cli/ se-cli/
ENV CARGO_BUILD_JOBS=2
RUN cargo build --release -p se-server -p se-cli \
  && rm -rf target/release/build target/release/deps target/release/.fingerprint

FROM debian:bookworm-slim
RUN useradd -r -u 10001 -d /data -s /usr/sbin/nologin seuser \
  && mkdir -p /data && chown seuser:seuser /data
COPY --from=builder /app/target/release/se-server /usr/local/bin/se-server
COPY --from=builder /app/target/release/se-cli /usr/local/bin/se-cli
USER seuser
ENV SE_ADDR=0.0.0.0:3000 SE_DATA=/data
EXPOSE 3000
VOLUME /data
SHELL ["/bin/bash", "-c"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD printf 'GET /health HTTP/1.0\r\n\r\n' > /dev/tcp/127.0.0.1/3000 | grep -q '"ok":true'
CMD ["se-server"]
