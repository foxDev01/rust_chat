<script setup lang="ts">
// Одно сообщение в ленте: аватар, автор, время и текст.
import { computed } from 'vue'
import type { ChatItem } from '../types'

const props = defineProps<{
  message: ChatItem
  /** Сообщение отправлено текущим пользователем. */
  own: boolean
  /** Сообщение личное, а не из общего канала. */
  direct?: boolean
}>()

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
      <p class="bubble">{{ message.text }}</p>
    </div>
  </article>
</template>
