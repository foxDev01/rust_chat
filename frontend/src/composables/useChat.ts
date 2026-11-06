import { computed, onMounted, ref, shallowRef } from 'vue'
import type {
  AuthForm,
  AuthResponse,
  AuthUser,
  ChatItem,
  ChatMessage,
  ClientMessage,
  ConversationSummary,
  DirectMessage,
  DirectThread,
  Room,
  ServerMessage,
  TimelineItem,
} from '../types'

/** Состояние соединения с сервером. */
export type ConnectionStatus = 'idle' | 'connecting' | 'online' | 'offline'

/** Максимальный размер файла, который клиент отправляет на сервер (должен совпадать с сервером). */
export const MAX_FILE_BYTES = 10 * 1024 * 1024

/** Ключ в localStorage, где запоминается последнее имя пользователя. */
export const NAME_STORAGE_KEY = 'rust-chat:name'
/** Ключ в localStorage, где хранится токен сессии. */
export const TOKEN_STORAGE_KEY = 'rust-chat:token'

/** Адрес WebSocket с токеном сессии в строке запроса. */
function socketUrl(token: string): string {
  const configured = import.meta.env.VITE_WS_URL
  const base =
    configured ??
    `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}/ws`

  const url = new URL(base)
  url.searchParams.set('token', token)
  return url.toString()
}

/** Приводит имя к ключу для сравнения без учёта регистра. */
function normalizeName(name: string): string {
  return name.trim().toLowerCase()
}

/** Заголовок авторизации для HTTP-запросов. */
function authHeaders(token: string): HeadersInit {
  return { Authorization: `Bearer ${token}` }
}

/** Выполняет запрос к API и превращает ошибку сервера в исключение с текстом. */
async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response

  try {
    response = await fetch(path, init)
  } catch {
    throw new Error('Не удалось связаться с сервером')
  }

  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { error?: string } | null
    throw new Error(body?.error ?? `Ошибка сервера (${response.status})`)
  }

  // У выхода из аккаунта нет тела ответа.
  if (response.status === 204) {
    return undefined as T
  }

  return (await response.json()) as T
}

