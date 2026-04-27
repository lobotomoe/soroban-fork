//! # soroban-fork
//!
//! Lazy-loading mainnet / testnet fork for Soroban tests.
//!
//! Think [Foundry's Anvil](https://book.getfoundry.sh/anvil/), but for Stellar
//! Soroban. When a test reads a ledger entry that isn't cached, it's fetched
//! from the Soroban RPC on the fly. State changes are local only — the real
//! network is never mutated. On drop, lazy-fetched entries can be persisted
//! to disk in the standard `stellar snapshot create` format, so a second run
//! is fully local.
//!
//! ```rust,no_run
//! use soroban_fork::ForkConfig;
//!
//! let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
//!     .cache_file("fork_cache.json")
//!     .build()
//!     .expect("fork setup");
//!
//! // `env` derefs to `soroban_sdk::Env`, backed by real network state.
//! env.mock_all_auths();
//! ```
//!
//! ## Error handling
//!
//! [`ForkConfig::build`] returns [`Result<ForkedEnv, ForkError>`] so
//! transport, cache, and XDR errors are all recoverable. Inside the VM
//! loop — where the trait signature forbids returning errors — the source
//! honors [`FetchMode::Strict`] (panic on transport failure) or
//! [`FetchMode::Lenient`] (log + treat entry as missing).
//!
//! ## Logging
//!
//! The crate uses the [`log`] facade — no output appears unless the test
//! binary initializes a logger (e.g. `env_logger`). Typical invocation:
//!
//! ```bash
//! RUST_LOG=soroban_fork=info cargo test
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(rust_2018_idioms)]

mod cache;
mod error;
mod rpc;
mod source;

pub use error::{ForkError, Result};
pub use rpc::{FetchedEntry, LatestLedger, NetworkMetadata, RpcClient, RpcConfig};
pub use source::{FetchMode, RpcSnapshotSource};

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use log::{info, warn};
use soroban_sdk::testutils::{Ledger as _, SnapshotSourceInput};
use soroban_sdk::{Env, IntoVal, Symbol, Val};

/// Average Stellar ledger close time in seconds. Used by [`ForkedEnv::warp_time`]
/// and [`ForkedEnv::warp_ledger`] to keep the two advancement modes in sync.
const LEDGER_INTERVAL_SECONDS: u64 = 5;

// ---------------------------------------------------------------------------
// ForkedEnv
// ---------------------------------------------------------------------------

/// A forked Soroban environment backed by real network state.
///
/// Derefs to [`soroban_sdk::Env`] so all standard SDK methods work directly.
/// When dropped, any lazy-fetched entries are persisted to the cache file
/// if one was configured.
pub struct ForkedEnv {
    env: Env,
    source: Rc<RpcSnapshotSource>,
    cache_path: Option<PathBuf>,
    ledger_sequence: u32,
    timestamp: u64,
    network_id: [u8; 32],
    protocol_version: u32,
}

impl std::ops::Deref for ForkedEnv {
    type Target = Env;
    fn deref(&self) -> &Env {
        &self.env
    }
}

impl ForkedEnv {
    /// Borrow the underlying [`Env`]. Handy when you need the `Env` but not
    /// the `ForkedEnv` wrapper — passing `&*forked` also works through `Deref`.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Number of RPC calls served through the fork's cache since construction.
    /// A cached (hit) read does not count; only lazy fetches that actually
    /// reached the network.
    pub fn fetch_count(&self) -> u32 {
        self.source.fetch_count()
    }

    /// Advance both the ledger sequence and timestamp. Soroban equivalent
    /// of Anvil's `evm_increaseTime` + `evm_mine` in one call.
    pub fn warp(&self, ledgers: u32, seconds: u64) {
        self.env.ledger().with_mut(|info| {
            info.sequence_number += ledgers;
            info.timestamp += seconds;
        });
    }

    /// Advance time by `seconds`; ledger sequence moves proportionally
    /// (~5 seconds per ledger, matching Stellar's target close rate).
    pub fn warp_time(&self, seconds: u64) {
        let ledgers = (seconds / LEDGER_INTERVAL_SECONDS) as u32;
        self.warp(ledgers, seconds);
    }

