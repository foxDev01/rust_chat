<script setup lang="ts">
// Экран входа и регистрации: имя и пароль проверяются ещё до отправки.
import { computed, ref } from 'vue'
import { NAME_STORAGE_KEY, type ConnectionStatus } from '../composables/useChat'
import type { AuthForm, AuthMode } from '../types'

const props = defineProps<{
  status: ConnectionStatus
  error: string
}>()

// Событие входа: родитель вызывает API и открывает WebSocket.
const emit = defineEmits<{
  submit: [form: AuthForm]
}>()

const mode = ref<AuthMode>('login')
// Подставляем последнее использованное имя, если оно есть.
const username = ref(localStorage.getItem(NAME_STORAGE_KEY) ?? '')
const password = ref('')
const confirmation = ref('')
// Показывать ли ошибки валидации (только после первой попытки отправки).
const touched = ref(false)

// Пока идёт подключение, поля и кнопка блокируются.
const busy = computed(() => props.status === 'connecting')
const isRegister = computed(() => mode.value === 'register')

// Проверка на клиенте — сервер всё равно проверит имя и пароль ещё раз.
const localError = computed(() => {
  if (!touched.value) {
    return ''
  }

  const nameLength = [...username.value.trim()].length
  if (nameLength < 2) {
    return 'Введите имя от 2 до 24 символов'
  }
  if (nameLength > 24) {
    return 'Имя слишком длинное'
  }

  if ([...password.value].length < 8) {
    return 'Пароль должен быть не короче 8 символов'
  }

  if (isRegister.value && password.value !== confirmation.value) {
    return 'Пароли не совпадают'
  }

  return ''
})

/** Переключает вход и регистрацию, очищая пароль. */
function switchMode(next: AuthMode) {
  if (mode.value === next) {
    return
  }

  mode.value = next
  touched.value = false
  password.value = ''
  confirmation.value = ''
}

/** Проверяет форму и отправляет данные родителю. */
function submit() {
  touched.value = true
  const value = username.value.trim()

  if ([...value].length < 2 || [...value].length > 24) {
    return
  }
  if ([...password.value].length < 8) {
    return
  }
  if (isRegister.value && password.value !== confirmation.value) {
    return
  }

  emit('submit', { username: value, password: password.value, mode: mode.value })
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

      <!-- Переключатель между входом и регистрацией. -->
      <div class="auth-toggle" role="tablist" aria-label="Вход или регистрация">
        <button
          type="button"
          role="tab"
          :aria-selected="!isRegister"
          :class="{ 'auth-toggle__button--active': !isRegister }"
          @click="switchMode('login')"
        >
          Вход
        </button>
        <button
          type="button"
          role="tab"
          :aria-selected="isRegister"
          :class="{ 'auth-toggle__button--active': isRegister }"
          @click="switchMode('register')"
        >
          Регистрация
        </button>
      </div>

      <form class="login-form" @submit.prevent="submit">
        <div class="field">
          <label for="username">Имя пользователя</label>
          <input
            id="username"
            v-model="username"
            name="username"
            maxlength="24"
            autocomplete="username"
            placeholder="Например, Феррис"
            :disabled="busy"
            autofocus
          />
          <p class="hint" :class="{ 'hint--error': localError }">
            {{ localError || '2–24 символа: буквы, цифры, пробел, «_», «-»' }}
          </p>
        </div>

        <div class="field">
          <label for="password">Пароль</label>
          <input
            id="password"
            v-model="password"
            name="password"
            type="password"
            maxlength="128"
            :autocomplete="isRegister ? 'new-password' : 'current-password'"
            placeholder="Не короче 8 символов"
            :disabled="busy"
          />
        </div>

        <div v-if="isRegister" class="field">
          <label for="password-confirmation">Повторите пароль</label>
          <input
            id="password-confirmation"
            v-model="confirmation"
            name="password-confirmation"
            type="password"
            maxlength="128"
            autocomplete="new-password"
            placeholder="Ещё раз тот же пароль"
            :disabled="busy"
          />
        </div>

        <p v-if="error" class="alert">{{ error }}</p>

        <button class="primary-button" type="submit" :disabled="busy">
          <span v-if="busy" class="spinner" aria-hidden="true"></span>
          {{ busy ? 'Подключаемся…' : isRegister ? 'Создать аккаунт' : 'Войти в чат' }}
        </button>
      </form>

      <p class="login-footer">Пароли хранятся в виде Argon2-хешей</p>
    </div>
  </section>
</template>
