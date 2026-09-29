# syntax=docker/dockerfile:1

# ---- 构建阶段 ----
FROM rust:1.98-slim-bookworm AS builder
WORKDIR /app

# 仅复制清单先构建依赖骨架？工作区多 crate 相互依赖，直接整仓构建；
# 层缓存命中时（仅代码改动）cargo 的增量复用仍在。
COPY . .
# 镜像里只需 cargo/rustc（基础镜像已内置 1.98.0）；rust-toolchain.toml 额外
# 要求 rustfmt/clippy 会触发 rustup 联网同步，在内网/弱网下长时间卡住，
# 构建阶段直接移除工具链文件，使用基础镜像自带的同版本工具链。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rm -f rust-toolchain.toml \
    && cargo build --release -p hotpot-api --bin hotpot-server \
    && cp target/release/hotpot-server /tmp/hotpot-server

# ---- 运行阶段：docker executor 模式只需要 daemon socket 与 ca-certificates ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /tmp/hotpot-server /usr/local/bin/hotpot-server

EXPOSE 7878
VOLUME ["/data"]

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s \
    CMD curl -fsS http://127.0.0.1:7878/healthz || exit 1

ENTRYPOINT ["hotpot-server"]
CMD ["--listen", "0.0.0.0:7878", "--data-dir", "/data"]
