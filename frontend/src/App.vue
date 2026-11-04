<script setup lang="ts">
import ChatView from './components/ChatView.vue'
import LoginView from './components/LoginView.vue'
import { useChat } from './composables/useChat'

const {
  status,
  me,
  users,
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
} = useChat()
</script>

<template>
  <div class="app">
    <Transition name="view" mode="out-in">
      <LoginView
        v-if="!joined"
        key="login"
        :status="status"
        :error="error"
        @join="connect"
      />
      <ChatView
        v-else
        key="chat"
        :me="me"
        :users="users"
        :timeline="activeTimeline"
        :conversations="conversations"
        :active-peer="activePeer"
        :status="status"
        :error="error"
        @send="post"
        @open-conversation="openConversation"
        @leave="leave"
        @reconnect="reconnect"
        @dismiss-error="dismissError"
      />
    </Transition>
  </div>
</template>
