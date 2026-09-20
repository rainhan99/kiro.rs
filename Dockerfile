FROM oven/bun:1-alpine AS frontend-builder

WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/bun.lock* ./
RUN bun install --frozen-lockfile --ignore-scripts
COPY admin-ui ./
RUN bun run build

FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev perl make

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# -p kiro-rs：仓库自 F-001 起是 workspace，desktop/src-tauri 也是成员。
# 镜像里只要代理二进制，显式点名避免将来有人加 --workspace 时把 Tauri 也拖进来。
RUN cargo build --release --no-default-features -p kiro-rs

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/target/release/kiro-rs /app/kiro-rs

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