    /// Advance by `ledgers` ledgers; timestamp moves proportionally.
    pub fn warp_ledger(&self, ledgers: u32) {
        let seconds = ledgers as u64 * LEDGER_INTERVAL_SECONDS;
        self.warp(ledgers, seconds);
    }

    /// Set a SEP-41 token balance to an exact amount. Soroban equivalent
    /// of Foundry's `deal()`.
    ///
    /// Computes the delta against the current balance and calls `mint`
    /// (if increasing) or `burn` (if decreasing) on the token contract.
    /// Requires [`Env::mock_all_auths`] because the caller has to stand
    /// in as the token's admin for `mint` and as `to` for `burn`.
    ///
    /// ```rust,ignore
    /// env.mock_all_auths();
    /// env.deal_token(&usdc, &alice, 1_000_000 * UNIT);
    /// ```
    pub fn deal_token(
        &self,
        token: &soroban_sdk::Address,
        to: &soroban_sdk::Address,
        amount: i128,
    ) {
        let e = &self.env;
        let to_val: Val = to.into_val(e);

        let current: i128 = e.invoke_contract(token, &Symbol::new(e, "balance"), {
            let mut v = soroban_sdk::Vec::new(e);
            v.push_back(to_val);
            v
        });

        let delta = amount - current;
        if delta == 0 {
            return;
        }

        let target_val: Val = to.into_val(e);
        if delta > 0 {
            let delta_val: Val = delta.into_val(e);
            let mut args = soroban_sdk::Vec::new(e);
            args.push_back(target_val);
            args.push_back(delta_val);
            e.invoke_contract::<()>(token, &Symbol::new(e, "mint"), args);
        } else {
            let abs_delta: Val = delta.unsigned_abs().into_val(e);
            let mut args = soroban_sdk::Vec::new(e);
            args.push_back(target_val);
            args.push_back(abs_delta);
            e.invoke_contract::<()>(token, &Symbol::new(e, "burn"), args);
        }
    }

    /// Explicitly persist the cache. Called automatically on drop when a
    /// `cache_file` is configured, but can be invoked manually — useful
    /// if you want cached state before a panic would drop the env.
    pub fn save_cache(&self) -> Result<()> {
        let Some(path) = &self.cache_path else {
            return Ok(());
        };
        do_save_cache(
            &self.source,
            path,
            self.ledger_sequence,
            self.timestamp,
            self.network_id,
            self.protocol_version,
        )
    }
}

