<script setup lang="ts">
// Экран чата: боковая панель с диалогами и участниками, лента и поле ввода.
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { type ConnectionStatus } from '../composables/useChat'
import type { ConversationSummary, TimelineItem } from '../types'
import MessageBubble from './MessageBubble.vue'

// Компонент не хранит состояние чата — только получает его через props.
const props = defineProps<{
  me: string
  users: string[]
  timeline: TimelineItem[]
  conversations: ConversationSummary[]
  activePeer: string | null
  status: ConnectionStatus
  error: string
}>()

// События наружу: отправка сообщения, выбор диалога, выход и переподключение.
const emit = defineEmits<{
  send: [text: string]
  openConversation: [peer: string | null]
  leave: []
  reconnect: []
  dismissError: []
}>()

// Подписи для индикатора состояния соединения.
const STATUS_LABELS: Record<ConnectionStatus, string> = {
  idle: 'Не подключено',
  connecting: 'Подключаемся…',
  online: 'В сети',
  offline: 'Нет связи',
}

// Локальное состояние интерфейса: черновик сообщения, прокрутка и мобильное меню.
const draft = ref('')
const listEl = ref<HTMLDivElement | null>(null)
const textareaEl = ref<HTMLTextAreaElement | null>(null)
// Прижата ли лента к низу: если пользователь листает историю, не дёргаем её.
const pinnedToBottom = ref(true)
const sidebarOpen = ref(false)

// Открыт личный диалог (иначе показан общий канал).
const isDirect = computed(() => props.activePeer !== null)

// В сети ли собеседник открытого диалога.
const peerOnline = computed(() => {
  const peer = props.activePeer
  if (!peer) {
    return true
  }

  const key = normalize(peer)
  return props.users.some((user) => normalize(user) === key)
})

// Можно ли отправить: есть соединение, получатель доступен и текст не пустой.
const canSend = computed(
  () =>
    props.status === 'online' &&
    (!isDirect.value || peerOnline.value) &&
    draft.value.trim().length > 0,
)

// Пояснение над лентой, если что-то мешает общению.
const connectionNote = computed(() => {
  if (props.status === 'offline') {
    return 'Соединение потеряно. Проверьте, что сервер запущен, и попробуйте снова.'
  }
  if (props.status === 'connecting') {
    return 'Переподключаемся к серверу…'
  }
  if (isDirect.value && !peerOnline.value) {
    return `${props.activePeer} не в сети — сообщение не доставить.`
  }
  return ''
})

// Подзаголовок в шапке: тип канала и число участников или статус собеседника.
const subtitle = computed(() => {
  if (props.activePeer) {
    return peerOnline.value
      ? 'Личная переписка · в сети'
      : 'Личная переписка · не в сети'
  }
  return `Общий канал · ${props.users.length} онлайн`
})

// Текст-заполнитель поля ввода зависит от состояния соединения и диалога.
const placeholder = computed(() => {
  if (props.status !== 'online') {
    return 'Нет соединения'
  }
  if (!props.activePeer) {
    return 'Напишите сообщение…'
  }
  if (!peerOnline.value) {
    return 'Пользователь не в сети'
  }
  return `Сообщение для ${props.activePeer}…`
})

// Подсказка под полем ввода.
const composerHint = computed(() =>
  isDirect.value
    ? 'Enter — отправить, Shift + Enter — новая строка · видят только вы двое'
    : 'Enter — отправить, Shift + Enter — новая строка',
)

// Что показать, пока в ленте нет сообщений.
const emptyText = computed(() => {
  if (props.activePeer) {
    return `Пока ни одного сообщения. Напишите ${props.activePeer} — это увидите только вы вдвоём.`
  }
  return 'Пока тихо. Напишите первым — сообщение увидят все, кто сейчас в чате.'
})

// Время в формате «ЧЧ:ММ».
const timeFormatter = new Intl.DateTimeFormat('ru-RU', {
  hour: '2-digit',
  minute: '2-digit',
})

/** Имя без пробелов по краям и в нижнем регистре — для сравнения. */
function normalize(name: string): string {
  return name.trim().toLowerCase()
}

/** Совпадают ли имена без учёта регистра. */
function sameName(first: string, second: string): boolean {
  return normalize(first) === normalize(second)
}

