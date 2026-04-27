//! JSON-RPC server mode — Stellar Soroban fork RPC.
//!
//! When enabled via the `server` cargo feature, this module exposes a
//! JSON-RPC HTTP server that speaks the Stellar Soroban RPC dialect, so
//! any consumer using `@stellar/stellar-sdk` (JS), `stellar-rpc-client`
//! (Rust), Stellar Lab, Freighter, or a custom client can point at our
//! `localhost:8000` instance and get state from the forked mainnet.
//!
//! ```ignore
//! use soroban_fork::{ForkConfig, server::Server};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let config = ForkConfig::new("https://soroban-rpc.mainnet.stellar.gateway.fm");
//!
//! Server::builder(config)
//!     .listen("127.0.0.1:8000".parse().unwrap())
//!     .serve()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! Notice that the server takes a [`ForkConfig`], **not** a built
//! [`ForkedEnv`]. The Env contains `Rc` and is `!Send`, so it can't
//! cross a thread boundary; the actor builds it inside its own worker
//! thread. Build errors (RPC unreachable, etc.) propagate back through
//! `serve()` before the listener accepts connections.
//!
//! # Methods supported in v0.5
//!
//! Read methods + simulation:
//! - `getHealth`
//! - `getVersionInfo`
//! - `getNetwork` *(planned: same release)*
//! - `getLatestLedger` *(planned: same release)*
//! - `getLedgers` *(planned: same release)*
//! - `getLedgerEntries` *(planned: same release)*
//! - `simulateTransaction` *(planned: same release)*
//!
//! Deferred to v0.6 / v0.8 / v0.9:
//! - `sendTransaction`, `getTransaction` — landed in v0.6
//! - `fork_setLedgerEntry`, `fork_closeLedgers` — landed in v0.8
//! - `fork_setStorage` — landed in v0.8.2
//! - `fork_setCode` — landed in v0.8.3
//! - `getEvents`, snapshot/revert, ergonomic wrappers (`fork_setBalance`,
//!   `fork_etch`, `fork_impersonate`) — pending
//!
//! # Architecture
//!
//! Multi-thread tokio runtime hosts axum HTTP handlers. They forward
//! commands through a bounded `mpsc` channel to one OS thread that owns
//! the `ForkedEnv` (the SDK's Env is `!Send`, so it has to live on a
//! single thread). See the `actor` module source for the trade-off
//! discussion.

pub(crate) mod actor;
pub(crate) mod handlers;
pub(crate) mod types;

use std::net::SocketAddr;

use axum::{routing::post, Router};
use log::{error, info};
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;

use crate::{ForkConfig, ForkError};

use self::handlers::{jsonrpc_handler, AppState};

/// Top-level error type for [`ServerBuilder::serve`].
///
/// Threads through both fork-build errors (which surface from the actor
/// thread once it tries to build the Env) and OS-level listener errors.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The forked Env could not be built — RPC unreachable, network
    /// metadata fetch failed, etc. Propagated from the actor thread.
    #[error("fork build failed: {0}")]
    Fork(#[from] ForkError),
    /// Binding the listener or running axum failed.
    #[error("listener I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Worker thread died before it could report ready/not-ready.
    /// Practically only happens if the thread panicked during build,
    /// which `ForkConfig::build` shouldn't do.
    #[error("worker thread aborted before reporting build status")]
    WorkerAborted,
}

/// Builder for a [`Server`]. `listen` is the only setting today; future
/// expansions (CORS allowlist, rate limit, auth header) will live here.
#[must_use = "call `serve()` to start the server"]
pub struct ServerBuilder {
    config: ForkConfig,
    listen: Option<SocketAddr>,
}

impl ServerBuilder {
    /// Set the listen address. Default if not called: `127.0.0.1:8000`.
    pub fn listen(mut self, addr: SocketAddr) -> Self {
        self.listen = Some(addr);
        self
    }

