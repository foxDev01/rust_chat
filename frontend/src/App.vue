<script setup lang="ts">
// Корневой компонент: показывает экран входа или сам чат в зависимости от `joined`.
import ChatView from './components/ChatView.vue'
import LoginView from './components/LoginView.vue'
import { useChat } from './composables/useChat'

// Всё состояние чата живёт в одном хуке и передаётся дочерним компонентам.
const {
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
  uploadProgress,
  uploadingFile,
  queued,
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
} = useChat()
</script>

<template>
  <div class="app">
    <!-- Два экрана, кадр каждого появляется своей анимацией из CSS.
         Общий Transition здесь не нужен: в фоновой вкладке браузер не вызывает
         requestAnimationFrame, и уход старого экрана мог не завершиться никогда. -->
    <LoginView
      v-if="!joined"
      :status="status"
      :error="error"
      @submit="connect"
    />
    <ChatView
      v-else
      :me="me"
      :users="users"
      :rooms="rooms"
      :active-room="activeRoom"
      :timeline="activeTimeline"
      :conversations="conversations"
      :active-peer="activePeer"
      :typing-users="typingUsers"
      :can-load-older="canLoadOlder"
      :loading-history="loadingHistory"
      :status="status"
      :error="error"
      :password-busy="passwordBusy"
      :password-error="passwordError"
      :password-notice="passwordNotice"
      :uploading="uploading"
      :upload-progress="uploadProgress"
      :uploading-file="uploadingFile"
      :queued="queued"
      :load-attachment="loadAttachment"
      :download-attachment="downloadAttachment"
      @send="post"
      @open-conversation="openConversation"
      @open-room="openRoom"
      @load-older="loadOlderHistory"
      @typing="notifyTyping"
      @leave="leave"
      @reconnect="reconnect"
      @change-password="changePassword"
      @clear-password-status="clearPasswordStatus"
      @dismiss-error="dismissError"
    />
  </div>
</template>