/** Выбран ли этот диалог (учитывая, что имена сравниваются без учёта регистра). */
function isActive(peer: string | null): boolean {
  if (peer === null || props.activePeer === null) {
    return peer === null && props.activePeer === null
  }
  return sameName(peer, props.activePeer)
}

/** Форматирует время сообщения; при ошибке разбора возвращает пустую строку. */
function formatTime(value: string): string {
  const date = new Date(value.replace(/\.(\d{3})\d+/, '.$1'))
  return Number.isNaN(date.getTime()) ? '' : timeFormatter.format(date)
}

/** Детерминированный цвет аватара: один и тот же оттенок для одного и того же имени. */
function avatarStyle(name: string): Record<string, string> {
  let hash = 0
  for (const char of name) {
    hash = (hash * 31 + (char.codePointAt(0) ?? 0)) % 360
  }
  return { '--avatar-hue': String(hash) }
}

/** Первая буква имени для аватара. */
function initial(name: string): string {
  return [...name][0]?.toUpperCase() ?? '?'
}

/** Прокручивает ленту вниз и снова прижимает её к низу. */
function scrollToBottom() {
  const el = listEl.value
  if (el) {
    el.scrollTop = el.scrollHeight
    pinnedToBottom.value = true
  }
}

/** Считаем ленту прижатой к низу, если до края меньше 80 пикселей. */
function onScroll() {
  const el = listEl.value
  if (!el) {
    return
  }
  pinnedToBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 80
}

/** Подстраивает высоту textarea под текст, но не выше 160 пикселей. */
function resize() {
  const el = textareaEl.value
  if (!el) {
    return
  }
  el.style.height = 'auto'
  el.style.height = `${Math.min(el.scrollHeight, 160)}px`
}

/** Переключает диалог и закрывает мобильную панель. */
function open(peer: string | null) {
  sidebarOpen.value = false
  emit('openConversation', peer)
}

/** Отправляет черновик и возвращает ленту к низу. */
function submit() {
  if (!canSend.value) {
    return
  }

  emit('send', draft.value)
  draft.value = ''
  pinnedToBottom.value = true

  nextTick(() => {
    resize()
    scrollToBottom()
    textareaEl.value?.focus()
  })
}

/** Enter отправляет сообщение, Shift+Enter добавляет новую строку. */
function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
    event.preventDefault()
    submit()
  }
}

// При смене диалога очищаем черновик и прокручиваем новую ленту вниз.
watch(
  () => props.activePeer,
  async () => {
    draft.value = ''
    pinnedToBottom.value = true
    await nextTick()
    resize()
    scrollToBottom()
  },
)

// Новые сообщения прокручиваем только если пользователь не читает историю выше.
watch(
  () => props.timeline.length,
  async () => {
    if (!pinnedToBottom.value) {
      return
    }
    await nextTick()
    scrollToBottom()
  },
)

onMounted(async () => {
  await nextTick()
  scrollToBottom()
})
</script>

