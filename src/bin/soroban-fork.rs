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
use soroban_fork::{server::Server, test_accounts, ForkConfig};

/// Number of pre-funded deterministic test accounts the CLI mints
/// at fork-build time. Override with `--accounts N` (set to 0 for
/// none).
const DEFAULT_TEST_ACCOUNTS: usize = 10;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Lazy-loading mainnet/testnet fork for Soroban tests. Inspired by Foundry's Anvil."
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

        /// Number of pre-funded deterministic test accounts to
        /// mint into the fork. Each gets 100K XLM. The same seed
        /// produces the same accounts every run, so test code can
        /// reference them by index (e.g. `account_0`). Default: 10.
        #[arg(long, default_value_t = DEFAULT_TEST_ACCOUNTS)]
        accounts: usize,
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
            accounts,
        } => {
            // Print the pre-funded test accounts BEFORE handing off
            // to the server. The accounts are deterministic, so the
            // ones we print here match the ones the server's actor
            // mints during fork-build — no risk of divergence.
            print_account_banner(accounts, listen);

            let mut config = ForkConfig::new(rpc)
                .tracing(tracing)
                .test_account_count(accounts);
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

/// Startup banner: prints the deterministic pre-funded accounts'
/// "G..." (public) and "S..." (secret) strkeys to stdout, plus the
/// listen URL, so users can paste them straight into JS-SDK code:
///
/// ```text
/// soroban-fork v0.7
/// Listening on http://127.0.0.1:8000
///
/// Available test accounts:
/// (0) GBXXX...AB12 (100000.0000000 XLM)  →  SAXXX...CD34
/// (1) GCYYY...EF56 (100000.0000000 XLM)  →  SAXXX...GH78
/// ...
/// ```
fn print_account_banner(count: usize, listen: SocketAddr) {
    println!("soroban-fork v{}", env!("CARGO_PKG_VERSION"));
    println!("Listening on http://{listen}");
    if count == 0 {
        println!("(no pre-funded test accounts; pass --accounts N to enable)");
        return;
    }
    println!();
    println!("Available test accounts:");
    let accounts = test_accounts::generate(count);
    for (i, account) in accounts.iter().enumerate() {
        let xlm = account.balance_stroops as f64 / 10_000_000.0;
        println!(
            "({i}) {}  ({xlm:.7} XLM)  ->  {}",
            account.account_strkey(),
            account.secret_key_strkey()
        );
    }
    println!();
}
