import { computed, ref, shallowRef } from 'vue'
import type {
  ChatItem,
  ChatMessage,
  ClientMessage,
  ConversationSummary,
  DirectMessage,
  DirectThread,
  ServerMessage,
  TimelineItem,
} from '../types'

/** Состояние соединения с сервером. */
export type ConnectionStatus = 'idle' | 'connecting' | 'online' | 'offline'

/** Ключ в localStorage, где запоминается последнее имя пользователя. */
export const NAME_STORAGE_KEY = 'rust-chat:name'

/** Адрес WebSocket: из `VITE_WS_URL` или тот же хост, что и у страницы. */
function socketUrl(): string {
  const configured = import.meta.env.VITE_WS_URL
  if (configured) {
    return configured
  }

  // На HTTPS используем защищённый WebSocket (wss).
  const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws'
  return `${scheme}://${window.location.host}/ws`
}

/** Приводит имя к ключу для сравнения без учёта регистра. */
function normalizeName(name: string): string {
  return name.trim().toLowerCase()
}

/** Преобразует серверное сообщение общего канала в элемент ленты. */
function toChatItem(message: ChatMessage): ChatItem {
  return {
    kind: 'chat',
    id: message.id,
    author: message.author,
    text: message.text,
    sentAt: message.sent_at,
  }
}

/** Преобразует серверное личное сообщение в элемент ленты. */
function toDirectItem(message: DirectMessage): ChatItem {
  return {
    kind: 'chat',
    id: message.id,
    author: message.from,
    recipient: message.to,
    text: message.text,
    sentAt: message.sent_at,
  }
}

/**
 * Время из RFC 3339 в миллисекундах.
 *
 * PostgreSQL может присылать больше трёх знаков после точки в секундах,
 * поэтому лишние цифры обрезаются — иначе `Date` в некоторых браузерах
 * не разберёт строку.
 */
function timeValue(value: string): number {
  const date = new Date(value.replace(/\.(\d{3})\d+/, '.$1'))
  return Number.isNaN(date.getTime()) ? 0 : date.getTime()
}

/**
 * Состояние чата и все действия с ним.
 *
 * Хук хранит одно WebSocket-соединение, публичную ленту и личные переписки
 * по собеседникам; компоненты только отображают это состояние.
 */
