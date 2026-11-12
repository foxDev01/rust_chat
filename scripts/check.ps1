# Локальная проверка проекта без CI.
#
# Запуск из корня репозитория:
#   pwsh scripts/check.ps1
#
# Проверяет форматирование, clippy, тесты и сборку фронтенда. Интеграционные
# тесты PostgreSQL по умолчанию пропускаются: для них нужен CHAT_DB_URL и
# отдельная тестовая база (см. README, раздел «Тесты»).

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

try {
    Write-Host '== cargo fmt ==' -ForegroundColor Cyan
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) {
        throw 'cargo fmt нашёл неотформатированный код'
    }

    Write-Host '== cargo clippy ==' -ForegroundColor Cyan
    cargo clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        throw 'clippy нашёл предупреждения'
    }

    Write-Host '== cargo test ==' -ForegroundColor Cyan
    cargo test
    if ($LASTEXITCODE -ne 0) {
        throw 'тесты не прошли'
    }

    Write-Host '== frontend build ==' -ForegroundColor Cyan
    Push-Location frontend
    try {
        npm run build
        if ($LASTEXITCODE -ne 0) {
            throw 'сборка клиента не прошла'
        }
    }
    finally {
        Pop-Location
    }

    Write-Host 'Все проверки прошли.' -ForegroundColor Green
}
finally {
    Pop-Location
}
