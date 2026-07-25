# mini-term 中转服务器镜像:Rust 中转 + 内置移动端 PWA 静态资源。
# 构建上下文为仓库根目录(需要 relay-server/ 与 mobile/ 两个目录):
#   docker build -f relay-server/Dockerfile -t mini-term-relay .
# 中转纪律:消息体仅内存转发不落盘——镜像内不声明任何持久化卷。

# ── 阶段 1:构建移动端 PWA ──
FROM node:22-alpine AS pwa
WORKDIR /app
COPY mobile/package.json mobile/package-lock.json ./
RUN npm ci
COPY mobile/ ./
RUN npm run build

# ── 阶段 2:构建中转服务 ──
FROM rust:1-bookworm AS relay
WORKDIR /build
COPY relay-server/ ./
RUN cargo build --release -p mt-relay-server

# ── 阶段 3:运行时(最小化,非 root) ──
FROM debian:bookworm-slim
RUN useradd --system --no-create-home relay
COPY --from=relay /build/target/release/mt-relay-server /usr/local/bin/mt-relay-server
COPY --from=pwa /app/dist /srv/pwa

ENV RELAY_BIND=0.0.0.0 \
    RELAY_PORT=8080 \
    RELAY_PWA_DIR=/srv/pwa

USER relay
EXPOSE 8080
CMD ["mt-relay-server"]
