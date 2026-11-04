-- Схема хранения чата. Применяется сервером при каждом старте, все команды идемпотентны.
-- seq — монотонный номер вставки: по нему восстанавливают порядок сообщений.

CREATE TABLE IF NOT EXISTS chat_messages (
    seq     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id      UUID NOT NULL UNIQUE,
    author  TEXT NOT NULL,
    body    TEXT NOT NULL,
    sent_at TIMESTAMPTZ NOT NULL
);

-- Личные сообщения. *_key — имена, приведённые к нижнему регистру на стороне приложения
-- (той же функцией, что и поиск онлайн-участников), чтобы диалог не зависел от регистра.
CREATE TABLE IF NOT EXISTS direct_messages (
    seq           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id            UUID NOT NULL UNIQUE,
    sender        TEXT NOT NULL,
    recipient     TEXT NOT NULL,
    sender_key    TEXT NOT NULL,
    recipient_key TEXT NOT NULL,
    body          TEXT NOT NULL,
    sent_at       TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS direct_messages_sender_key_idx
    ON direct_messages (sender_key, seq);
CREATE INDEX IF NOT EXISTS direct_messages_recipient_key_idx
    ON direct_messages (recipient_key, seq);

-- Сколько сообщений диалога пользователь уже прочитал.
CREATE TABLE IF NOT EXISTS direct_reads (
    user_key TEXT NOT NULL,
    peer_key TEXT NOT NULL,
    last_seq BIGINT NOT NULL,
    read_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_key, peer_key)
);
