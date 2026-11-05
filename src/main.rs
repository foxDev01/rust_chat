//! Точка входа сервера чата.
//!
//! Здесь собираются вместе конфигурация, подключение к PostgreSQL, общее
//! состояние и HTTP/WebSocket-маршруты, после чего запускается axum.

mod auth;
mod config;
mod db;
mod protocol;
mod rate_limit;
mod routes;
mod state;
mod ws;

use std::{error::Error, net::SocketAddr};

use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{config::Config, db::ChatDb, state::ChatState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Логи настраиваем первыми, чтобы видеть ошибки старта.
    init_tracing();

    // Адрес прослушивания и строка подключения к базе берутся из окружения.
    let config = Config::from_env()?;
    // Подключаемся к PostgreSQL и применяем схему (см. migrations/001_init.sql).
    let db = ChatDb::connect(&config.db_url).await?;
    // Восстанавливаем публичную историю, чтобы перезапуск не терял сообщения.
    let state = ChatState::persisted(db).await?;

    // Собираем маршруты: /ws, /api/* и раздача собранного фронтенда.
    let app = routes::router(state.clone());

    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(address = %config.bind_addr, "chat server is listening");

    // Сервер работает до Ctrl+C или SIGTERM, затем закрывает соединения.
    // ConnectInfo передаёт обработчикам IP клиента: по нему считается лимит
    // попыток входа и регистрации.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Корректно закрываем пул соединений с базой.
    state.close_storage().await;
    info!("server stopped");
    Ok(())
}

/// Инициализирует `tracing` с фильтром из `RUST_LOG` или разумным значением по умолчанию.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("rust_chat=info,tower_http=info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Ждёт сигнал остановки: Ctrl+C на всех платформах и SIGTERM на Unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    // SIGTERM нужен, например, при остановке процесса в Docker или systemd.
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    // На Windows SIGTERM нет — эта ветка просто никогда не завершится.
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // Завершаемся по первому из сигналов.
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received");
}
