/** Публичное сообщение общего канала (совпадает с `ChatMessage` в `src/protocol.rs`). */
export interface ChatMessage {
  id: string
  author: string
  text: string
  sent_at: string
}

/** Личное сообщение между двумя пользователями. */
export interface DirectMessage {
  id: string
  from: string
  to: string
  text: string
  sent_at: string
}

/** Сообщения, которые присылает сервер на Rust (см. `src/protocol.rs`). */
export type ServerMessage =
  | { type: 'welcome'; you: string; users: string[] }
  | { type: 'history'; messages: ChatMessage[] }
  | { type: 'message'; message: ChatMessage }
  | { type: 'direct'; message: DirectMessage }
  | { type: 'direct_history'; threads: DirectThread[] }
  | { type: 'system'; text: string; at: string }
  | { type: 'users'; users: string[] }
  | { type: 'error'; message: string }

/** Сообщения, которые клиент отправляет серверу на Rust. */
export type ClientMessage =
  | { type: 'message'; text: string }
  | { type: 'direct'; to: string; text: string }
  | { type: 'read'; peer: string }

/** Режим формы входа: существующий аккаунт или создание нового. */
export type AuthMode = 'login' | 'register'

/** Данные, которые форма входа отправляет в хук чата. */
export interface AuthForm {
  username: string
  password: string
  mode: AuthMode
}

/** Публичные данные аккаунта. */
export interface AuthUser {
  id: string
  username: string
}

/** Ответ сервера на вход или регистрацию. */
export interface AuthResponse {
  token: string
  user: AuthUser
}

/** Личная переписка, восстановленная из PostgreSQL после входа. */
export interface DirectThread {
  /** Собеседник в том написании, под которым он вошёл. */
  peer: string
  /** Последние сообщения диалога по порядку. */
  messages: DirectMessage[]
  /** Сколько сообщений от собеседника ещё не прочитано. */
  unread: number
}

/** Элемент ленты: обычное сообщение (общее или личное). */
export interface ChatItem {
  kind: 'chat'
  id: string
  author: string
  text: string
  sentAt: string
  /** Заполняется у личных сообщений: кому адресовано. */
  recipient?: string
}

/** Элемент ленты: системное уведомление о входе или выходе. */
export interface SystemItem {
  kind: 'system'
  id: string
  text: string
  at: string
}

/** Всё, что может отображаться в ленте сообщений. */
export type TimelineItem = ChatItem | SystemItem

/** Краткая карточка личного диалога в боковой панели. */
export interface ConversationSummary {
  peer: string
  unread: number
  lastAt: string
}
