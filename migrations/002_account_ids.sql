-- Переход от имён к идентификаторам аккаунтов в таблицах сообщений и отметок
-- о прочтении.
--
-- Файл применяется при каждом старте после 001 и должен быть идемпотентным:
-- перенос данных выполняется только пока существуют старые колонки с именами,
-- а новые колонки, индексы и ограничения создаются лишь при их отсутствии.
-- На пустой базе 001 уже создаёт схему с UUID, поэтому весь перенос пропускается.

-- ---------- Публичные сообщения ----------

ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS author_id UUID;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'chat_messages'
          AND column_name = 'author'
    ) THEN
        -- Авторство переносим по ключу имени без учёта регистра: username_key
        -- для аккаунта и lower(btrim(author)) для старой строки.
        EXECUTE $migrate$
            UPDATE chat_messages m
            SET author_id = a.id
            FROM accounts a
            WHERE m.author_id IS NULL
              AND a.username_key = lower(btrim(m.author))
        $migrate$;

        -- Сообщения, отправленные до появления аккаунтов, привязать не к кому:
        -- без автора восстановить их нельзя, поэтому такие строки удаляются.
        EXECUTE 'DELETE FROM chat_messages WHERE author_id IS NULL';
    END IF;
END
$$;

ALTER TABLE chat_messages ALTER COLUMN author_id SET NOT NULL;
ALTER TABLE chat_messages DROP COLUMN IF EXISTS author;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'chat_messages_author_id_fkey'
          AND conrelid = 'chat_messages'::regclass
    ) THEN
        ALTER TABLE chat_messages
            ADD CONSTRAINT chat_messages_author_id_fkey
            FOREIGN KEY (author_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS chat_messages_author_id_idx
    ON chat_messages (author_id, seq);

-- ---------- Личные сообщения ----------

ALTER TABLE direct_messages ADD COLUMN IF NOT EXISTS sender_id UUID;
ALTER TABLE direct_messages ADD COLUMN IF NOT EXISTS recipient_id UUID;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'direct_messages'
          AND column_name = 'sender_key'
    ) THEN
        -- sender_key/recipient_key уже приведены приложением к нижнему регистру,
        -- поэтому сравниваем их с username_key напрямую.
        EXECUTE $migrate$
            UPDATE direct_messages m
            SET sender_id = a.id
            FROM accounts a
            WHERE m.sender_id IS NULL
              AND a.username_key = m.sender_key
        $migrate$;

        EXECUTE $migrate$
            UPDATE direct_messages m
            SET recipient_id = a.id
            FROM accounts a
            WHERE m.recipient_id IS NULL
              AND a.username_key = m.recipient_key
        $migrate$;

        -- Как и с публичной историей: диалог без известного участника
        -- восстановить нельзя.
        EXECUTE $migrate$
            DELETE FROM direct_messages
            WHERE sender_id IS NULL OR recipient_id IS NULL
        $migrate$;
    END IF;
END
$$;

ALTER TABLE direct_messages ALTER COLUMN sender_id SET NOT NULL;
ALTER TABLE direct_messages ALTER COLUMN recipient_id SET NOT NULL;

-- Старые колонки с именами и их индексы больше не нужны.
ALTER TABLE direct_messages DROP COLUMN IF EXISTS sender;
ALTER TABLE direct_messages DROP COLUMN IF EXISTS recipient;
ALTER TABLE direct_messages DROP COLUMN IF EXISTS sender_key;
ALTER TABLE direct_messages DROP COLUMN IF EXISTS recipient_key;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'direct_messages_sender_id_fkey'
          AND conrelid = 'direct_messages'::regclass
    ) THEN
        ALTER TABLE direct_messages
            ADD CONSTRAINT direct_messages_sender_id_fkey
            FOREIGN KEY (sender_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'direct_messages_recipient_id_fkey'
          AND conrelid = 'direct_messages'::regclass
    ) THEN
        ALTER TABLE direct_messages
            ADD CONSTRAINT direct_messages_recipient_id_fkey
            FOREIGN KEY (recipient_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS direct_messages_sender_id_idx
    ON direct_messages (sender_id, seq);
CREATE INDEX IF NOT EXISTS direct_messages_recipient_id_idx
    ON direct_messages (recipient_id, seq);

-- ---------- Отметки о прочтении ----------

ALTER TABLE direct_reads ADD COLUMN IF NOT EXISTS user_id UUID;
ALTER TABLE direct_reads ADD COLUMN IF NOT EXISTS peer_id UUID;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'direct_reads'
          AND column_name = 'user_key'
    ) THEN
        EXECUTE $migrate$
            UPDATE direct_reads r
            SET user_id = a.id
            FROM accounts a
            WHERE r.user_id IS NULL
              AND a.username_key = r.user_key
        $migrate$;

        EXECUTE $migrate$
            UPDATE direct_reads r
            SET peer_id = a.id
            FROM accounts a
            WHERE r.peer_id IS NULL
              AND a.username_key = r.peer_key
        $migrate$;

        EXECUTE 'DELETE FROM direct_reads WHERE user_id IS NULL OR peer_id IS NULL';
    END IF;
END
$$;

ALTER TABLE direct_reads ALTER COLUMN user_id SET NOT NULL;
ALTER TABLE direct_reads ALTER COLUMN peer_id SET NOT NULL;

-- Удаление старых колонок автоматически снимает старый первичный ключ
-- (user_key, peer_key); на его место встаёт ключ по UUID.
ALTER TABLE direct_reads DROP COLUMN IF EXISTS user_key;
ALTER TABLE direct_reads DROP COLUMN IF EXISTS peer_key;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'direct_reads_pkey'
          AND conrelid = 'direct_reads'::regclass
    ) THEN
        ALTER TABLE direct_reads ADD PRIMARY KEY (user_id, peer_id);
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'direct_reads_user_id_fkey'
          AND conrelid = 'direct_reads'::regclass
    ) THEN
        ALTER TABLE direct_reads
            ADD CONSTRAINT direct_reads_user_id_fkey
            FOREIGN KEY (user_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'direct_reads_peer_id_fkey'
          AND conrelid = 'direct_reads'::regclass
    ) THEN
        ALTER TABLE direct_reads
            ADD CONSTRAINT direct_reads_peer_id_fkey
            FOREIGN KEY (peer_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;
END
$$;
