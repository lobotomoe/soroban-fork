//! # soroban-fork
//!
//! Lazy-loading mainnet/testnet fork for Soroban tests.
//!
//! Think Foundry's Anvil, but for Stellar Soroban. When a test reads a ledger
//! entry that isn't cached, it's fetched from the Soroban RPC on the fly.
//!
//! ```rust,no_run
//! use soroban_fork::ForkConfig;
//!
//! let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
//!     .cache_file("fork_cache.json")
//!     .build();
//!
//! // `env` derefs to `soroban_sdk::Env`, backed by real network state.
//! // State changes are local only -- the network is never modified.
//! // Cache is auto-saved on drop (includes lazy-fetched entries).
//! ```

mod cache;
mod rpc;
mod source;

pub use source::{FetchMode, RpcSnapshotSource};

use soroban_sdk::testutils::{Ledger as _, SnapshotSourceInput};
use soroban_sdk::{Env, IntoVal, Symbol, Val};
use std::path::PathBuf;
use std::rc::Rc;

/// Average Stellar ledger close time in seconds.
const LEDGER_INTERVAL_SECONDS: u64 = 5;

/// SHA-256 of "Test SDF Network ; September 2015"
const TESTNET_NETWORK_ID: [u8; 32] = [
    0xce, 0xe0, 0x30, 0x2d, 0x59, 0x84, 0x4d, 0x32, 0xbd, 0xca, 0x91, 0x5c, 0x82, 0x03, 0xdd, 0x44,
    0xb3, 0x3f, 0xbb, 0x7e, 0xdc, 0x19, 0x05, 0x1e, 0xa3, 0x7a, 0xbe, 0xdf, 0x28, 0xec, 0xd4, 0x72,
];

/// SHA-256 of "Public Global Stellar Network ; September 2015"
const MAINNET_NETWORK_ID: [u8; 32] = [
    0x7a, 0xc3, 0x39, 0x97, 0x54, 0x4e, 0x31, 0x75, 0xd2, 0x66, 0xbd, 0x02, 0x24, 0x39, 0xb2, 0x2c,
    0xdb, 0x16, 0x50, 0x8c, 0x01, 0x16, 0x3f, 0x26, 0xe5, 0xcb, 0x2a, 0x3e, 0x10, 0x45, 0xa9, 0x79,
];

// ---------------------------------------------------------------------------
// ForkedEnv — wrapper that auto-saves cache on drop
// ---------------------------------------------------------------------------

