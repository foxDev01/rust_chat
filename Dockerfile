# Образ чата: клиент собирается Node, сервер — Rust, запуск — минимальный Debian.
#
#   docker build -t rust-chat .
#   docker run --rm -p 3000:3000 \
#     -e CHAT_DB_URL=postgres://user:pass@host:5432/rust_chat \
#     rust-chat
#
# Для локального запуска вместе с PostgreSQL удобнее docker compose up.

# --- Клиент ---------------------------------------------------------------
FROM node:22-bookworm-slim AS frontend

WORKDIR /app/frontend

# Сначала только манифесты: слой npm ci переиспользуется, пока не изменился
# package-lock.json.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci

COPY frontend/ ./
RUN npm run build

# --- Сервер ---------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /app

# Пустой main.rs позволяет собрать и закэшировать зависимости отдельно от
# исходников: при изменении кода заново компилируется только сам крейт.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked

# Миграции встроены в бинарник через include_str!, поэтому нужны на этапе
# компиляции, но не в runtime-образе.
COPY src ./src
COPY migrations ./migrations

# Docker сохраняет mtime файлов из контекста, а dummy-бинарник собран позже.
# Без touch Cargo считает исходники устаревшими и оставляет пустую заглушку.
RUN find src -type f -exec touch {} + && cargo build --release --locked

# --- Запуск ---------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates — для исходящих TLS-соединений, curl — для HEALTHCHECK.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Сервер не требует root: пишем только в /data/uploads.
RUN useradd --create-home --uid 10001 chat
WORKDIR /app

COPY --from=builder /app/target/release/rust-chat /usr/local/bin/rust-chat
COPY --from=frontend /app/frontend/dist ./frontend/dist

RUN mkdir -p /data/uploads && chown -R chat:chat /data
USER chat

# Адрес и папка вложений по умолчанию рассчитаны на контейнер; строку
# подключения к базе обязательно передайте через CHAT_DB_URL.
ENV CHAT_BIND=0.0.0.0:3000 \
    CHAT_UPLOAD_DIR=/data/uploads

EXPOSE 3000

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=5 \
    CMD curl -fsS http://127.0.0.1:3000/api/health || exit 1

CMD ["rust-chat"]
