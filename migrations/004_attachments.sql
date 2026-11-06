-- Вложения сообщений: имя, тип и размер файла.
--
-- Сам файл лежит на диске (`CHAT_UPLOAD_DIR`) под UUID сообщения, а здесь
-- хранятся только метаданные. Колонки необязательные: у обычных сообщений
-- вложения нет, а старые базы обновляются без потери истории.

ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS attachment_name TEXT;
ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS attachment_mime TEXT;
ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS attachment_size BIGINT;

ALTER TABLE direct_messages ADD COLUMN IF NOT EXISTS attachment_name TEXT;
ALTER TABLE direct_messages ADD COLUMN IF NOT EXISTS attachment_mime TEXT;
ALTER TABLE direct_messages ADD COLUMN IF NOT EXISTS attachment_size BIGINT;
