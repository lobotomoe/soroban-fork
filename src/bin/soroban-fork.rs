//! `soroban-fork` CLI — mostly a thin wrapper that boots the JSON-RPC
//! server with a forked Env. Available only when the `server` feature is
//! enabled (see Cargo.toml `required-features`).
//!
//! ```sh
//! cargo install soroban-fork --features server
//! soroban-fork serve --rpc https://soroban-rpc.mainnet.stellar.gateway.fm
//! ```

use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use log::error;
use soroban_fork::{server::Server, ForkConfig};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Lazy-loading mainnet/testnet fork for Soroban — Anvil-equivalent for Stellar."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the JSON-RPC server bound to a forked Env.
    ///
    /// Any client speaking the Stellar Soroban RPC dialect (the JS,
    /// Python, Go, or Rust SDKs; Stellar Lab; Freighter; custom tools)
    /// can point at the listen address and read state from the forked
    /// network without modification.
    Serve {
        /// Upstream Soroban RPC endpoint to fork from.
        #[arg(
            long,
            default_value = "https://soroban-rpc.mainnet.stellar.gateway.fm",
            env = "SOROBAN_FORK_RPC"
        )]
        rpc: String,

        /// Address to bind the JSON-RPC server to.
        #[arg(long, default_value = "127.0.0.1:8000", env = "SOROBAN_FORK_LISTEN")]
        listen: SocketAddr,

        /// Optional path to a cache file. If it exists, entries pre-load
        /// (skipping initial RPC fetches); on shutdown the cache is
        /// persisted so the next start is fully local.
        #[arg(long, env = "SOROBAN_FORK_CACHE")]
        cache: Option<std::path::PathBuf>,

        /// Enable diagnostic-event tracing in the forked Env. The
        /// server captures `fn_call`/`fn_return` events on every
        /// simulation; useful when consumers ask for trace output.
        #[arg(long)]
        tracing: bool,
    },
}

#[tokio::main]
async fn main() {
    // Initialize the `log` facade. Level via RUST_LOG; default is `info`
    // for our crate so the server prints listen banner and shutdown
    // markers without the user wiring anything up.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("soroban_fork=info"),
    )
    .init();

    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        error!("{e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Serve {
            rpc,
            listen,
            cache,
            tracing,
        } => {
            let mut config = ForkConfig::new(rpc).tracing(tracing);
            if let Some(path) = cache {
                config = config.cache_file(path);
            }
            // The server builds the Env inside its worker thread (Env is
            // !Send), so we hand off the config — fork-build errors
            // surface via `serve().await?`.
            Server::builder(config).listen(listen).serve().await?;
            Ok(())
        }
    }
}
