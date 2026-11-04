export interface ChatMessage {
  id: string
  author: string
  text: string
  sent_at: string
}

export interface DirectMessage {
  id: string
  from: string
  to: string
  text: string
  sent_at: string
}

/** Messages sent by the Rust server (see `src/protocol.rs`). */
export type ServerMessage =
  | { type: 'welcome'; you: string; users: string[] }
  | { type: 'history'; messages: ChatMessage[] }
  | { type: 'message'; message: ChatMessage }
  | { type: 'direct'; message: DirectMessage }
  | { type: 'direct_history'; threads: DirectThread[] }
  | { type: 'system'; text: string; at: string }
  | { type: 'users'; users: string[] }
  | { type: 'error'; message: string }

/** Messages sent to the Rust server. */
export type ClientMessage =
  | { type: 'join'; name: string }
  | { type: 'message'; text: string }
  | { type: 'direct'; to: string; text: string }
  | { type: 'read'; peer: string }

/** A private conversation replayed from PostgreSQL after a join. */
export interface DirectThread {
  peer: string
  messages: DirectMessage[]
  unread: number
}

export interface ChatItem {
  kind: 'chat'
  id: string
  author: string
  text: string
  sentAt: string
  /** Set for private messages: who the message was addressed to. */
  recipient?: string
}

export interface SystemItem {
  kind: 'system'
  id: string
  text: string
  at: string
}

export type TimelineItem = ChatItem | SystemItem

/** A private conversation shown in the sidebar. */
export interface ConversationSummary {
  peer: string
  unread: number
  lastAt: string
}
