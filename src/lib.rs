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
pub mod fees;
mod rpc;
mod source;
pub mod test_accounts;
pub mod trace;

/// JSON-RPC server mode. Available with the `server` cargo feature —
/// pulls in tokio + axum + tower-http; library-mode users (default)
/// don't pay for these deps.
#[cfg(feature = "server")]
pub mod server;

pub use error::{ForkError, Result};
pub use rpc::{FetchedEntry, LatestLedger, NetworkMetadata, RpcClient, RpcConfig};
pub use source::{FetchMode, RpcSnapshotSource};
pub use trace::{Trace, TraceFrame, TraceResult};

use std::cell::OnceCell;
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
    /// Network passphrase from the fork-time `getNetwork` call. Kept so
    /// the optional JSON-RPC server can answer `getNetwork` without a
    /// re-query and so future reproducibility tools can verify a cache
    /// matches its declared network. `None` only when the user
    /// overrode `network_id` and we never had a passphrase to keep.
    passphrase: Option<String>,
    /// Lazily resolved Soroban resource-fee schedule, sourced from the
    /// six on-chain `ConfigSetting` entries the first time it's asked
    /// for and reused thereafter.
    fee_configuration: OnceCell<fees::FeeConfiguration>,
    /// Pre-funded deterministic accounts the fork minted at build
    /// time, exposed for test/demo code that wants to use them as
    /// transaction sources. Empty when the user disabled them via
    /// [`ForkConfig::without_test_accounts`].
    test_accounts: Vec<test_accounts::TestAccount>,
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

    /// Advance the ledger sequence and timestamp that the `Env` reports
    /// to contracts.
    ///
    /// **Not block production.** Unlike Anvil's `evm_mine`, this does
    /// not process any pending state — there are no pending transactions
    /// in this model, and no contract code runs as a side effect of the
    /// warp. It only changes what `env.ledger().sequence_number()` and
    /// `env.ledger().timestamp()` return on subsequent reads. Use this
    /// when contract logic conditionally branches on ledger time
    /// (vesting cliffs, auction windows, oracle staleness checks).
    ///
    /// **TTL caveat.** Bumping the sequence past a cached entry's
    /// `live_until_ledger_seq` does not automatically simulate Soroban's
    /// archival/restore flow — soroban-fork does not yet model entry
    /// expiry. Tests that rely on TTL-expiry semantics will see false
    /// positives (the entry stays "live" past its real-mainnet expiry).
    /// Tracking issue: <https://github.com/lobotomoe/soroban-fork/issues>.
    pub fn warp(&self, ledgers: u32, seconds: u64) {
        // Saturating arithmetic: pre-v0.8 these `+=` ops were
        // reachable only from lib-mode test code, where an overflow
        // panic was a fine signal of "your test math is wrong". v0.8
        // wires `warp` through `anvil_mine` over JSON-RPC, so any
        // client can request unbounded advances; saturating keeps
        // wire-driven misuse from panicking the worker thread (which
        // would kill the whole server). The saturated values are
        // already past any meaningful real-Stellar ledger horizon, so
        // tests reaching them are intentionally pathological anyway.
        self.env.ledger().with_mut(|info| {
            info.sequence_number = info.sequence_number.saturating_add(ledgers);
            info.timestamp = info.timestamp.saturating_add(seconds);
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

    /// Network passphrase from the fork-time `getNetwork` call, or `None`
    /// if the user overrode `network_id` (in which case we never queried
    /// the upstream RPC and don't know the passphrase). Used by the
    /// optional JSON-RPC server's `getNetwork` method.
    pub fn passphrase(&self) -> Option<&str> {
        self.passphrase.as_deref()
    }

    /// SHA-256 hash of the network passphrase. Returned by
    /// `getNetwork` as `networkId` after hex encoding.
    pub fn network_id(&self) -> [u8; 32] {
        self.network_id
    }

    /// Protocol version reported by the forked Env.
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// Ledger sequence the forked Env *currently* reports — reads
    /// the live value out of [`Env::ledger`] so any [`Self::warp`] /
    /// [`Self::warp_ledger`] / `anvil_mine` calls are reflected
    /// immediately. At fork build the value matches the upstream
    /// RPC's latest (or [`ForkConfig::at_ledger`]); cheatcodes that
    /// advance time move it forward from there.
    ///
    /// The fork-point sequence (used as cache metadata in
    /// [`Self::save_cache`]) is preserved separately on the
    /// `ledger_sequence` field so cache provenance stays accurate
    /// even after the env has been warped.
    pub fn ledger_sequence(&self) -> u32 {
        self.env.ledger().get().sequence_number
    }

    /// Close-time the forked Env *currently* reports (Unix seconds).
    /// Live reading from [`Env::ledger`] — `anvil_mine` and
    /// [`Self::warp_time`] move it; the fork-point timestamp is
    /// preserved separately for cache provenance.
    pub fn ledger_close_time(&self) -> u64 {
        self.env.ledger().get().timestamp
    }

    /// Direct access to the snapshot source. Useful when the JSON-RPC
    /// server needs to resolve `getLedgerEntries` requests through the
    /// same cache the SDK reads from — exposing it here avoids
    /// duplicating the lazy-fetch logic in the server module.
    pub fn snapshot_source(&self) -> &Rc<RpcSnapshotSource> {
        &self.source
    }

    /// Pre-funded deterministic test accounts the fork minted at
    /// build time. Empty when [`ForkConfig::test_account_count`] was
    /// set to `0`.
    ///
    /// Each carries the secret seed (so test/demo code can sign
    /// envelopes from the account) and the public key. Use
    /// [`test_accounts::TestAccount::account_strkey`] for the
    /// `G...` source-account string JS-SDK clients expect.
    pub fn test_accounts(&self) -> &[test_accounts::TestAccount] {
        &self.test_accounts
    }

    /// The on-chain Soroban resource-fee schedule for this forked
    /// network, resolved lazily on first call.
    ///
    /// Implementation: at first call we fetch the six `ConfigSetting`
    /// entries that compose the schedule (one upstream-RPC round-trip
    /// per uncached key, then served from the snapshot cache forever)
    /// and decode them into a [`fees::FeeConfiguration`]. Subsequent
    /// calls return the same cached reference.
    ///
    /// Used by the JSON-RPC server's `simulateTransaction` to compute
    /// honest `minResourceFee` numbers; library callers can also
    /// invoke this directly for fee-projection tests.
    pub fn fee_configuration(&self) -> Result<&fees::FeeConfiguration> {
        if let Some(cfg) = self.fee_configuration.get() {
            return Ok(cfg);
        }
        let cfg = fees::fetch_fee_configuration(&self.source)?;
        // `set` only fails if another thread (or re-entrant call) already
        // populated the cell. ForkedEnv is `!Send`, so the only way to hit
        // that path is re-entry from inside `fetch_fee_configuration` —
        // and the snapshot source there does not call back into us.
        let _ = self.fee_configuration.set(cfg);
        Ok(self
            .fee_configuration
            .get()
            .expect("fee_configuration just populated"))
    }

    /// Reconstruct the cross-contract call tree from the host's diagnostic
    /// event stream.
    ///
    /// Returns an empty [`Trace`] if [`ForkConfig::tracing`] was not set
    /// to `true` at build time, or if no contract calls have happened
    /// yet.
    ///
    /// **Per-invocation scoping.** The host's `InvocationMeter` clears
    /// the events buffer at the start of every top-level
    /// `invoke_contract`, so each `trace()` reflects only the most
    /// recent top-level call — earlier calls' events are gone. This is
    /// what you usually want for per-test assertions; if you need
    /// history across multiple invocations, capture each `trace()`
    /// before the next call.
    ///
    /// See [`crate::trace`] for the wire-format details and known
    /// caveats (single-`Vec`-arg ambiguity, trapped vs. rolled-back
    /// frames).
    pub fn trace(&self) -> trace::Trace {
        let events = self.diagnostic_events();
        trace::Trace::from_events(&events)
    }

    /// Print the call-tree to stderr in a Foundry-`-vvvv`-style indented
    /// format. Convenience for debug sessions; equivalent to
    /// `eprintln!("{}", env.trace())`.
    pub fn print_trace(&self) {
        eprintln!("{}", self.trace());
    }

    /// Raw access to the host's diagnostic event stream.
    ///
    /// Returns an empty [`soroban_env_host::events::Events`] when tracing
    /// is off or when reading the buffer fails (the failure path is
    /// extraordinarily rare — host event externalisation only fails on
    /// budget exhaustion, which means the test was already broken).
    /// We log the underlying error at `warn!` and hand back an empty
    /// stream rather than panicking, because `trace()` is most often
    /// called from a test that's already failing and a panic here would
    /// hide the original failure.
    pub fn diagnostic_events(&self) -> soroban_env_host::events::Events {
        match self.env.host().get_diagnostic_events() {
            Ok(events) => events,
            Err(e) => {
                warn!("soroban-fork: get_diagnostic_events failed: {e:?}");
                soroban_env_host::events::Events(Vec::new())
            }
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
    tracing: bool,
    /// Number of pre-funded test accounts to mint at build time.
    /// `0` disables them. Default: 10 (Anvil parity).
    test_account_count: usize,
    /// Trustlines to pre-create for each test account against
    /// Stellar-Classic-issued assets. Required so that SAC `transfer`
    /// to a test account succeeds — without a trustline the host
    /// returns `Error(Contract, #13) "trustline missing"`.
    ///
    /// Default targets the **mainnet USDC** issuer (Circle), since
    /// that's what most demos and dapp tests reach for. Forks
    /// pointing at testnet or a custom network should override via
    /// [`ForkConfig::test_account_trustlines`] — the default's
    /// issuer would still preload, but testnet's USDC SAC routes
    /// through a different issuer and would never write to it.
    test_trustlines: Vec<soroban_env_host::xdr::TrustLineAsset>,
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
            tracing: false,
            test_account_count: 10,
            test_trustlines: vec![test_accounts::usdc_mainnet_trustline_asset()],
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

    /// Enable call-tree tracing.
    ///
    /// When `true`, [`Self::build`] flips the host into
    /// [`DiagnosticLevel::Debug`](soroban_env_host::DiagnosticLevel) so
    /// every cross-contract call emits `fn_call`/`fn_return` diagnostic
    /// events. Read the resulting tree via [`ForkedEnv::trace`] or print
    /// it with [`ForkedEnv::print_trace`].
    ///
    /// **Must be set before building the env** — flipping the diagnostic
    /// level after the first invocation does not retroactively capture
    /// earlier calls.
    ///
    /// **Cost:** the host runs in debug mode, which charges a separate
    /// "shadow" budget for diagnostic-event bookkeeping. For typical
    /// integration tests this is negligible; for fuzzing-style workloads
    /// with thousands of invocations, consider leaving tracing off and
    /// flipping it on only for the failing case.
    pub fn tracing(mut self, enabled: bool) -> Self {
        self.tracing = enabled;
        self
    }

    /// Number of pre-funded deterministic test accounts to mint at
    /// fork-build time. Each gets ~100K XLM. Default: 10
    /// (Anvil-equivalent UX). Set to `0` to skip account
    /// pre-population — useful when the only thing you'll do with
    /// the fork is read mainnet state, never construct envelopes.
    ///
    /// The accounts are deterministic: the same seed string
    /// produces the same keypairs across runs and across machines,
    /// so test code can reference them by index without juggling
    /// fixtures. Read them back via [`ForkedEnv::test_accounts`].
    pub fn test_account_count(mut self, count: usize) -> Self {
        self.test_account_count = count;
        self
    }

    /// Replace the list of Classic assets the fork pre-creates
    /// trustlines for, on each test account. Default is
    /// `[mainnet USDC]`.
    ///
    /// **When to override.** Forking a non-mainnet network (testnet,
    /// futurenet, custom) — the default issuer doesn't exist there,
    /// so USDC SAC operations would still fail. Pass the assets
    /// your dapp actually transacts with, with the right issuers
    /// for that network. Pass an empty `Vec` to skip trustline
    /// pre-creation entirely (tests that only use Soroban-native
    /// tokens and Contract-address recipients don't need them).
    ///
    /// Builds a trustline with `flags = AUTHORIZED_FLAG`, `limit =
    /// i64::MAX`, `balance = 0`. Equivalent to the user having run
    /// `ChangeTrust` and the issuer having authorized the trustline
    /// — minus the round-trip through the issuer's KYC.
    pub fn test_account_trustlines(
        mut self,
        assets: Vec<soroban_env_host::xdr::TrustLineAsset>,
    ) -> Self {
        self.test_trustlines = assets;
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

        // Resolve network metadata: explicit override wins; otherwise
        // fetch from the upstream RPC. We keep the passphrase around
        // (for the optional JSON-RPC server's `getNetwork`) when we
        // queried it ourselves; an explicit `network_id` override
        // intentionally has `None` passphrase since we don't know what
        // the caller used.
        let (network_id, passphrase) = match self.network_id {
            Some(id) => (id, None),
            None => {
                let meta = client.get_network()?;
                (meta.network_id, Some(meta.passphrase))
            }
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

        // We always set the diagnostic level explicitly. soroban-sdk's
        // `new_for_testutils` unconditionally turns on Debug mode (so its
        // `auths()` and `events()` hooks always work) — that means our
        // `tracing(false)` would be a silent lie unless we override.
        // `set_diagnostic_level` is the public hook; the InvocationMeter
        // tracks metrics independently of the diagnostic level, so flipping
        // to None here doesn't break the SDK's other test machinery.
        let (level, level_label) = if self.tracing {
            (soroban_env_host::DiagnosticLevel::Debug, "Debug")
        } else {
            (soroban_env_host::DiagnosticLevel::None, "None")
        };
        env.host().set_diagnostic_level(level).map_err(|e| {
            error::ForkError::Host(format!("set_diagnostic_level({level_label}) failed: {e:?}"))
        })?;
        if self.tracing {
            info!("soroban-fork: tracing enabled (DiagnosticLevel::Debug)");
        }

        info!("soroban-fork: forked at ledger {sequence} (protocol {protocol_version})");

        // Pre-mint deterministic test accounts. Doing this AFTER the
        // env is built means the freshly-written Account entries
        // never go through SDK initialisation — they sit in the
        // snapshot source's cache, served on-demand by the next
        // `getLedgerEntries` (the basis of JS-SDK's `getAccount`).
        //
        // Each account also gets a USDC trustline so DEX scenarios
        // (XLM→USDC on Phoenix / Soroswap) work straight away —
        // without one, the SAC fails to credit the account with
        // `Error(Contract, #13) "trustline missing"`. This is the
        // same shape `ChangeTrust` writes on real mainnet, just
        // bootstrapped at fork time. Not a workaround: it's the
        // honest representation of "this test account holds USDC".
        let test_accounts = test_accounts::generate(self.test_account_count);
        if !test_accounts.is_empty() {
            let trustline_count = self.test_trustlines.len();
            let mut preloads: Vec<(
                soroban_env_host::xdr::LedgerKey,
                soroban_env_host::xdr::LedgerEntry,
                Option<u32>,
            )> = Vec::with_capacity(test_accounts.len() * (1 + trustline_count));
            for a in &test_accounts {
                let (key, entry) = a.ledger_entry(sequence);
                preloads.push((key, entry, None));
                for asset in &self.test_trustlines {
                    let (tl_key, tl_entry) = a.trustline_entry(asset.clone(), sequence);
                    preloads.push((tl_key, tl_entry, None));
                }
            }
            source_rc.preload(preloads);
            info!(
                "soroban-fork: minted {} pre-funded test account{} \
                 ({} trustline{} each)",
                test_accounts.len(),
                if test_accounts.len() == 1 { "" } else { "s" },
                trustline_count,
                if trustline_count == 1 { "" } else { "s" }
            );
        }

        Ok(ForkedEnv {
            env,
            source: source_rc,
            cache_path: self.cache_path,
            ledger_sequence: sequence,
            timestamp,
            passphrase,
            network_id,
            protocol_version,
            fee_configuration: OnceCell::new(),
            test_accounts,
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
            .max_protocol_version(25)
            .tracing(true);

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
        assert!(cfg.tracing);
    }

    #[test]
    fn fork_config_tracing_default_is_off() {
        // Tracing has a measurable cost (host runs in debug mode + a
        // separate budget). Default off keeps fast tests fast.
        let cfg = ForkConfig::new("https://example.test");
        assert!(!cfg.tracing);
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
