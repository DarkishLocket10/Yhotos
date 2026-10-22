#![forbid(unsafe_code)]

//! Wires validated configuration, tracing and isolated HTTP listeners.

use anyhow::{Context, Result};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::TcpListener, sync::watch};
use tracing_subscriber::EnvFilter;
use yhotos_config::{Config, LogFormat};
use yhotos_http::{AppState, public_router};

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Startup errors have no request id. Configuration errors contain
            // field names and rules only, never environment values.
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !args.is_empty() {
        return create_admin(&args).await;
    }
    let config = Box::leak(Box::new(Config::from_env()?));
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match config.log_format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_writer(std::io::stderr)
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .pretty()
            .with_writer(std::io::stderr)
            .try_init(),
    }
    .map_err(|_| anyhow::anyhow!("could not initialize tracing"))?;
    let public = TcpListener::bind(config.public_listen)
        .await
        .context("could not bind public listener")?;
    #[cfg(feature = "admin")]
    let admin = TcpListener::bind(
        config
            .admin_listen
            .context("admin listener configuration missing")?,
    )
    .await
    .context("could not bind admin listener")?;
    let public_db = std::sync::Arc::new(
        yhotos_db::Db::connect(
            &config.database_url_public,
            yhotos_db::Role::Public,
            16,
            database_tls()?,
        )
        .await
        .map_err(|_| anyhow::anyhow!("could not connect public database"))?,
    );
    #[cfg(feature = "admin")]
    let admin_db = Some(std::sync::Arc::new(
        yhotos_db::Db::connect(
            config
                .database_url_admin
                .as_ref()
                .context("admin database configuration missing")?,
            yhotos_db::Role::Admin,
            8,
            database_tls()?,
        )
        .await
        .map_err(|_| anyhow::anyhow!("could not connect admin database"))?,
    ));
    #[cfg(not(feature = "admin"))]
    let admin_db = None;
    // Both routers share one cache and cryptographic service. Each handler
    // selects only its listener's separately authenticated database pool.
    let auth = yhotos_http::auth::Auth::new(config, public_db, admin_db).await?;
    let state = AppState {
        config,
        auth: Some(auth),
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let public = axum::serve(
        public,
        public_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
    #[cfg(feature = "admin")]
    let admin = axum::serve(
        admin,
        yhotos_http::admin_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx));
    let servers = async {
        #[cfg(feature = "admin")]
        tokio::try_join!(async { public.await }, async { admin.await })
            .map(|_| ())
            .context("HTTP listener failed")?;
        #[cfg(not(feature = "admin"))]
        public.await.context("HTTP listener failed")?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(servers);
    tracing::info!("listeners ready");
    tokio::select! {
        result = &mut servers => result?,
        signal = shutdown_signal() => {
            signal?;
            let _ = shutdown_tx.send(true);
            if let Ok(result) = tokio::time::timeout(Duration::from_secs(10), &mut servers).await {
                result?;
            } else {
                tracing::warn!("shutdown drain deadline reached");
            }
        }
    }
    Ok(())
}
async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    let _ = receiver.wait_for(|value| *value).await;
}
async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("could not register termination signal")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("could not receive interrupt")?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("could not receive interrupt")?;
    Ok(())
}

fn database_tls() -> Result<yhotos_db::DatabaseTls> {
    match std::env::var("YHOTOS_DATABASE_TLS").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("require") => Ok(yhotos_db::DatabaseTls::Require),
        Ok("disable") => Ok(yhotos_db::DatabaseTls::Disable),
        _ => {
            anyhow::bail!("YHOTOS_DATABASE_TLS must be require or disable (local development only)")
        }
    }
}
async fn create_admin(args: &[String]) -> Result<()> {
    use secrecy::{ExposeSecret, SecretString};
    use std::io::Write;
    let [command, flag, username] = args else {
        anyhow::bail!("usage: yhotos-server create-admin --username NAME");
    };
    if command != "create-admin"
        || flag != "--username"
        || username.is_empty()
        || username.len() > 80
        || username.chars().any(char::is_control)
    {
        anyhow::bail!("usage: yhotos-server create-admin --username NAME");
    }
    let url = SecretString::from(
        std::env::var("YHOTOS_DATABASE_URL_ADMIN")
            .map_err(|_| anyhow::anyhow!("YHOTOS_DATABASE_URL_ADMIN required"))?,
    );
    let db = yhotos_db::Db::connect(&url, yhotos_db::Role::Admin, 1, database_tls()?)
        .await
        .map_err(|_| anyhow::anyhow!("could not connect administrative database"))?;
    let password = yhotos_auth::generate_temp_password();
    yhotos_auth::check_password_policy(&password, username)?;
    let hash = yhotos_auth::hash_password(&password)?;
    let mut c = db
        .pool()
        .acquire()
        .await
        .map_err(|_| anyhow::anyhow!("could not connect administrative database"))?;
    yhotos_db::users::create_admin(&mut c, username, &SecretString::from(hash.as_str()))
        .await
        .map_err(|_| anyhow::anyhow!("could not create administrative account"))?;
    // This explicit CLI is the only intentional credential output. Never trace it.
    writeln!(std::io::stdout(), "{}", password.expose_secret())?;
    Ok(())
}