export function useChat() {
  const socket = shallowRef<WebSocket | null>(null)
  const status = ref<ConnectionStatus>('idle')
  const me = ref('')
  const users = ref<string[]>([])
  const timeline = ref<TimelineItem[]>([])

  /** Личные переписки: ключ — имя собеседника в нижнем регистре. */
  const directThreads = ref<Record<string, ChatItem[]>>({})
  /** Написание имён собеседников, как их показываем в интерфейсе. */
  const peerNames = ref<Record<string, string>>({})
  /** Счётчики непрочитанных сообщений по собеседникам. */
  const unread = ref<Record<string, number>>({})
  /** Выбранный диалог; `null` — общий канал `# general`. */
  const activePeer = ref<string | null>(null)

  /** Текст последней ошибки, которую нужно показать пользователю. */
  const error = ref('')

  /** Вошёл ли пользователь в чат. */
  const joined = computed(() => me.value !== '')

  /** Лента текущего экрана: общий канал или открытый диалог. */
  const activeTimeline = computed<TimelineItem[]>(() => {
    const peer = activePeer.value
    if (!peer) {
      return timeline.value
    }
    return directThreads.value[normalizeName(peer)] ?? []
  })

  /** Карточки личных диалогов для боковой панели, свежие — сверху. */
  const conversations = computed<ConversationSummary[]>(() =>
    Object.entries(peerNames.value)
      .map(([key, peer]) => {
        const thread = directThreads.value[key] ?? []
        const last = thread[thread.length - 1]
        return {
          peer,
          unread: unread.value[key] ?? 0,
          lastAt: last?.sentAt ?? '',
        }
      })
      .sort((a, b) => timeValue(b.lastAt) - timeValue(a.lastAt)),
  )

  /** Счётчик для уникальных id системных уведомлений. */
  let systemSequence = 0
  /** Успел ли текущий сокет получить `welcome` (то есть вход состоялся). */
  let joinedSocket = false

  /** Закрывает текущее соединение и снимает обработчики, чтобы `onclose` не сработал. */
  function closeSocket() {
    const current = socket.value
    socket.value = null

    if (!current) {
      return
    }

    current.onopen = null
    current.onmessage = null
    current.onerror = null
    current.onclose = null
    current.close()
  }

  /** Полностью очищает состояние после выхода из чата. */
  function resetSession() {
    me.value = ''
    users.value = []
    timeline.value = []
    directThreads.value = {}
    peerNames.value = {}
    unread.value = {}
    activePeer.value = null
  }

  /** Очищает только публичную часть — при переподключении под тем же именем. */
  function resetPublicView() {
    users.value = []
    timeline.value = []
  }

  /** Открывает личный диалог (или общий канал, если `peer === null`). */
  function openConversation(peer: string | null) {
    if (!peer) {
      activePeer.value = null
      return
    }

    const key = normalizeName(peer)
    if (!peerNames.value[key]) {
      peerNames.value = { ...peerNames.value, [key]: peer }
    }

    activePeer.value = peerNames.value[key] ?? peer

    if ((unread.value[key] ?? 0) > 0) {
      unread.value = { ...unread.value, [key]: 0 }
      send({ type: 'read', peer })
    }
  }

  /** Заменяет локальные переписки тем, что PostgreSQL сохранил для пользователя. */
  function restoreThreads(threads: DirectThread[]) {
    const names = { ...peerNames.value }
    const items = { ...directThreads.value }
    const counts = { ...unread.value }
    const activeKey = activePeer.value ? normalizeName(activePeer.value) : null

    for (const thread of threads) {
      const key = normalizeName(thread.peer)
      names[key] = names[key] ?? thread.peer
      // Сервер пишет в базу до доставки, поэтому его копия никогда не теряет
      // сообщение, которое клиент уже видел.
      items[key] = thread.messages.map(toDirectItem)
      counts[key] = key === activeKey ? 0 : thread.unread
    }

    peerNames.value = names
    directThreads.value = items
    unread.value = counts
  }

  /** Добавляет личное сообщение в нужную переписку и обновляет счётчик непрочитанных. */
  function receiveDirect(message: DirectMessage) {
    // Для собственной копии собеседником считаем получателя.
    const mine = normalizeName(message.from) === normalizeName(me.value)
    const peer = mine ? message.to : message.from
    const key = normalizeName(peer)

    if (!peerNames.value[key]) {
      peerNames.value = { ...peerNames.value, [key]: peer }
    }

    const item = toDirectItem(message)
    const thread = directThreads.value[key] ?? []
    directThreads.value = { ...directThreads.value, [key]: [...thread, item] }

    const activeKey = activePeer.value ? normalizeName(activePeer.value) : null

    if (mine) {
      // Своя реплика никогда не считается непрочитанной.
    } else if (activeKey === key) {
      // Диалог открыт: сбрасываем счётчик и отмечаем прочитанным на сервере,
      // иначе непрочитанное воскреснет после переподключения.
      if ((unread.value[key] ?? 0) > 0) {
        unread.value = { ...unread.value, [key]: 0 }
      }
      send({ type: 'read', peer })
    } else {
      unread.value = { ...unread.value, [key]: (unread.value[key] ?? 0) + 1 }
    }
  }

  /** Разбирает событие сервера и обновляет состояние интерфейса. */
  function handle(message: ServerMessage) {
    switch (message.type) {
      // Вход подтверждён: запоминаем имя и переходим в онлайн.
      case 'welcome':
        me.value = message.you
        users.value = message.users
        status.value = 'online'
        error.value = ''
        joinedSocket = true
        localStorage.setItem(NAME_STORAGE_KEY, message.you)
        break

      // Пришла публичная история — заменяем ленту целиком.
      case 'history':
        timeline.value = message.messages.map(toChatItem)
        break

      // Новое публичное сообщение.
      case 'message':
        timeline.value.push(toChatItem(message.message))
        break

      // Новое личное сообщение (своё эхо или входящее).
      case 'direct':
        receiveDirect(message.message)
        break

      // Восстановленные из базы личные переписки.
      case 'direct_history':
        restoreThreads(message.threads)
        break

      // Системное уведомление о входе или выходе участника.
      case 'system':
        timeline.value.push({
          kind: 'system',
          id: `system-${++systemSequence}`,
          text: message.text,
          at: message.at,
        })
        break

      // Обновился список участников онлайн.
      case 'users':
        users.value = message.users
        break

      // Ошибка: если вход не состоялся, откатываем соединение.
      case 'error':
        error.value = message.message
        if (!joinedSocket) {
          closeSocket()
          resetSession()
          status.value = 'idle'
        }
        break
    }
  }

  /** Подключается к серверу под указанным именем. */
  function connect(name: string) {
    const trimmed = name.trim()
    if (!trimmed) {
      return
    }

    // Переподключение под тем же именем сохраняет личные переписки, которые
    // сервер хранит не для всех; вход под другим именем начинает с чистого листа.
    const sameUser = me.value !== '' && normalizeName(trimmed) === normalizeName(me.value)

    closeSocket()
    if (sameUser) {
      resetPublicView()
    } else {
      resetSession()
    }

    error.value = ''
    status.value = 'connecting'
    joinedSocket = false

    const ws = new WebSocket(socketUrl())
    socket.value = ws

    // Как только соединение открыто, сразу представляемся серверу.
    ws.onopen = () => {
      send({ type: 'join', name: trimmed })
    }

    ws.onmessage = (event) => {
      let message: ServerMessage
      try {
        message = JSON.parse(event.data as string) as ServerMessage
      } catch {
        error.value = 'Сервер прислал некорректный ответ'
        return
      }

      handle(message)
    }

    // Закрытие сокета: показываем «нет связи» или возвращаем экран входа.
    ws.onclose = () => {
      // Игнорируем закрытие старого сокета, если уже создан новый.
      if (socket.value !== ws) {
        return
      }

      socket.value = null
      joinedSocket = false
      if (me.value) {
        status.value = 'offline'
      } else {
        status.value = 'idle'
        if (!error.value) {
          error.value = 'Не удалось подключиться к серверу'
        }
      }
    }
  }

  /** Переподключается под текущим именем после потери связи. */
  function reconnect() {
    const name = me.value
    if (name) {
      connect(name)
    }
  }

  /** Выходит из чата и очищает состояние. */
  function leave() {
    closeSocket()
    resetSession()
    status.value = 'idle'
    error.value = ''
    joinedSocket = false
  }

  /** Отправляет сообщение серверу; `false`, если сокет не открыт. */
  function send(message: ClientMessage): boolean {
    const ws = socket.value
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return false
    }

    ws.send(JSON.stringify(message))
    return true
  }

  /** Отправляет публичное сообщение в `# general` или личное открытому собеседнику. */
  function post(text: string): boolean {
    const trimmed = text.trim()
    if (!trimmed) {
      return false
    }

    const peer = activePeer.value
    if (peer) {
      return send({ type: 'direct', to: peer, text: trimmed })
    }

    return send({ type: 'message', text: trimmed })
  }

  /** Скрывает текущую ошибку. */
  function dismissError() {
    error.value = ''
  }

  return {
    status,
    me,
    users,
    timeline,
    conversations,
    activePeer,
    activeTimeline,
    error,
    joined,
    connect,
    reconnect,
    leave,
    post,
    openConversation,
    dismissError,
  }
}