<template>
  <section class="chat-screen">
    <!-- Затемнение фона, когда на мобильном открыта панель диалогов. -->
    <div v-if="sidebarOpen" class="sidebar-backdrop" @click="sidebarOpen = false"></div>

    <!-- Боковая панель: общий канал, личные переписки и участники онлайн. -->
    <aside class="sidebar" :class="{ 'sidebar--open': sidebarOpen }">
      <button
        class="room room-button"
        :class="{ 'room--active': isActive(null) }"
        type="button"
        @click="open(null)"
      >
        <span class="room-hash">#</span>
        <div class="room-text">
          <strong>general</strong>
          <span>{{ users.length }} онлайн</span>
        </div>
      </button>

      <!-- Личные диалоги: счётчик непрочитанных и время последнего сообщения. -->
      <div v-if="conversations.length" class="sidebar-section">
        <p class="sidebar-label">Личные сообщения</p>
        <ul class="user-list">
          <li v-for="conversation in conversations" :key="conversation.peer">
            <button
              class="user-button"
              :class="{ 'user-button--active': isActive(conversation.peer) }"
              type="button"
              :title="`Открыть переписку с ${conversation.peer}`"
              @click="open(conversation.peer)"
            >
              <span
                class="avatar avatar--small"
                :style="avatarStyle(conversation.peer)"
                aria-hidden="true"
              >
                {{ initial(conversation.peer) }}
              </span>
              <span class="user-name">{{ conversation.peer }}</span>
              <span v-if="conversation.unread" class="unread-badge">
                {{ conversation.unread > 9 ? '9+' : conversation.unread }}
              </span>
            </button>
          </li>
        </ul>
      </div>

      <!-- Участники онлайн: по нажатию открывается личный диалог. -->
      <div class="sidebar-section">
        <p class="sidebar-label">Участники</p>
        <ul class="user-list">
          <li v-for="user in users" :key="user">
            <button
              class="user-button"
              :class="{ 'user-button--active': isActive(user) }"
              type="button"
              :disabled="user === me"
              :title="user === me ? 'Это вы' : `Написать лично ${user}`"
              @click="open(user)"
            >
              <span class="avatar avatar--small" :style="avatarStyle(user)" aria-hidden="true">
                {{ initial(user) }}
              </span>
              <span class="user-name">{{ user }}</span>
              <span v-if="user === me" class="user-you">вы</span>
              <span v-else class="dm-hint" aria-hidden="true">@</span>
            </button>
          </li>
        </ul>
      </div>

      <div class="sidebar-footer">
        <span class="status-pill" :class="`status-pill--${status}`">
          <span class="status-dot"></span>
          {{ STATUS_LABELS[status] }}
        </span>
        <button class="ghost-button" type="button" @click="emit('leave')">Выйти</button>
      </div>
    </aside>

    <div class="chat-main">
      <!-- Шапка: название канала, статус и действия. -->
      <header class="chat-header">
        <div class="chat-heading">
          <h2 v-if="activePeer"><span class="at-sign">@</span> {{ activePeer }}</h2>
          <h2 v-else><span class="room-hash">#</span> general</h2>
          <span class="chat-subtitle">{{ subtitle }}</span>
        </div>
        <div class="chat-actions">
          <button
            class="ghost-button mobile-menu"
            type="button"
            @click="sidebarOpen = true"
          >
            Диалоги
          </button>
          <button
            v-if="status === 'offline'"
            class="primary-button primary-button--small"
            type="button"
            @click="emit('reconnect')"
          >
            Переподключиться
          </button>
          <button class="ghost-button mobile-leave" type="button" @click="emit('leave')">
            Выйти
          </button>
        </div>
      </header>

      <!-- Предупреждение о потере связи или недоступном собеседнике. -->
      <div
        v-if="connectionNote"
        class="banner"
        :class="`banner--${status === 'offline' || (isDirect && !peerOnline) ? 'warning' : 'info'}`"
      >
        <span>{{ connectionNote }}</span>
      </div>

      <!-- Сообщение об ошибке, которое можно закрыть. -->
      <div v-if="error" class="banner banner--error">
        <span>{{ error }}</span>
        <button type="button" class="banner-close" aria-label="Скрыть" @click="emit('dismissError')">
          ×
        </button>
      </div>

      <!-- Лента сообщений: системные уведомления и обычные сообщения. -->
      <div ref="listEl" class="messages" @scroll="onScroll">
        <p v-if="timeline.length === 0" class="empty-state">
          {{ emptyText }}
        </p>

        <template v-for="item in timeline" :key="item.id">
          <div v-if="item.kind === 'system'" class="notice" :title="formatTime(item.at)">
            {{ item.text }}
          </div>
          <MessageBubble
            v-else
            :message="item"
            :own="item.author === me"
            :direct="isDirect"
          />
        </template>
      </div>

      <!-- Поле ввода: Enter отправляет, Shift+Enter переносит строку. -->
      <form class="composer" @submit.prevent="submit">
        <textarea
          ref="textareaEl"
          v-model="draft"
          class="composer-input"
          rows="1"
          maxlength="2000"
          :placeholder="placeholder"
          :disabled="status !== 'online' || (isDirect && !peerOnline)"
          @input="resize"
          @keydown="onKeydown"
        ></textarea>

        <div class="composer-actions">
          <span v-if="draft.length > 1800" class="counter">{{ draft.length }}/2000</span>
          <button class="primary-button primary-button--small" type="submit" :disabled="!canSend">
            Отправить
          </button>
        </div>
      </form>

      <p class="composer-hint">{{ composerHint }}</p>
    </div>
  </section>
</template>
