<script setup lang="ts">
// Экран чата: боковая панель с диалогами и участниками, лента и поле ввода.
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { MAX_FILE_BYTES, type ConnectionStatus } from '../composables/useChat'
import type { ConversationSummary, Room, TimelineItem } from '../types'
import MessageBubble from './MessageBubble.vue'

// Компонент не хранит состояние чата — только получает его через props.
const props = defineProps<{
  me: string
  users: string[]
  rooms: Room[]
  activeRoom: string
  timeline: TimelineItem[]
  conversations: ConversationSummary[]
  activePeer: string | null
  typingUsers: string[]
  canLoadOlder: boolean
  loadingHistory: boolean
  status: ConnectionStatus
  error: string
  passwordBusy: boolean
  passwordError: string
  passwordNotice: string
  /** Идёт загрузка файла на сервер. */
  uploading: boolean
  /** Готовит blob-ссылку файла для показа и скачивания. */
  loadAttachment: (id: string) => Promise<string>
  /** Скачивает файл сообщения. */
  downloadAttachment: (id: string, name: string) => void
}>()

// События наружу: отправка сообщения (с файлом или без), выбор комнаты или
// диалога, история, «печатает…», выход и переподключение.
const emit = defineEmits<{
  send: [text: string, file: File | null]
  openConversation: [peer: string | null]
  openRoom: [room: string]
  loadOlder: []
  typing: []
  leave: []
  reconnect: []
  changePassword: [current: string, next: string]
  clearPasswordStatus: []
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
// Выбранный файл и открытие его средствами браузера.
const fileInputEl = ref<HTMLInputElement | null>(null)
const selectedFile = ref<File | null>(null)
const fileError = ref('')
// Прижата ли лента к низу: если пользователь листает историю, не дёргаем её.
const pinnedToBottom = ref(true)
const sidebarOpen = ref(false)
// Расстояние от низа ленты до прокрутки: сохраняет позицию при подгрузке старых сообщений.
let historyAnchorOffset: number | null = null

// Форма смены пароля. Поля живут в компоненте, а запрос выполняет хук чата.
const passwordOpen = ref(false)
const currentPassword = ref('')
const newPassword = ref('')
const passwordConfirmation = ref('')
const passwordTouched = ref(false)

// Локальные проверки до отправки: сервер проверит пароль ещё раз.
const passwordLocalError = computed(() => {
  if (!passwordTouched.value) {
    return ''
  }
  if (!currentPassword.value) {
    return 'Введите текущий пароль'
  }
  if ([...newPassword.value].length < 8) {
    return 'Новый пароль должен быть не короче 8 символов'
  }
  if (newPassword.value !== passwordConfirmation.value) {
    return 'Пароли не совпадают'
  }
  if (newPassword.value === currentPassword.value) {
    return 'Новый пароль совпадает с текущим'
  }
  return ''
})

// Открыт личный диалог (иначе показана комната).
const isDirect = computed(() => props.activePeer !== null)

// Название открытой комнаты для шапки и подсказок.
const activeRoomTitle = computed(
  () => props.rooms.find((room) => room.id === props.activeRoom)?.title ?? 'Общий',
)

// В сети ли собеседник открытого диалога.
const peerOnline = computed(() => {
  const peer = props.activePeer
  if (!peer) {
    return true
  }

  const key = normalize(peer)
  return props.users.some((user) => normalize(user) === key)
})

// Можно ли отправить: есть соединение, получатель доступен, есть текст или файл.
const canSend = computed(
  () =>
    props.status === 'online' &&
    !props.uploading &&
    (!isDirect.value || peerOnline.value) &&
    (draft.value.trim().length > 0 || selectedFile.value !== null),
)

// Подпись на кнопке: текст, файл или загрузка.
const sendLabel = computed(() => {
  if (props.uploading && selectedFile.value) {
    return 'Загружаем…'
  }
  return selectedFile.value ? 'Отправить файл' : 'Отправить'
})

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
  return `${activeRoomTitle.value} · ${props.users.length} онлайн`
})