/// A forked Soroban environment backed by real network state.
///
/// Derefs to `soroban_sdk::Env` so all SDK methods work transparently.
/// When dropped, any lazy-fetched entries are persisted to the cache file.
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
    /// Get a reference to the underlying `Env`.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Number of RPC calls made since the fork was created.
    pub fn fetch_count(&self) -> u32 {
        self.source.fetch_count()
    }

    /// Advance both ledger sequence and timestamp.
    ///
    /// This is the Soroban equivalent of Anvil's `evm_increaseTime` + `evm_mine`.
    /// Use it to test time-dependent logic: upgrade timelocks, TTL expiry,
    /// interest accrual, bridged balance staleness, etc.
    pub fn warp(&self, ledgers: u32, seconds: u64) {
        self.env.ledger().with_mut(|info| {
            info.sequence_number += ledgers;
            info.timestamp += seconds;
        });
    }

    /// Advance time by the given seconds.
    /// Ledger sequence advances proportionally (~5 seconds per ledger on Stellar).
    pub fn warp_time(&self, seconds: u64) {
        let ledgers = (seconds / LEDGER_INTERVAL_SECONDS) as u32;
        self.warp(ledgers, seconds);
    }

    /// Advance by the given number of ledgers.
    /// Timestamp advances proportionally (~5 seconds per ledger).
    pub fn warp_ledger(&self, ledgers: u32) {
        let seconds = ledgers as u64 * LEDGER_INTERVAL_SECONDS;
        self.warp(ledgers, seconds);
    }

    /// Set a token balance to an exact amount (Soroban equivalent of Foundry's `deal()`).
    ///
    /// Works by calculating the delta and calling mint (if increasing) or burn
    /// (if decreasing) on the token contract. Requires `mock_all_auths()`.
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
        let abs_delta: Val = delta.unsigned_abs().into_val(e);

        if delta > 0 {
            let delta_val: Val = delta.into_val(e);
            let mut args = soroban_sdk::Vec::new(e);
            args.push_back(target_val);
            args.push_back(delta_val);
            e.invoke_contract::<()>(token, &Symbol::new(e, "mint"), args);
        } else {
            let mut args = soroban_sdk::Vec::new(e);
            args.push_back(target_val);
            args.push_back(abs_delta);
            e.invoke_contract::<()>(token, &Symbol::new(e, "burn"), args);
        }
    }

    /// Explicitly save the cache to disk.
    /// Called automatically on drop, but can be called manually for safety.
    pub fn save_cache(&self) -> Result<(), String> {
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
        if let Some(path) = &self.cache_path {
            if let Err(e) = do_save_cache(
                &self.source,
                path,
                self.ledger_sequence,
                self.timestamp,
                self.network_id,
                self.protocol_version,
            ) {
                eprintln!("[soroban-fork] cache save error on drop: {e}");
            } else {
                let count = self.source.entries().len();
                eprintln!("[soroban-fork] saved {count} entries to {}", path.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ForkConfig — builder
// ---------------------------------------------------------------------------

/// Builder for creating a forked Soroban environment.
pub struct ForkConfig {
    rpc_url: String,
    cache_path: Option<PathBuf>,
    network_id: Option<[u8; 32]>,
    fetch_mode: Option<FetchMode>,
    pinned_ledger: Option<u32>,
    max_protocol_version: Option<u32>,
}

impl ForkConfig {
    /// Create a new fork config pointing at a Soroban RPC.
    ///
    /// Common endpoints:
    /// - Testnet: `https://soroban-testnet.stellar.org:443`
    /// - Mainnet: `https://soroban-rpc.mainnet.stellar.gateway.fm`
    pub fn new(rpc_url: &str) -> Self {
        // Auto-detect network ID from URL
        let network_id = if rpc_url.contains("testnet") {
            Some(TESTNET_NETWORK_ID)
        } else if rpc_url.contains("mainnet") {
            Some(MAINNET_NETWORK_ID)
        } else {
            None
        };

        Self {
            rpc_url: rpc_url.to_string(),
            cache_path: None,
            network_id,
            fetch_mode: None,
            pinned_ledger: None,
            max_protocol_version: None,
        }
    }

    /// Path to a JSON file for persisting fetched entries.
    /// If the file exists, entries are pre-loaded into the cache.
    /// On drop, all entries (including lazy-fetched) are written back.
    pub fn cache_file(mut self, path: &str) -> Self {
        self.cache_path = Some(PathBuf::from(path));
        self
    }

    /// Override the network ID (SHA-256 of network passphrase).
    pub fn network_id(mut self, id: [u8; 32]) -> Self {
        self.network_id = Some(id);
        self
    }

    /// Set the fetch mode.
    /// - `Strict` (default): panic on RPC errors. Best for tests.
    /// - `Lenient`: log errors and return None. For partial-state scenarios.
    pub fn fetch_mode(mut self, mode: FetchMode) -> Self {
        self.fetch_mode = Some(mode);
        self
    }

    /// Pin to a specific ledger sequence for reproducible tests.
    ///
    /// Note: the RPC always returns entries at the latest ledger. This override
    /// sets the Env's ledger sequence/timestamp so contract logic sees the
    /// pinned value. For full state reproducibility, use `cache_file()`.
    pub fn at_ledger(mut self, sequence: u32) -> Self {
        self.pinned_ledger = Some(sequence);
        self
    }

    /// Cap the protocol version (e.g., 25 to avoid "protocol too new" errors
    /// when the network has upgraded but your SDK hasn't).
    pub fn max_protocol_version(mut self, version: u32) -> Self {
        self.max_protocol_version = Some(version);
        self
    }

    /// Build the forked environment.
    ///
    /// 1. Creates `RpcSnapshotSource` (lazy RPC fallback on cache miss)
    /// 2. Pre-loads entries from cache file if it exists
    /// 3. Fetches current ledger info from RPC
    /// 4. Returns a `ForkedEnv` that derefs to `soroban_sdk::Env`
    pub fn build(self) -> ForkedEnv {
        let source = RpcSnapshotSource::new(self.rpc_url.clone());
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
                        eprintln!(
                            "[soroban-fork] pre-loaded {count} entries from {}",
                            path.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("[soroban-fork] cache load error (starting fresh): {e}");
                    }
                }
            }
        }

        // Fetch current ledger info
        let client = reqwest::blocking::Client::new();
        let ledger_info = rpc::get_latest_ledger(&client, &self.rpc_url)
            .expect("[soroban-fork] failed to fetch latest ledger from RPC");

        let network_id = self.network_id.unwrap_or(ledger_info.network_id);

        // Apply overrides
        let sequence = self.pinned_ledger.unwrap_or(ledger_info.sequence);
        let protocol_version = match self.max_protocol_version {
            Some(max) if ledger_info.protocol_version > max => {
                eprintln!(
                    "[soroban-fork] capping protocol version {} -> {} (max_protocol_version)",
                    ledger_info.protocol_version, max
                );
                max
            }
            _ => ledger_info.protocol_version,
        };

        let sdk_ledger_info = soroban_env_host::LedgerInfo {
            protocol_version,
            sequence_number: sequence,
            timestamp: ledger_info.timestamp,
            network_id,
            base_reserve: 100,
            min_persistent_entry_ttl: 4096,
            min_temp_entry_ttl: 16,
            max_entry_ttl: 6_312_000,
        };

        let source_rc = Rc::new(source);

        let input = SnapshotSourceInput {
            source: source_rc.clone(),
            ledger_info: Some(sdk_ledger_info),
            snapshot: None,
        };

        let env = Env::from_ledger_snapshot(input);

        eprintln!(
            "[soroban-fork] forked at ledger {} (protocol {})",
            sequence, protocol_version
        );

        ForkedEnv {
            env,
            source: source_rc,
            cache_path: self.cache_path,
            ledger_sequence: sequence,
            timestamp: ledger_info.timestamp,
            network_id,
            protocol_version,
        }
    }
}

fn do_save_cache(
    source: &RpcSnapshotSource,
    path: &std::path::Path,
    sequence: u32,
    timestamp: u64,
    network_id: [u8; 32],
    protocol_version: u32,
) -> Result<(), String> {
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
// Integration tests (hit real testnet RPC)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Ledger;
    use soroban_sdk::{Address, String as SorobanString, Symbol};

    const TESTNET_RPC: &str = "https://soroban-testnet.stellar.org:443";

    /// Our deployed vault on testnet (v3).
    const VAULT_ID: &str = "CCN4JVLCHZHYNRDY5VA7B26FWDI7G6DPXJTHFIUGHNUCD4T3SDTKWAR5";

    #[test]
    fn test_fork_connects_to_testnet() {
        let env = ForkConfig::new(TESTNET_RPC)
            .max_protocol_version(25)
            .build();
        let info = env.ledger().get();
        assert!(info.sequence_number > 0);
        assert_eq!(info.network_id, TESTNET_NETWORK_ID);
        assert_eq!(env.fetch_count(), 0);
        eprintln!("[test] forked at ledger {}", info.sequence_number);
    }

    #[test]
    fn test_fork_with_snapshot_file() {
        let snapshot_path = "/tmp/vault_snapshot.json";
        if !std::path::Path::new(snapshot_path).exists() {
            eprintln!(
                "[test] skipping: {snapshot_path} not found. \
                 Run `stellar snapshot create` first."
            );
            return;
        }

        let env = ForkConfig::new(TESTNET_RPC)
            .cache_file(snapshot_path)
            .max_protocol_version(25)
            .build();
        env.mock_all_auths();

        let vault_addr = Address::from_string(&SorobanString::from_str(&env, VAULT_ID));

        let admin: Address = env.invoke_contract(
            &vault_addr,
            &Symbol::new(&env, "admin"),
            soroban_sdk::vec![&env],
        );
        eprintln!("[test] vault admin = {:?}", admin);
        eprintln!("[test] RPC fetches: {}", env.fetch_count());
    }

    /// Dogfooding: invoke contract methods on the real testnet vault
    /// via pure lazy fetch (no snapshot file).
    /// Each ledger entry is fetched from the RPC on demand.
    #[test]
    fn test_lazy_invoke_vault() {
        let env = ForkConfig::new(TESTNET_RPC)
            .max_protocol_version(25)
            .build();
        env.mock_all_auths();

        let vault_addr = Address::from_string(&SorobanString::from_str(&env, VAULT_ID));

        // name() reads from instance storage -> triggers lazy fetch of:
        //   1. ContractData(instance) -> gets WASM hash
        //   2. ContractCode(wasm)     -> gets the WASM binary
        //   3. Storage entry for name
        let name: soroban_sdk::String = env.invoke_contract(
            &vault_addr,
            &Symbol::new(&env, "name"),
            soroban_sdk::vec![&env],
        );
        eprintln!("[test] vault name = {:?}", name);

        let symbol: soroban_sdk::String = env.invoke_contract(
            &vault_addr,
            &Symbol::new(&env, "symbol"),
            soroban_sdk::vec![&env],
        );
        eprintln!("[test] vault symbol = {:?}", symbol);

        let total_supply: i128 = env.invoke_contract(
            &vault_addr,
            &Symbol::new(&env, "total_supply"),
            soroban_sdk::vec![&env],
        );
        eprintln!("[test] vault total_supply = {total_supply}");
        assert!(total_supply >= 0);

        eprintln!("[test] RPC fetches: {}", env.fetch_count());
    }

    #[test]
    fn test_warp_time() {
        let env = ForkConfig::new(TESTNET_RPC)
            .max_protocol_version(25)
            .build();
        let before = env.ledger().get();

        // Warp 24 hours
        env.warp_time(86_400);

        let after = env.ledger().get();
        assert_eq!(after.timestamp, before.timestamp + 86_400);
        assert_eq!(
            after.sequence_number,
            before.sequence_number + 86_400 / 5 // ~17,280 ledgers
        );
        eprintln!(
            "[test] warped: ledger {} -> {}, time +24h",
            before.sequence_number, after.sequence_number
        );

        // Warp 100 ledgers
        env.warp_ledger(100);
        let final_info = env.ledger().get();
        assert_eq!(final_info.sequence_number, after.sequence_number + 100);
        assert_eq!(final_info.timestamp, after.timestamp + 500); // 100 * 5s
        eprintln!(
            "[test] warped: ledger {} -> {}, time +500s",
            after.sequence_number, final_info.sequence_number
        );
    }
}
