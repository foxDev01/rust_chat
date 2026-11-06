<script setup lang="ts">
// Одно сообщение в ленте: аватар, автор, время, текст и вложение.
import { computed, onMounted, ref, watch } from 'vue'
import type { Attachment, ChatItem } from '../types'

const props = defineProps<{
  message: ChatItem
  /** Сообщение отправлено текущим пользователем. */
  own: boolean
  /** Сообщение личное, а не из общего канала. */
  direct?: boolean
  /** Готовит blob-ссылку файла: качает байты с токеном сессии. */
  loadAttachment: (id: string) => Promise<string>
  /** Скачивает файл под исходным именем. */
  downloadAttachment: (id: string, name: string) => void
}>()

/** Массивы данных для картинок: SVG показывать не рискуем. */
const IMAGE_MIMES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']

// Время в формате «ЧЧ:ММ».
const timeFormatter = new Intl.DateTimeFormat('ru-RU', {
  hour: '2-digit',
  minute: '2-digit',
})

// Первая буква имени для аватара.
const initial = computed(() => [...props.message.author][0]?.toUpperCase() ?? '?')

// Цвет аватара зависит от имени, поэтому у одного автора он всегда одинаковый.
const hue = computed(() => {
  let hash = 0
  for (const char of props.message.author) {
    hash = (hash * 31 + (char.codePointAt(0) ?? 0)) % 360
  }
  return hash
})

/** Вложение сообщения, если оно есть. */
const attachment = computed<Attachment | null>(() => props.message.attachment ?? null)

/** Показывать ли картинку вместо карточки файла. */
const isImage = computed(() => {
  const mime = attachment.value?.mime.toLowerCase() ?? ''
  return IMAGE_MIMES.some((known) => mime === known || mime.startsWith(`${known};`))
})

/** Blob-ссылка на превью; пустая, пока файл не скачан. */
const previewUrl = ref('')

/** Превью не загрузилось: показываем обычную карточку файла. */
const previewFailed = ref(false)

/** Загружает превью для сообщения с картинкой. */
async function loadPreview() {
  previewUrl.value = ''
  previewFailed.value = false

  if (!isImage.value || !attachment.value) {
    return
  }

  try {
    previewUrl.value = await props.loadAttachment(props.message.id)
  } catch {
    previewFailed.value = true
  }
}

// Превью нужно один раз: идентификатор сообщения внутри одного элемента не меняется.
onMounted(loadPreview)
watch(() => props.message.id, loadPreview)

/** Размер файла в понятных единицах. */
function formatSize(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} Б`
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} КБ`
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} МБ`
}

/** Форматирует время; при ошибке разбора возвращает пустую строку. */
function formatTime(value: string): string {
  const date = new Date(value.replace(/\.(\d{3})\d+/, '.$1'))
  return Number.isNaN(date.getTime()) ? '' : timeFormatter.format(date)
}
</script>

<template>
  <!-- Свои сообщения подсвечиваются классом `message--own`. -->
  <article class="message" :class="{ 'message--own': own }">
    <span
      class="avatar"
      :style="{ '--avatar-hue': hue }"
      :title="message.author"
      aria-hidden="true"
    >
      {{ initial }}
    </span>

    <div class="message-body">
      <header class="message-head">
        <span class="message-author">{{ own ? 'Вы' : message.author }}</span>
        <span v-if="direct" class="message-scope" title="Личное сообщение">лично</span>
        <time class="message-time" :datetime="message.sentAt">{{ formatTime(message.sentAt) }}</time>
      </header>

      <!-- Текст может отсутствовать: файл отправлен без комментария. -->
      <p v-if="message.text" class="bubble">{{ message.text }}</p>

      <figure v-if="attachment" class="attachment">
        <!-- Картинки показываем превью, остальное — карточкой с именем. -->
        <button
          v-if="isImage && previewUrl && !previewFailed"
          class="attachment-image"
          type="button"
          :title="`Скачать ${attachment.name}`"
          @click="downloadAttachment(attachment.id, attachment.name)"
        >
          <img
            :src="previewUrl"
            :alt="attachment.name"
            @error="previewFailed = true"
          />
        </button>

        <a
          v-else-if="!isImage || !previewUrl || previewFailed"
          class="attachment-file"
          href="#"
          role="button"
          :title="`Скачать ${attachment.name}`"
          @click.prevent="downloadAttachment(attachment.id, attachment.name)"
        >
          <span class="attachment-icon" aria-hidden="true">📄</span>
          <span class="attachment-meta">
            <span class="attachment-name">{{ attachment.name }}</span>
            <span class="attachment-size">{{ formatSize(attachment.size) }}</span>
          </span>
        </a>

        <!-- Под превью остаётся строка с именем и размером. -->
        <figcaption v-if="isImage && previewUrl && !previewFailed" class="attachment-caption">
          {{ attachment.name }} · {{ formatSize(attachment.size) }}
        </figcaption>
      </figure>
    </div>
  </article>
</template>
