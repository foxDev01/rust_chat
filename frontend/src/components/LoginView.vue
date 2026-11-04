<script setup lang="ts">
// Экран входа: имя сохраняется в localStorage и проверяется ещё до отправки.
import { computed, ref } from 'vue'
import { NAME_STORAGE_KEY, type ConnectionStatus } from '../composables/useChat'

const props = defineProps<{
  status: ConnectionStatus
  error: string
}>()

// Событие входа: родитель создаёт WebSocket-соединение.
const emit = defineEmits<{
  join: [name: string]
}>()

// Подставляем последнее использованное имя, если оно есть.
const name = ref(localStorage.getItem(NAME_STORAGE_KEY) ?? '')
// Показывать ли ошибку валидации (только после первой попытки отправки).
const touched = ref(false)

// Пока идёт подключение, поле и кнопка блокируются.
const busy = computed(() => props.status === 'connecting')

// Проверка длины имени на клиенте — сервер всё равно проверит его ещё раз.
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

/** Проверяет имя и отправляет событие входа. */
function submit() {
  touched.value = true
  const value = name.value.trim()

  // Слишком короткое или длинное имя дальше не пропускаем.
  if ([...value].length < 2 || [...value].length > 24) {
    return
  }

  emit('join', value)
}
</script>

<template>
  <section class="login-screen">
    <!-- Декоративное свечение на фоне карточки входа. -->
    <div class="login-glow" aria-hidden="true"></div>

    <div class="login-card">
      <!-- Логотип и название проекта. -->
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
