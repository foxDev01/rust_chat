<script setup lang="ts">
import { computed, ref } from 'vue'
import { NAME_STORAGE_KEY, type ConnectionStatus } from '../composables/useChat'

const props = defineProps<{
  status: ConnectionStatus
  error: string
}>()

const emit = defineEmits<{
  join: [name: string]
}>()

const name = ref(localStorage.getItem(NAME_STORAGE_KEY) ?? '')
const touched = ref(false)

const busy = computed(() => props.status === 'connecting')

const localError = computed(() => {
  if (!touched.value) {
    return ''
  }

  const length = [...name.value.trim()].length
  if (length < 2) {
    return 'Введите имя от 2 до 24 символов'
  }
  if (length > 24) {
    return 'Имя слишком длинное'
  }
  return ''
})

function submit() {
  touched.value = true
  const value = name.value.trim()

  if ([...value].length < 2 || [...value].length > 24) {
    return
  }

  emit('join', value)
}
</script>

<template>
  <section class="login-screen">
    <div class="login-glow" aria-hidden="true"></div>

    <div class="login-card">
      <div class="brand">
        <span class="brand-mark" aria-hidden="true">🦀</span>
        <div>
          <h1>Ржавый чат</h1>
          <p>WebSocket-комната на Rust и Vue</p>
        </div>
      </div>

      <form class="login-form" @submit.prevent="submit">
        <label for="nickname">Как вас зовут?</label>
        <input
          id="nickname"
          v-model="name"
          name="nickname"
          maxlength="24"
          autocomplete="nickname"
          placeholder="Например, Феррис"
          :disabled="busy"
          autofocus
        />
        <p class="hint" :class="{ 'hint--error': localError }">
          {{ localError || '2–24 символа: буквы, цифры, пробел, «_», «-»' }}
        </p>

        <p v-if="error" class="alert">{{ error }}</p>

        <button class="primary-button" type="submit" :disabled="busy">
          <span v-if="busy" class="spinner" aria-hidden="true"></span>
          {{ busy ? 'Подключаемся…' : 'Войти в чат' }}
        </button>
      </form>

      <p class="login-footer">Сервер: axum + tokio · Клиент: Vue 3 + TypeScript</p>
    </div>
  </section>
</template>