impl Drop for ForkedEnv {
    fn drop(&mut self) {
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        match do_save_cache(
            &self.source,
            path,
            self.ledger_sequence,
            self.timestamp,
            self.network_id,
            self.protocol_version,
        ) {
            Ok(()) => {
                let count = self.source.entries().len();
                info!("soroban-fork: saved {count} entries to {}", path.display());
            }
            Err(e) => {
                warn!("soroban-fork: cache save error on drop: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ForkConfig
// ---------------------------------------------------------------------------

/// Builder for a [`ForkedEnv`]. All fields have sensible defaults — the only
/// required input is the RPC URL.
#[derive(Clone, Debug)]
pub struct ForkConfig {
    rpc_url: String,
    cache_path: Option<PathBuf>,
    network_id: Option<[u8; 32]>,
    fetch_mode: Option<FetchMode>,
    pinned_ledger: Option<u32>,
    pinned_timestamp: Option<u64>,
    max_protocol_version: Option<u32>,
    rpc_config: RpcConfig,
}

impl ForkConfig {
    /// Create a new fork config pointing at a Soroban RPC endpoint.
    ///
    /// Common endpoints:
    /// - Testnet: `https://soroban-testnet.stellar.org:443`
    /// - Mainnet (public): `https://soroban-rpc.mainnet.stellar.gateway.fm`
    ///
    /// The network ID is queried from the RPC's `getNetwork` method during
    /// [`Self::build`] — no URL heuristics, no defaults. If you're running
    /// an offline fork (not usually the point), supply the ID explicitly
    /// via [`Self::network_id`].
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            cache_path: None,
            network_id: None,
            fetch_mode: None,
            pinned_ledger: None,
            pinned_timestamp: None,
            max_protocol_version: None,
            rpc_config: RpcConfig::default(),
        }
    }

    /// Path to a JSON file for persisting fetched entries.
    ///
    /// If the file exists at [`Self::build`] time, its entries pre-populate
    /// the cache (compatible with `stellar snapshot create` output). On
    /// [`ForkedEnv`] drop, the cache is written back — subsequent runs are
    /// fully local.
    pub fn cache_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_path = Some(path.into());
        self
    }

    /// Override the network ID (SHA-256 of the network passphrase).
    ///
    /// Normally fetched from the RPC. Override only if you need a specific
    /// value — e.g. for regression tests of signatures computed against an
    /// older network ID.
    pub fn network_id(mut self, id: [u8; 32]) -> Self {
        self.network_id = Some(id);
        self
    }

    /// Select the fetch mode. Defaults to [`FetchMode::Strict`] — RPC errors
    /// inside the VM loop panic, which is what tests want.
    pub fn fetch_mode(mut self, mode: FetchMode) -> Self {
        self.fetch_mode = Some(mode);
        self
    }

    /// Pin the ledger sequence the forked `Env` reports to contracts.
    ///
    /// The RPC itself always returns entries at the latest ledger — this
    /// setting only shifts what `env.ledger().sequence_number()` reports,
    /// which is what contract logic reads. For full state reproducibility,
    /// pair with [`Self::cache_file`].
    pub fn at_ledger(mut self, sequence: u32) -> Self {
        self.pinned_ledger = Some(sequence);
        self
    }

    /// Pin the ledger timestamp the forked `Env` reports (Unix seconds).
    ///
    /// The default is the close time of the ledger we're forking from,
    /// fetched via `getLedgers` at build time. That keeps tests
    /// deterministic across runs — the previous wall-clock default made
    /// every run depend on when it was started, which is a footgun
    /// silently waiting to bite anyone who asserts on contract logic that
    /// reads `env.ledger().timestamp()`.
    ///
    /// Pin an explicit value when reproducing a known historical
    /// scenario or when the network's reported close time would conflict
    /// with the test's assumed timeline.
    pub fn pinned_timestamp(mut self, unix_seconds: u64) -> Self {
        self.pinned_timestamp = Some(unix_seconds);
        self
    }

    /// Cap the protocol version the VM reports to contracts.
    ///
    /// Useful when the real network has upgraded past what your `soroban-sdk`
    /// version knows — the contract sees `max`, not the live value, and
    /// protocol-specific asserts still pass.
    pub fn max_protocol_version(mut self, version: u32) -> Self {
        self.max_protocol_version = Some(version);
        self
    }

    /// Replace the default RPC transport configuration (timeouts, retries,
    /// batch size). See [`RpcConfig`] for tunables.
    pub fn rpc_config(mut self, config: RpcConfig) -> Self {
        self.rpc_config = config;
        self
    }

    /// Build the forked environment.
    ///
    /// Steps:
    /// 1. Construct an [`RpcClient`](crate::RpcConfig) from this config.
    /// 2. Pre-load entries from `cache_file` if it exists.
    /// 3. Resolve network metadata + latest ledger sequence via RPC
    ///    (unless overridden by [`Self::network_id`] / [`Self::at_ledger`]).
    /// 4. Return a [`ForkedEnv`] that derefs to [`Env`].
    pub fn build(self) -> Result<ForkedEnv> {
        let client = Arc::new(rpc::RpcClient::new(
            self.rpc_url.clone(),
            self.rpc_config.clone(),
        )?);

        let source = source::RpcSnapshotSource::new(client.clone());
        let source = match self.fetch_mode {
            Some(mode) => source.with_fetch_mode(mode),
            None => source,
        };

        // Pre-load from cache file
        if let Some(ref path) = self.cache_path {
            if path.exists() {
                match cache::load_snapshot(path) {
                    Ok(entries) => {
                        let count = entries.len();
                        source.preload(entries);
                        info!(
                            "soroban-fork: pre-loaded {count} entries from {}",
                            path.display()
                        );
                    }
                    Err(e) => {
                        warn!(
                            "soroban-fork: cache load error, starting fresh ({}): {e}",
                            path.display()
                        );
                    }
                }
            }
        }

        let latest = client.get_latest_ledger()?;

        // Resolve network_id: explicit override wins; otherwise fetch from RPC.
        let network_id = match self.network_id {
            Some(id) => id,
            None => client.get_network()?.network_id,
        };

        let sequence = self.pinned_ledger.unwrap_or(latest.sequence);
        let protocol_version = match self.max_protocol_version {
            Some(max) if latest.protocol_version > max => {
                info!(
                    "soroban-fork: capping protocol version {} -> {} (max_protocol_version)",
                    latest.protocol_version, max
                );
                max
            }
            _ => latest.protocol_version,
        };
        let timestamp = self.pinned_timestamp.unwrap_or(latest.close_time);

        let sdk_ledger_info = soroban_env_host::LedgerInfo {
            protocol_version,
            sequence_number: sequence,
            timestamp,
            network_id,
            base_reserve: cache::DEFAULT_BASE_RESERVE,
            min_persistent_entry_ttl: cache::DEFAULT_MIN_PERSISTENT_ENTRY_TTL,
            min_temp_entry_ttl: cache::DEFAULT_MIN_TEMP_ENTRY_TTL,
            max_entry_ttl: cache::DEFAULT_MAX_ENTRY_TTL,
        };

        let source_rc = Rc::new(source);
        let input = SnapshotSourceInput {
            source: source_rc.clone(),
            ledger_info: Some(sdk_ledger_info),
            snapshot: None,
        };

        let env = Env::from_ledger_snapshot(input);

        info!("soroban-fork: forked at ledger {sequence} (protocol {protocol_version})");

        Ok(ForkedEnv {
            env,
            source: source_rc,
            cache_path: self.cache_path,
            ledger_sequence: sequence,
            timestamp,
            network_id,
            protocol_version,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn do_save_cache(
    source: &RpcSnapshotSource,
    path: &std::path::Path,
    sequence: u32,
    timestamp: u64,
    network_id: [u8; 32],
    protocol_version: u32,
) -> Result<()> {
    let entries = source.entries();
    if entries.is_empty() {
        return Ok(());
    }
    cache::save_snapshot(
        path,
        &entries,
        sequence,
        timestamp,
        network_id,
        protocol_version,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_config_builder_records_overrides() {
        let cfg = ForkConfig::new("https://example.test")
            .cache_file("/tmp/ignored.json")
            .network_id([7u8; 32])
            .fetch_mode(FetchMode::Lenient)
            .at_ledger(12345)
            .pinned_timestamp(999)
            .max_protocol_version(25);

        assert_eq!(cfg.rpc_url, "https://example.test");
        assert_eq!(
            cfg.cache_path.as_deref(),
            Some(std::path::Path::new("/tmp/ignored.json"))
        );
        assert_eq!(cfg.network_id, Some([7u8; 32]));
        assert_eq!(cfg.fetch_mode, Some(FetchMode::Lenient));
        assert_eq!(cfg.pinned_ledger, Some(12345));
        assert_eq!(cfg.pinned_timestamp, Some(999));
        assert_eq!(cfg.max_protocol_version, Some(25));
    }

    #[test]
    fn fork_config_debug_redacts_nothing_sensitive() {
        // ForkConfig is Debug; this sanity-tests it doesn't panic and
        // renders the URL (no secrets in config today).
        let cfg = ForkConfig::new("https://example.test");
        let s = format!("{cfg:?}");
        assert!(s.contains("example.test"));
    }

    /// The `getNetwork`-based network_id path is exercised in
    /// `tests/network.rs` (marked `#[ignore]` so offline CI still passes).
    #[test]
    fn explicit_network_id_override_is_stored() {
        let cfg = ForkConfig::new("https://example.test").network_id([0xAB; 32]);
        assert_eq!(cfg.network_id, Some([0xAB; 32]));
    }
}
