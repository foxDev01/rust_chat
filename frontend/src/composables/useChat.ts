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

export type ConnectionStatus = 'idle' | 'connecting' | 'online' | 'offline'

export const NAME_STORAGE_KEY = 'rust-chat:name'

function socketUrl(): string {
  const configured = import.meta.env.VITE_WS_URL
  if (configured) {
    return configured
  }

  const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws'
  return `${scheme}://${window.location.host}/ws`
}

function normalizeName(name: string): string {
  return name.trim().toLowerCase()
}

function toChatItem(message: ChatMessage): ChatItem {
  return {
    kind: 'chat',
    id: message.id,
    author: message.author,
    text: message.text,
    sentAt: message.sent_at,
  }
}

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

function timeValue(value: string): number {
  const date = new Date(value.replace(/\.(\d{3})\d+/, '.$1'))
  return Number.isNaN(date.getTime()) ? 0 : date.getTime()
}

export function useChat() {
  const socket = shallowRef<WebSocket | null>(null)
  const status = ref<ConnectionStatus>('idle')
  const me = ref('')
  const users = ref<string[]>([])
  const timeline = ref<TimelineItem[]>([])

  /** Private threads keyed by the peer's normalized (lowercase) name. */
  const directThreads = ref<Record<string, ChatItem[]>>({})
  /** Preserves the display spelling of every peer we have a thread with. */
  const peerNames = ref<Record<string, string>>({})
  const unread = ref<Record<string, number>>({})
  /** `null` means the public `# general` channel. */
  const activePeer = ref<string | null>(null)

  const error = ref('')

  const joined = computed(() => me.value !== '')

  const activeTimeline = computed<TimelineItem[]>(() => {
    const peer = activePeer.value
    if (!peer) {
      return timeline.value
    }
    return directThreads.value[normalizeName(peer)] ?? []
  })

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

  let systemSequence = 0
  let joinedSocket = false

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

  function resetSession() {
    me.value = ''
    users.value = []
    timeline.value = []
    directThreads.value = {}
    peerNames.value = {}
    unread.value = {}
    activePeer.value = null
  }

  function resetPublicView() {
    users.value = []
    timeline.value = []
  }

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

  /** Replaces local threads with what PostgreSQL has stored for this user. */
  function restoreThreads(threads: DirectThread[]) {
    const names = { ...peerNames.value }
    const items = { ...directThreads.value }
    const counts = { ...unread.value }
    const activeKey = activePeer.value ? normalizeName(activePeer.value) : null

    for (const thread of threads) {
      const key = normalizeName(thread.peer)
      names[key] = names[key] ?? thread.peer
      // The server writes to the database before delivering, so its copy is
      // never missing a message the client has already seen.
      items[key] = thread.messages.map(toDirectItem)
      counts[key] = key === activeKey ? 0 : thread.unread
    }

    peerNames.value = names
    directThreads.value = items
    unread.value = counts
  }

  function receiveDirect(message: DirectMessage) {
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
      // своя реплика никогда не считается непрочитанной
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

  function handle(message: ServerMessage) {
    switch (message.type) {
      case 'welcome':
        me.value = message.you
        users.value = message.users
        status.value = 'online'
        error.value = ''
        joinedSocket = true
        localStorage.setItem(NAME_STORAGE_KEY, message.you)
        break

      case 'history':
        timeline.value = message.messages.map(toChatItem)
        break

      case 'message':
        timeline.value.push(toChatItem(message.message))
        break

      case 'direct':
        receiveDirect(message.message)
        break

      case 'direct_history':
        restoreThreads(message.threads)
        break

      case 'system':
        timeline.value.push({
          kind: 'system',
          id: `system-${++systemSequence}`,
          text: message.text,
          at: message.at,
        })
        break

      case 'users':
        users.value = message.users
        break

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

  function connect(name: string) {
    const trimmed = name.trim()
    if (!trimmed) {
      return
    }

    // Reconnecting under the same nickname keeps the private threads the server
    // does not store; joining as somebody else starts from scratch.
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

    ws.onclose = () => {
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

  function reconnect() {
    const name = me.value
    if (name) {
      connect(name)
    }
  }

  function leave() {
    closeSocket()
    resetSession()
    status.value = 'idle'
    error.value = ''
    joinedSocket = false
  }

  function send(message: ClientMessage): boolean {
    const ws = socket.value
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return false
    }

    ws.send(JSON.stringify(message))
    return true
  }

  /** Sends a public message in `# general` or a private one to the open peer. */
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
