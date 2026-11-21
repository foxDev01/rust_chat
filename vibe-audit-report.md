# 🩺 vibe-audit: rust_chat

**Итог: 1 находок — 🔴 0 критичных, 🟡 1 важных, 🔵 0 на проверку**

## 🟡 Auth-роут без rate limit
- Где: `frontend\src\types.ts:70`
- Детали: в проекте не найдено ни одной библиотеки rate-limiting
- Как чинить: Добавь лимит на login/signup/reset (express-rate-limit, slowapi, @upstash/ratelimit). Иначе — брутфорс и спам кодов.