/** Преобразует серверное сообщение общего канала в элемент ленты. */
function toChatItem(message: ChatMessage): ChatItem {
  return {
    kind: 'chat',
    id: message.id,
    author: message.author,
    text: message.text,
    sentAt: message.sent_at,
    attachment: message.attachment,
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
    attachment: message.attachment,
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
 * Хук отвечает за вход по аккаунту, хранит одно WebSocket-соединение,
 * публичную ленту и личные переписки по собеседникам.
 */
export function useChat() {
  const socket = shallowRef<WebSocket | null>(null)
  const status = ref<ConnectionStatus>('idle')
  const me = ref('')
  const users = ref<string[]>([])

  /** Доступные комнаты в порядке, который задал сервер. */
  const rooms = ref<Room[]>([])
  /** Комната, в которую сейчас отправляются сообщения. */
  const activeRoom = ref('')
  /** Ленты публичных комнат: ключ — идентификатор комнаты. */
  const roomTimelines = ref<Record<string, TimelineItem[]>>({})
  /** Есть ли в комнате сообщения старше уже загруженных. */
  const roomHasMore = ref<Record<string, boolean>>({})
  /** Кто сейчас печатает в комнате. */
  const typing = ref<Record<string, string[]>>({})
  /** Идёт ли загрузка страницы старой истории. */
  const loadingHistory = ref(false)

  /** Токен текущей сессии; хранится в памяти и в localStorage. */
  const token = ref('')

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

  /** Идёт ли загрузка файла на сервер. */
  const uploading = ref(false)

  /** Готовые blob-ссылки для превью и скачивания: идентификатор сообщения → ссылка. */
  const attachmentUrls = new Map<string, string>()
  /** Загрузки, которые уже выполняются, чтобы не качать один файл дважды. */
  const pendingAttachments = new Map<string, Promise<string>>()

  /** Идёт ли запрос на смену пароля. */
  const passwordBusy = ref(false)
  /** Ошибка формы смены пароля. */
  const passwordError = ref('')
  /** Сообщение об успешной смене пароля. */
  const passwordNotice = ref('')

  /** Вошёл ли пользователь в чат. */
  const joined = computed(() => me.value !== '')

  /** Лента текущего экрана: комната или открытый диалог. */
  const activeTimeline = computed<TimelineItem[]>(() => {
    const peer = activePeer.value
    if (peer) {
      return directThreads.value[normalizeName(peer)] ?? []
    }
    return roomTimelines.value[activeRoom.value] ?? []
  })

  /** Имена участников, которые печатают в открытой комнате. */
  const typingUsers = computed<string[]>(() => {
    if (activePeer.value) {
      return []
    }

    const myKey = normalizeName(me.value)
    return (typing.value[activeRoom.value] ?? []).filter(
      (user) => normalizeName(user) !== myKey,
    )
  })

  /** Есть ли в открытой комнате ещё не загруженные старые сообщения. */
  const canLoadOlder = computed(
    () => !activePeer.value && (roomHasMore.value[activeRoom.value] ?? false),
  )

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
  /** Таймер «welcome не пришёл»: не даёт интерфейсу навсегда зависнуть в «Подключаемся…». */
  let welcomeTimer: ReturnType<typeof setTimeout> | null = null
  /** Когда в последний раз отправляли `typing`, чтобы не спамить сервер. */
  let lastTypingAt = 0
  /** Таймеры, которые убирают имя из строки «печатает…». */
  const typingTimers = new Map<string, ReturnType<typeof setTimeout>>()

  /** Убирает все таймеры индикатора «печатает…». */
  function clearTypingTimers() {
    for (const timer of typingTimers.values()) {
      clearTimeout(timer)
    }
    typingTimers.clear()
  }

  /** Отменяет таймер ожидания `welcome`. */
  function clearWelcomeTimer() {
    if (welcomeTimer !== null) {
      clearTimeout(welcomeTimer)
      welcomeTimer = null
    }
  }

  /** Закрывает текущее соединение и снимает обработчики, чтобы `onclose` не сработал. */
  function closeSocket() {
    clearWelcomeTimer()
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

  /** Полностью очищает состояние чата, не трогая сохранённый токен. */
  function resetSession() {
    me.value = ''
    users.value = []
    rooms.value = []
    activeRoom.value = ''
    roomTimelines.value = {}
    roomHasMore.value = {}
    typing.value = {}
    loadingHistory.value = false
    directThreads.value = {}
    peerNames.value = {}
    unread.value = {}
    activePeer.value = null
    lastTypingAt = 0
    clearTypingTimers()
    revokeAttachmentUrls()
  }

  /** Удаляет токен и возвращает экран входа. */
  function clearSession() {
    token.value = ''
    localStorage.removeItem(TOKEN_STORAGE_KEY)
    resetSession()
    status.value = 'idle'
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

  /** Открывает комнату и просит у сервера её свежую историю. */
  function openRoom(room: string) {
    if (!room) {
      return
    }

    activePeer.value = null
    loadingHistory.value = false

    if (room === activeRoom.value) {
      return
    }

    activeRoom.value = room
    lastTypingAt = 0
    // Пока сервер отвечает, показываем закэшированную ленту этой комнаты.
    send({ type: 'join_room', room })
  }

  /** Показывает, что участник печатает, и гасит надпись через несколько секунд. */
  function receiveTyping(room: string, user: string) {
    if (normalizeName(user) === normalizeName(me.value)) {
      return
    }

    const userKey = normalizeName(user)
    const key = `${room}:${userKey}`
    const current = typing.value[room] ?? []

    if (!current.some((name) => normalizeName(name) === userKey)) {
      typing.value = { ...typing.value, [room]: [...current, user] }
    }

    const previous = typingTimers.get(key)
    if (previous !== undefined) {
      clearTimeout(previous)
    }

    typingTimers.set(
      key,
      setTimeout(() => {
        typingTimers.delete(key)
        const names = typing.value[room] ?? []
        const next = names.filter((name) => normalizeName(name) !== userKey)

        if (next.length) {
          typing.value = { ...typing.value, [room]: next }
          return
        }

        const copy = { ...typing.value }
        delete copy[room]
        typing.value = copy
      }, 4_000),
    )
  }

  /**
   * Сообщает серверу, что пользователь набирает текст.
   *
   * Событие отправляется не чаще раза в 2,5 секунды: сервер рассылает его
   * всем в комнате, и на каждое нажатие отвечать не нужно.
   */
  function notifyTyping() {
    if (!activeRoom.value || activePeer.value || status.value !== 'online') {
      return
    }

    const now = Date.now()
    if (now - lastTypingAt < 2_500) {
      return
    }

    lastTypingAt = now
    send({ type: 'typing', room: activeRoom.value })
  }

  /** Подгружает 50 более старых сообщений открытой комнаты. */
  function loadOlderHistory(): boolean {
    const room = activeRoom.value
    if (!room || activePeer.value || loadingHistory.value) {
      return false
    }
    if (!(roomHasMore.value[room] ?? false)) {
      return false
    }

    const oldest = (roomTimelines.value[room] ?? []).find((item) => item.kind === 'chat')
    if (!oldest) {
      return false
    }

    loadingHistory.value = true
    if (!send({ type: 'history_before', room, before: oldest.id })) {
      loadingHistory.value = false
      return false
    }

    return true
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
      // Вход подтверждён: запоминаем имя, комнаты и переходим в онлайн.
      case 'welcome':
        me.value = message.you
        users.value = message.users
        rooms.value = message.rooms
        activeRoom.value = message.room
        roomTimelines.value = {}
        roomHasMore.value = {}
        typing.value = {}
        clearTypingTimers()
        status.value = 'online'
        error.value = ''
        joinedSocket = true
        localStorage.setItem(NAME_STORAGE_KEY, message.you)
        break

      // Пришла история комнаты — заменяем её ленту целиком.
      case 'history':
        roomTimelines.value = {
          ...roomTimelines.value,
          [message.room]: message.messages.map(toChatItem),
        }
        roomHasMore.value = {
          ...roomHasMore.value,
          [message.room]: message.has_more,
        }
        // После быстрого переключения может прийти ответ на старый запрос:
        // ленту кэшируем, но открытую комнату не меняем.
        if (!activeRoom.value) {
          activeRoom.value = message.room
        }
        loadingHistory.value = false
        break

      // Порция более старых сообщений встаёт в начало ленты комнаты.
      case 'history_page': {
        const existing = roomTimelines.value[message.room] ?? []
        const known = new Set(existing.map((item) => item.id))
        const older = message.messages
          .map(toChatItem)
          .filter((item) => !known.has(item.id))

        roomTimelines.value = {
          ...roomTimelines.value,
          [message.room]: [...older, ...existing],
        }
        roomHasMore.value = {
          ...roomHasMore.value,
          [message.room]: message.has_more,
        }
        loadingHistory.value = false
        break
      }

      // Новое сообщение комнаты (сервер присылает только открытую).
      case 'message': {
        const room = message.message.room
        const current = roomTimelines.value[room] ?? []
        roomTimelines.value = {
          ...roomTimelines.value,
          [room]: [...current, toChatItem(message.message)],
        }
        break
      }

      // Кто-то набирает текст в комнате.
      case 'typing':
        receiveTyping(message.room, message.user)
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
        // В личном диалоге системные события не показываем: его лента
        // состоит только из переписки двух человек.
        if (activePeer.value || !activeRoom.value) {
          break
        }

        roomTimelines.value = {
          ...roomTimelines.value,
          [activeRoom.value]: [
            ...(roomTimelines.value[activeRoom.value] ?? []),
            {
              kind: 'system',
              id: `system-${++systemSequence}`,
              text: message.text,
              at: message.at,
            },
          ],
        }
        break

      // Обновился список участников онлайн.
      case 'users':
        users.value = message.users
        break

      // Ошибка: если вход не состоялся, откатываем соединение.
      case 'error':
        error.value = message.message
        loadingHistory.value = false
        if (!joinedSocket) {
          closeSocket()
          resetSession()
          status.value = 'idle'
        }
        break

      // Сервер завершил сессию (например, пароль сменили на другом устройстве).
      case 'session_ended':
        closeSocket()
        clearSession()
        error.value = message.reason
        status.value = 'idle'
        break
    }
  }

  /** Открывает WebSocket с текущим токеном сессии. */
  function openSocket() {
    if (!token.value) {
      status.value = 'idle'
      return
    }

    closeSocket()
    error.value = ''
    status.value = 'connecting'
    joinedSocket = false
    loadingHistory.value = false
    lastTypingAt = 0

    const currentToken = token.value
    const ws = new WebSocket(socketUrl(currentToken))
    socket.value = ws

    ws.onmessage = (event) => {
      // Игнорируем кадры старого сокета, если уже открыт новый.
      if (socket.value !== ws) {
        return
      }

      let message: ServerMessage
      try {
        message = JSON.parse(event.data as string) as ServerMessage
      } catch {
        error.value = 'Сервер прислал некорректный ответ'
        return
      }

      handle(message)

      if (joinedSocket) {
        clearWelcomeTimer()
      }
    }

    // Соединение закрылось: проверяем, жива ли ещё сессия.
    ws.onclose = () => {
      // Игнорируем закрытие старого сокета, если уже создан новый.
      if (socket.value !== ws) {
        return
      }

      socket.value = null
      clearWelcomeTimer()
      const welcomed = joinedSocket
      joinedSocket = false

      if (!welcomed) {
        void verifySession(currentToken)
        return
      }

      status.value = me.value ? 'offline' : 'idle'
      if (!me.value && !error.value) {
        error.value = 'Не удалось подключиться к серверу'
      }
    }

    // Страховка: если сервер не ответил `welcome` за 10 секунд, выходим из
    // «Подключаемся…» и проверяем сессию, чтобы экран не завис навсегда.
    clearWelcomeTimer()
    welcomeTimer = setTimeout(() => {
      if (socket.value !== ws || joinedSocket) {
        return
      }

      closeSocket()
      void verifySession(currentToken)
    }, 10_000)
  }

  /** Проверяет сохранённый токен после неудачного апгрейда WebSocket. */
  async function verifySession(currentToken: string) {
    try {
      const user = await api<AuthUser>('/api/me', { headers: authHeaders(currentToken) })

      // Сессия жива: скорее всего, аккаунт уже открыт в другом окне.
      me.value = user.username
      status.value = 'offline'
      if (!error.value) {
        error.value = 'Соединение потеряно. Попробуйте переподключиться.'
      }
    } catch {
      clearSession()
      error.value = 'Сессия истекла, войдите снова'
    }
  }

  /** Входит в аккаунт или регистрирует новый. */
  async function connect(form: AuthForm) {
    const username = form.username.trim()
    if (!username || !form.password) {
      error.value = 'Введите имя и пароль'
      return
    }

    closeSocket()
    resetSession()
    error.value = ''
    status.value = 'connecting'

    try {
      const response = await api<AuthResponse>(
        form.mode === 'register' ? '/api/register' : '/api/login',
        {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ username, password: form.password }),
        },
      )

      token.value = response.token
      localStorage.setItem(TOKEN_STORAGE_KEY, response.token)
      localStorage.setItem(NAME_STORAGE_KEY, response.user.username)
      me.value = response.user.username
      openSocket()
    } catch (cause) {
      status.value = 'idle'
      error.value = cause instanceof Error ? cause.message : 'Не удалось войти'
    }
  }

  /** Восстанавливает сессию из localStorage при загрузке страницы. */
  async function restoreSession() {
    const stored = localStorage.getItem(TOKEN_STORAGE_KEY)
    if (!stored) {
      return
    }

    token.value = stored
    status.value = 'connecting'

    try {
      const user = await api<AuthUser>('/api/me', { headers: authHeaders(stored) })
      me.value = user.username
      localStorage.setItem(NAME_STORAGE_KEY, user.username)
      status.value = 'idle'
      openSocket()
    } catch {
      clearSession()
    }
  }

  /** Переподключается с текущим токеном после потери связи. */
  function reconnect() {
    openSocket()
  }

  /**
   * Меняет пароль текущего аккаунта.
   *
   * Сервер требует текущий пароль и после успешной смены отзывает остальные
   * сессии, поэтому текущему окну повторно входить не нужно.
   */
  async function changePassword(currentPassword: string, newPassword: string): Promise<boolean> {
    const currentToken = token.value
    passwordError.value = ''
    passwordNotice.value = ''

    if (!currentToken) {
      passwordError.value = 'Требуется вход'
      return false
    }

    passwordBusy.value = true
    try {
      await api<void>('/api/password', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          ...authHeaders(currentToken),
        },
        body: JSON.stringify({
          current_password: currentPassword,
          new_password: newPassword,
        }),
      })

      passwordNotice.value = 'Пароль изменён. Остальные сеансы завершены.'
      return true
    } catch (cause) {
      passwordError.value =
        cause instanceof Error ? cause.message : 'Не удалось сменить пароль'
      return false
    } finally {
      passwordBusy.value = false
    }
  }

  /** Убирает сообщения формы смены пароля. */
  function clearPasswordStatus() {
    passwordError.value = ''
    passwordNotice.value = ''
  }

  /** Выходит из аккаунта: отзывает сессию и очищает состояние. */
  function leave() {
    const currentToken = token.value

    closeSocket()
    if (currentToken) {
      // Выход не должен мешать интерфейсу, поэтому ошибку игнорируем.
      void fetch('/api/logout', {
        method: 'POST',
        headers: authHeaders(currentToken),
      }).catch(() => undefined)
    }

    clearSession()
    error.value = ''
    joinedSocket = false
  }

  /** Освобождает blob-ссылки загруженных файлов: они больше не нужны. */
  function revokeAttachmentUrls() {
    for (const url of attachmentUrls.values()) {
      URL.revokeObjectURL(url)
    }
    attachmentUrls.clear()
    pendingAttachments.clear()
  }

  /**
   * Готовит blob-ссылку файла сообщения: скачивает байты с авторизацией.
   *
   * Адрес `/api/attachments/{id}` требует токен сессии, который нельзя
   * безопасно вставить в `<img src>`, поэтому файл получает браузер и создаёт
   * локальную ссылку. Ссылки кэшируются до выхода из аккаунта.
   */
  async function loadAttachment(id: string): Promise<string> {
    const cached = attachmentUrls.get(id)
    if (cached) {
      return cached
    }

    const inflight = pendingAttachments.get(id)
    if (inflight) {
      return inflight
    }

    const currentToken = token.value
    if (!currentToken) {
      throw new Error('Требуется вход')
    }

    const promise = (async () => {
      const response = await fetch(`/api/attachments/${id}`, {
        headers: authHeaders(currentToken),
      })
      if (!response.ok) {
        throw new Error('Не удалось загрузить файл')
      }

      const blob = await response.blob()
      const url = URL.createObjectURL(blob)
      attachmentUrls.set(id, url)
      return url
    })()

    pendingAttachments.set(id, promise)
    try {
      return await promise
    } finally {
      pendingAttachments.delete(id)
    }
  }

  /** Скачивает файл сообщения под исходным именем. */
  async function downloadAttachment(id: string, name: string): Promise<void> {
    try {
      const url = await loadAttachment(id)

      const link = document.createElement('a')
      link.href = url
      link.download = name
      document.body.appendChild(link)
      link.click()
      link.remove()
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : 'Не удалось скачать файл'
    }
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

  /**
   * Отправляет сообщение: с файлом — через HTTP-загрузку, без него — по WebSocket.
   *
   * Файл отправляется в текущую комнату или открытый личный диалог; сам текст
   * уходит как комментарий к файлу и может быть пустым.
   */
  async function post(text: string, file: File | null = null): Promise<boolean> {
    if (file) {
      return uploadFile(file, text)
    }

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

  /** Загружает файл на сервер; сообщение создаёт и рассылает сам сервер. */
  async function uploadFile(file: File, caption: string): Promise<boolean> {
    const currentToken = token.value
    const peer = activePeer.value
    const room = activeRoom.value

    if (!currentToken || (!peer && !room)) {
      error.value = 'Нет соединения — файл не отправлен'
      return false
    }

    if (file.size > MAX_FILE_BYTES) {
      error.value = 'Файл больше 10 МБ'
      return false
    }

    const body = new FormData()
    body.append('file', file, file.name)
    if (peer) {
      body.append('to', peer)
    } else {
      body.append('room', room)
    }
    if (caption.trim()) {
      body.append('text', caption.trim())
    }

    uploading.value = true
    try {
      await api<{ id: string }>('/api/upload', {
        method: 'POST',
        headers: authHeaders(currentToken),
        body,
      })
      return true
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : 'Не удалось отправить файл'
      return false
    } finally {
      uploading.value = false
    }
  }

  /** Скрывает текущую ошибку. */
  function dismissError() {
    error.value = ''
  }

  // При загрузке страницы пробуем войти по сохранённому токену.
  onMounted(() => {
    void restoreSession()
  })

  return {
    status,
    me,
    users,
    rooms,
    activeRoom,
    conversations,
    activePeer,
    activeTimeline,
    typingUsers,
    canLoadOlder,
    loadingHistory,
    error,
    joined,
    passwordBusy,
    passwordError,
    passwordNotice,
    uploading,
    connect,
    reconnect,
    leave,
    post,
    openConversation,
    openRoom,
    loadOlderHistory,
    notifyTyping,
    changePassword,
    clearPasswordStatus,
    dismissError,
    loadAttachment,
    downloadAttachment,
  }
}
