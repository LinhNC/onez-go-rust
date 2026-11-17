//! Server binary: `shorten`.
//!
//! Startup and shutdown follow the previous program exactly: load the
//! configuration from the environment, open the SQLite store (which applies
//! the migration set), start the analytics flush loop, serve HTTP with the
//! `net/http` timeouts, then on `SIGINT`/`SIGTERM` clear readiness, drain
//! in-flight requests, flush the analytics buffer, checkpoint the WAL and
//! close the store.

mod logging;
mod serve;

use std::net::SocketAddr;
use std::sync::Arc;

use shorten::analytics::{Collector, Flusher};
use shorten::config::{bind_addresses, Config};
use shorten::redirectcache::Cache;
use shorten::store::Store;
use shorten_web::App;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinError;

#[tokio::main]
async fn main() {
    logging::init();

    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(error = %error, "invalid configuration");
            std::process::exit(1);
        }
    };

    let database_path = config.database_path.clone();
    let readers = config.reader_connections();
    let store = tokio::task::spawn_blocking(move || Store::open(&database_path, readers))
        .await
        .map(|opened| match opened {
            Ok(store) => Arc::new(store),
            Err(error) => {
                tracing::error!(error = %error, "open SQLite store");
                std::process::exit(1);
            }
        })
        .unwrap_or_else(|error: JoinError| {
            tracing::error!(error = %error, "open SQLite store");
            std::process::exit(1);
        });

    let collector = Collector::new(
        Arc::clone(&store) as Arc<dyn Flusher>,
        config.analytics_interval,
        config.analytics_threshold,
        config.analytics_max_keys,
    );
    let cache = Cache::new(
        config.cache_capacity,
        config.positive_cache_ttl,
        config.negative_cache_ttl,
    );
    let app = App::new(config.clone(), Arc::clone(&store), cache, collector);
    let analytics = app.analytics().start();

    let (listener, local) = bind_first_address(&config.address).await;
    let drain = Arc::new(serve::Drain::new());
    let server = serve::Server::new(listener, serve::make_service(app.clone()));
    let mut serving = tokio::spawn(server.run(Arc::clone(&drain), config.shutdown_timeout));

    tracing::info!(
        address = %config.address,
        local = %local,
        config = %config,
        "server started",
    );

    // Same select the previous program used: the first of the shutdown signal
    // and a serve failure wins. A signal begins the drain; a serve failure is
    // fatal.
    let fatal = tokio::select! {
        result = &mut serving => match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(error.to_string()),
        },
        () = terminate_signal() => { drain.start(); None }
        () = interrupt_signal() => { drain.start(); None }
    };
    if let Some(error) = fatal {
        tracing::error!(error = %error, "serve HTTP");
        let _ = store.close();
        std::process::exit(1);
    }

    tracing::info!("shutdown signal received");
    app.set_ready(false);
    // `http.Server.Shutdown` had the same bound as the operator's shutdown
    // timeout; the serve loop applies it to the live connections.
    match serving.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(error = %error, "drain HTTP requests"),
        Err(error) => tracing::error!(error = %error, "drain HTTP requests"),
    }

    app.analytics().stop();
    let _ = analytics.join();
    if let Err(error) = app.analytics().flush() {
        tracing::error!(error = %error, "flush analytics");
    }
    drop(app);
    if let Err(error) = store.checkpoint() {
        tracing::error!(error = %error, "checkpoint SQLite WAL");
    }
    if let Err(error) = store.close() {
        tracing::error!(error = %error, "close SQLite store");
    }
}

/// Bind the configured address, expanding `:port` to the bracketed dual-stack
/// form, and return the listener with its resolved socket address.
async fn bind_first_address(address: &str) -> (TcpListener, SocketAddr) {
    for candidate in bind_addresses(address) {
        match TcpListener::bind(candidate.as_str()).await {
            Ok(listener) => {
                let local = match listener.local_addr() {
                    Ok(local) => local,
                    Err(error) => {
                        tracing::error!(address = %candidate, %error, "resolve bound address");
                        std::process::exit(1);
                    }
                };
                return (listener, local);
            }
            Err(error) => {
                tracing::warn!(address = %candidate, %error, "bind attempt failed");
            }
        }
    }
    tracing::error!(address = %address, "bind HTTP address");
    std::process::exit(1);
}

/// Resolve on `SIGTERM`.
async fn terminate_signal() {
    match signal(SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(error) => {
            tracing::error!(%error, "install SIGTERM handler");
            std::process::exit(1);
        }
    }
}

/// Resolve on `SIGINT` (the previous program listened for `os.Interrupt`).
async fn interrupt_signal() {
    match signal(SignalKind::interrupt()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(error) => {
            tracing::error!(%error, "install SIGINT handler");
            std::process::exit(1);
        }
    }
}