// Текст-заполнитель поля ввода зависит от состояния соединения и диалога.
const placeholder = computed(() => {
  if (props.status !== 'online') {
    return 'Нет соединения'
  }
  if (!props.activePeer) {
    return `Напишите в #${props.activeRoom || 'general'}…`
  }
  if (!peerOnline.value) {
    return 'Пользователь не в сети'
  }
  return `Сообщение для ${props.activePeer}…`
})

// Подсказка под полем ввода.
const composerHint = computed(() =>
  isDirect.value
    ? 'Enter — отправить, Shift + Enter — новая строка · файлы до 10 МБ · видят только вы двое'
    : 'Enter — отправить, Shift + Enter — новая строка · файлы до 10 МБ',
)

// Что показать, пока в ленте нет сообщений.
const emptyText = computed(() => {
  if (props.activePeer) {
    return `Пока ни одного сообщения. Напишите ${props.activePeer} — это увидите только вы вдвоём.`
  }
  return `Пока тихо в #${props.activeRoom || 'general'}. Напишите первым — сообщение увидят все, кто сейчас в комнате.`
})

// Строка «печатает…» под лентой.
const typingText = computed(() => {
  const names = props.typingUsers
  if (names.length === 0) {
    return ''
  }
  if (names.length === 1) {
    return `${names[0]} печатает…`
  }
  if (names.length === 2) {
    return `${names[0]} и ${names[1]} печатают…`
  }
  return 'Несколько человек печатают…'
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

/** Переключает комнату и закрывает мобильную панель. */
function openRoom(room: string) {
  sidebarOpen.value = false
  emit('openRoom', room)
}

/** Подгружает старые сообщения, запоминая текущую позицию прокрутки. */
function loadOlder() {
  const el = listEl.value
  historyAnchorOffset = el ? el.scrollHeight - el.scrollTop : null
  emit('loadOlder')
}

/** Обновляет высоту поля ввода и сообщает серверу, что пользователь печатает. */
function onComposerInput() {
  resize()
  if (!isDirect.value) {
    emit('typing')
  }
}

/** Открывает форму смены пароля и убирает старые сообщения. */
function openPasswordForm() {
  passwordOpen.value = true
  passwordTouched.value = false
  emit('clearPasswordStatus')
}

/** Закрывает форму и очищает введённые пароли. */
function closePasswordForm() {
  passwordOpen.value = false
  currentPassword.value = ''
  newPassword.value = ''
  passwordConfirmation.value = ''
  passwordTouched.value = false
  emit('clearPasswordStatus')
}

/** Проверяет форму и отправляет запрос на смену пароля. */
function submitPassword() {
  passwordTouched.value = true

  if (passwordLocalError.value || props.passwordBusy) {
    return
  }

  emit('changePassword', currentPassword.value, newPassword.value)
}

/** Выбирает файл из диалога браузера; забирает его после подгрузки страницы. */
function pickFile() {
  fileInputEl.value?.click()
}

/** Проверяет выбранный файл: не слишком ли он большой. */
function onFileChange(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0] ?? null

  if (file && file.size > MAX_FILE_BYTES) {
    fileError.value = 'Файл больше 10 МБ'
    selectedFile.value = null
  } else {
    fileError.value = ''
    selectedFile.value = file
  }

  // Сброс значения позволяет выбрать тот же файл повторно.
  input.value = ''
}

/** Убирает выбранный файл из поля ввода. */
function clearFile() {
  selectedFile.value = null
  fileError.value = ''
}

/** Размер файла в понятных единицах. */
function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} Б`
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} КБ`
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} МБ`
}

/** Отправляет черновик и возвращает ленту к низу. */
function submit() {
  if (!canSend.value) {
    return
  }

  emit('send', draft.value, selectedFile.value)
  draft.value = ''
  clearFile()
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

/** Готовит ленту к показу: очищает черновик и прокручивает вниз. */
async function resetTimelineView() {
  draft.value = ''
  clearFile()
  pinnedToBottom.value = true
  historyAnchorOffset = null
  await nextTick()
  resize()
  scrollToBottom()
}

// При смене диалога или комнаты очищаем черновик и прокручиваем новую ленту вниз.
watch(() => props.activePeer, resetTimelineView)
watch(() => props.activeRoom, resetTimelineView)

// Новые сообщения прокручиваем только если пользователь не читает историю выше.
watch(
  () => props.timeline[props.timeline.length - 1]?.id,
  async () => {
    if (!pinnedToBottom.value) {
      return
    }
    await nextTick()
    scrollToBottom()
  },
)

// После подгрузки старых сообщений возвращаем прокрутку к тому же месту.
watch(
  () => props.timeline[0]?.id,
  async (firstId, previous) => {
    if (historyAnchorOffset === null || !previous || firstId === previous) {
      return
    }

    const anchor = historyAnchorOffset
    historyAnchorOffset = null
    await nextTick()

    const el = listEl.value
    if (el) {
      el.scrollTop = el.scrollHeight - anchor
    }
  },
)

// После успешной смены пароля поля больше не нужны: пароль уже не тот.
watch(
  () => props.passwordNotice,
  (notice) => {
    if (!notice) {
      return
    }

    currentPassword.value = ''
    newPassword.value = ''
    passwordConfirmation.value = ''
    passwordTouched.value = false
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

    <!-- Боковая панель: комнаты, личные переписки и участники онлайн. -->
    <aside class="sidebar" :class="{ 'sidebar--open': sidebarOpen }">
      <div class="sidebar-section sidebar-section--rooms">
        <p class="sidebar-label">Комнаты</p>
        <ul class="room-list">
          <li v-for="room in rooms" :key="room.id">
            <button
              class="room room-button"
              :class="{ 'room--active': !isDirect && activeRoom === room.id }"
              type="button"
              :title="`Открыть комнату ${room.title}`"
              @click="openRoom(room.id)"
            >
              <span class="room-hash">#</span>
              <div class="room-text">
                <strong>{{ room.title }}</strong>
                <span>{{ room.id }}</span>
              </div>
            </button>
          </li>
        </ul>
      </div>

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
        <div class="sidebar-actions">
          <button class="ghost-button" type="button" @click="openPasswordForm">Пароль</button>
          <button class="ghost-button" type="button" @click="emit('leave')">Выйти</button>
        </div>
      </div>
    </aside>

    <div class="chat-main">
      <!-- Шапка: название канала, статус и действия. -->
      <header class="chat-header">
        <div class="chat-heading">
          <h2 v-if="activePeer"><span class="at-sign">@</span> {{ activePeer }}</h2>
          <h2 v-else><span class="room-hash">#</span> {{ activeRoom || 'general' }}</h2>
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
        <!-- Подгрузка более старой истории: доступна в комнатах. -->
        <button
          v-if="!isDirect && canLoadOlder"
          class="history-button"
          type="button"
          :disabled="loadingHistory"
          @click="loadOlder"
        >
          {{ loadingHistory ? 'Загружаем…' : 'Показать более ранние' }}
        </button>

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
          :load-attachment="loadAttachment"
          :download-attachment="downloadAttachment"
        />
      </template>
      </div>

      <!-- Кто печатает в комнате; высота строки зарезервирована в стилях. -->
      <p class="typing-indicator" aria-live="polite">
        <span v-if="typingText">{{ typingText }}</span>
      </p>

      <!-- Поле ввода: Enter отправляет, Shift+Enter переносит строку. -->
      <form class="composer" @submit.prevent="submit">
        <!-- Выбранный файл: подпись с именем и кнопка отмены. -->
        <div v-if="selectedFile || fileError" class="file-row">
          <p v-if="fileError" class="file-error banner--error">{{ fileError }}</p>
          <span v-else class="file-chip">
            <span class="file-chip__name">{{ selectedFile?.name }}</span>
            <span class="file-chip__size">
              {{ selectedFile ? formatBytes(selectedFile.size) : '' }}
            </span>
            <button
              class="file-chip__remove"
              type="button"
              aria-label="Убрать файл"
              @click="clearFile"
            >
              ×
            </button>
          </span>
        </div>

        <div class="composer-row">
          <!-- Кнопка прикрепления файла: вызывает скрытый input. -->
          <button
            class="attach-button"
            type="button"
            title="Прикрепить файл"
            aria-label="Прикрепить файл"
            :disabled="status !== 'online' || uploading || (isDirect && !peerOnline)"
            @click="pickFile"
          >
            📎
          </button>
          <input
            ref="fileInputEl"
            class="attach-input"
            type="file"
            @change="onFileChange"
          />

          <textarea
            ref="textareaEl"
            v-model="draft"
            class="composer-input"
            rows="1"
            maxlength="2000"
            :placeholder="placeholder"
            :disabled="status !== 'online' || (isDirect && !peerOnline)"
            @input="onComposerInput"
            @keydown="onKeydown"
          ></textarea>

          <div class="composer-actions">
            <span v-if="draft.length > 1800" class="counter">{{ draft.length }}/2000</span>
            <button class="primary-button primary-button--small" type="submit" :disabled="!canSend">
              {{ sendLabel }}
            </button>
          </div>
        </div>
      </form>

      <p class="composer-hint">{{ composerHint }}</p>
    </div>

    <!-- Модальное окно смены пароля. -->
    <div v-if="passwordOpen" class="modal-backdrop" @click.self="closePasswordForm">
      <section
        class="login-card password-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="password-title"
      >
        <header class="password-modal__head">
          <h2 id="password-title">Смена пароля</h2>
          <button
            class="banner-close"
            type="button"
            aria-label="Закрыть"
            @click="closePasswordForm"
          >
            ×
          </button>
        </header>

        <p class="password-modal__hint">
          После смены пароля остальные устройства выйдут из аккаунта, а это окно
          останется подключённым.
        </p>

        <form class="login-form" @submit.prevent="submitPassword">
          <div class="field">
            <label for="password-current">Текущий пароль</label>
            <input
              id="password-current"
              v-model="currentPassword"
              type="password"
              maxlength="128"
              autocomplete="current-password"
              :disabled="passwordBusy"
            />
          </div>

          <div class="field">
            <label for="password-new">Новый пароль</label>
            <input
              id="password-new"
              v-model="newPassword"
              type="password"
              maxlength="128"
              autocomplete="new-password"
              placeholder="Не короче 8 символов"
              :disabled="passwordBusy"
            />
          </div>

          <div class="field">
            <label for="password-confirm">Повторите новый пароль</label>
            <input
              id="password-confirm"
              v-model="passwordConfirmation"
              type="password"
              maxlength="128"
              autocomplete="new-password"
              :disabled="passwordBusy"
            />
          </div>

          <p v-if="passwordLocalError" class="hint hint--error">{{ passwordLocalError }}</p>
          <p v-if="passwordError" class="alert">{{ passwordError }}</p>
          <p v-if="passwordNotice" class="alert alert--success">{{ passwordNotice }}</p>

          <div class="password-form__actions">
            <button class="ghost-button" type="button" @click="closePasswordForm">
              Отмена
            </button>
            <button
              class="primary-button primary-button--small"
              type="submit"
              :disabled="passwordBusy"
            >
              {{ passwordBusy ? 'Меняем…' : 'Сменить пароль' }}
            </button>
          </div>
        </form>
      </section>
    </div>
  </section>
</template>
