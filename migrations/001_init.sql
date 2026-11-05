-- Базовая схема хранения чата. Применяется сервером при каждом старте,
-- все команды идемпотентны.
--
-- seq — монотонный номер вставки: по нему восстанавливают порядок сообщений.
-- Начиная с миграции 002 сообщения ссылаются на аккаунты по UUID, а не по имени,
-- поэтому переименование пользователя не «потеряет» его историю.

-- Аккаунты: имя для показа, ключ для поиска без учёта регистра и хеш пароля (Argon2id).
CREATE TABLE IF NOT EXISTS accounts (
    id            UUID PRIMARY KEY,
    username      TEXT NOT NULL,
    username_key  TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Сессии входа. В базе лежит только SHA-256 от токена: утечка таблицы не даёт
-- готовых токенов, а истёкшие сессии отсекаются при проверке.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_account_id_idx
    ON sessions (account_id);

-- Публичная история общего канала.
CREATE TABLE IF NOT EXISTS chat_messages (
    seq       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id        UUID NOT NULL UNIQUE,
    author_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    body      TEXT NOT NULL,
    sent_at   TIMESTAMPTZ NOT NULL
);

-- Личные сообщения. Пара sender_id/recipient_id однозначно задаёт диалог,
-- даже если один из участников сменит имя.
CREATE TABLE IF NOT EXISTS direct_messages (
    seq          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id           UUID NOT NULL UNIQUE,
    sender_id    UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    recipient_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    body         TEXT NOT NULL,
    sent_at      TIMESTAMPTZ NOT NULL
);

-- Сколько сообщений диалога пользователь уже прочитал.
CREATE TABLE IF NOT EXISTS direct_reads (
    user_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    peer_id  UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    last_seq BIGINT NOT NULL,
    read_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, peer_id)
);
