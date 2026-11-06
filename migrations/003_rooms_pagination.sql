-- Комнаты общего чата и привязка сохранённых сообщений к комнате.
--
-- Файл применяется при каждом старте после 002 и должен быть идемпотентным:
-- на новой базе таблица уже создана в 001, а на старой сообщения получают
-- комнату `general`, чтобы ничего не потерялось при обновлении.

CREATE TABLE IF NOT EXISTS rooms (
    id       TEXT PRIMARY KEY,
    title    TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0
);

-- Колонка появилась вместе с комнатами; на самых старых базах её может не быть.
ALTER TABLE rooms ADD COLUMN IF NOT EXISTS position INTEGER NOT NULL DEFAULT 0;

-- Стартовый набор комнат нужен уже здесь: к `general` привязываются старые
-- сообщения. Названия и порядок при запуске синхронизирует сервер
-- (`DEFAULT_ROOMS` в src/protocol.rs).
INSERT INTO rooms (id, title, position) VALUES
    ('general', 'Общий', 0),
    ('rust', 'Rust', 1),
    ('random', 'Курилка', 2)
ON CONFLICT (id) DO NOTHING;

ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS room_id TEXT;

-- Вся публичная история до появления комнат попадает в общую комнату.
UPDATE chat_messages
SET room_id = 'general'
WHERE room_id IS NULL;

ALTER TABLE chat_messages ALTER COLUMN room_id SET DEFAULT 'general';
ALTER TABLE chat_messages ALTER COLUMN room_id SET NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'chat_messages_room_id_fkey'
          AND conrelid = 'chat_messages'::regclass
    ) THEN
        ALTER TABLE chat_messages
            ADD CONSTRAINT chat_messages_room_id_fkey
            FOREIGN KEY (room_id) REFERENCES rooms(id) ON DELETE CASCADE;
    END IF;
END
$$;

-- Пагинация читает сообщения комнаты по убыванию seq.
CREATE INDEX IF NOT EXISTS chat_messages_room_seq_idx
    ON chat_messages (room_id, seq);