    /// Bind the listener and start serving in the background.
    ///
    /// Returns a [`RunningServer`] handle that exposes the bound
    /// address (useful when binding ephemeral with `0.0.0.0:0`) and
    /// gives the caller two ways to wait for shutdown:
    /// [`RunningServer::run_until_signal`] (Ctrl-C / SIGTERM, used by
    /// the CLI) or [`RunningServer::shutdown`] (programmatic, used by
    /// integration tests).
    ///
    /// Errors are surfaced before binding the port:
    /// 1. **Fork build errors** — the actor builds the Env first; if
    ///    that fails (RPC unreachable, bad network metadata), we
    ///    return immediately.
    /// 2. **Listener errors** — port already in use, permission denied.
    pub async fn start(self) -> std::result::Result<RunningServer, ServeError> {
        let addr = self
            .listen
            .unwrap_or_else(|| "127.0.0.1:8000".parse().expect("hard-coded address parses"));

        // Spawn the worker; it builds the Env on its own thread (Env is
        // !Send so it can't cross), then signals ready/error back.
        let (actor, ready) = actor::spawn(self.config);

        match ready.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("soroban-fork: fork build failed: {e}");
                return Err(ServeError::Fork(e));
            }
            Err(_) => return Err(ServeError::WorkerAborted),
        }

        let state = AppState { actor };

        // POST / is the JSON-RPC endpoint. CORS is permissive by default —
        // browser-based tools (Stellar Lab, Freighter dev mode) need it,
        // and the server is meant to live on `localhost` for tests anyway.
        // Operators who care can wrap with their own reverse proxy.
        let app = Router::new()
            .route("/", post(jsonrpc_handler))
            .layer(CorsLayer::permissive())
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        info!("soroban-fork: serving JSON-RPC on http://{local_addr}");

        // Channel used by `shutdown()` to break the serve loop.
        // `axum::serve` takes a future; we select on either an external
        // shutdown signal or the caller's `shutdown_tx.send(())`.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Ok(RunningServer {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            server_task,
        })
    }

    /// Convenience: start the server and run until Ctrl-C / SIGTERM.
    ///
    /// Equivalent to `start().await?.run_until_signal().await`.
    /// The CLI uses this; tests typically use `start()` directly so
    /// they can call `shutdown()` programmatically.
    pub async fn serve(self) -> std::result::Result<(), ServeError> {
        let running = self.start().await?;
        running.run_until_signal().await
    }
}

/// Handle to a running server.
///
/// Cancellation safety: the server task continues to accept connections
/// until `shutdown()` is called or the handle is dropped. Dropping the
/// handle does **not** stop the server — call `shutdown()` explicitly
/// for tests, or `run_until_signal()` for production.
#[must_use = "RunningServer must be awaited or shut down explicitly"]
pub struct RunningServer {
    local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    /// The address the listener actually bound to. When the caller
    /// asked for `0.0.0.0:0`, this returns the OS-assigned port.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Wait for Ctrl-C / SIGTERM and shut down gracefully. Returns
    /// when the server has fully stopped.
    pub async fn run_until_signal(mut self) -> std::result::Result<(), ServeError> {
        shutdown_signal().await;
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.server_task
            .await
            .map_err(|e| ServeError::Io(std::io::Error::other(e.to_string())))??;
        Ok(())
    }

    /// Programmatically shut down the server and await completion.
    pub async fn shutdown(mut self) -> std::result::Result<(), ServeError> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        self.server_task
            .await
            .map_err(|e| ServeError::Io(std::io::Error::other(e.to_string())))??;
        Ok(())
    }
}

/// Top-level server entry point.
pub struct Server;

impl Server {
    /// Start a server builder.
    ///
    /// Takes a [`ForkConfig`], not a built [`crate::ForkedEnv`] — the
    /// SDK's Env is `!Send`, so the actor must build it inside its own
    /// worker thread. Build errors propagate through [`ServerBuilder::serve`].
    pub fn builder(config: ForkConfig) -> ServerBuilder {
        ServerBuilder {
            config,
            listen: None,
        }
    }
}

/// Wait for Ctrl-C or SIGTERM and complete; axum uses this future as
/// the shutdown trigger.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("soroban-fork: SIGINT received, shutting down"),
        _ = terminate => info!("soroban-fork: SIGTERM received, shutting down"),
    }
}
