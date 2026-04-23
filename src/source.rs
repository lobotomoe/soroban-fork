use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use soroban_env_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_env_host::xdr::{
    ContractDataDurability, LedgerEntry, LedgerKey, PublicKey, ScAddress, ScVal,
};
use soroban_env_host::HostError;

use crate::rpc;

/// Controls behavior on RPC errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchMode {
    /// Panic on RPC errors. Best for tests where a missing entry is a real bug.
    Strict,
    /// Log errors and return `None`. Useful when partial state is acceptable.
    Lenient,
}

/// A `SnapshotSource` that lazily fetches ledger entries from a Soroban RPC
/// on cache miss. Entries are cached in memory and optionally persisted to disk.
///
/// This is the core of soroban-fork: it makes any Soroban `Env` backed by
/// real network state, fetched on-demand.
pub struct RpcSnapshotSource {
    /// In-memory cache. `Some(entry)` = exists, `None` = confirmed nonexistent.
    cache: RefCell<BTreeMap<LedgerKey, Option<EntryWithLiveUntil>>>,
    /// HTTP client (reused for connection pooling).
    client: reqwest::blocking::Client,
    /// Soroban RPC endpoint URL.
    rpc_url: String,
    /// Counter of RPC fetches (for diagnostics).
    fetch_count: RefCell<u32>,
    /// Error handling mode.
    fetch_mode: FetchMode,
}

impl RpcSnapshotSource {
    pub fn new(rpc_url: String) -> Self {
        Self {
            cache: RefCell::new(BTreeMap::new()),
            client: reqwest::blocking::Client::new(),
            rpc_url,
            fetch_count: RefCell::new(0),
            fetch_mode: FetchMode::Strict,
        }
    }

    /// Set the fetch mode (strict or lenient). Builder-style.
    pub fn with_fetch_mode(mut self, mode: FetchMode) -> Self {
        self.fetch_mode = mode;
        self
    }

    /// Pre-populate the cache from a list of entries (e.g., from a snapshot file).
    pub fn preload(
        &self,
        entries: impl IntoIterator<Item = (LedgerKey, LedgerEntry, Option<u32>)>,
    ) {
        let mut cache = self.cache.borrow_mut();
        for (key, entry, live_until) in entries {
            cache.insert(key, Some((Rc::new(entry), live_until)));
        }
    }

    /// Number of RPC fetches made so far.
    pub fn fetch_count(&self) -> u32 {
        *self.fetch_count.borrow()
    }

    /// Export all cached entries for persistence.
    pub fn entries(&self) -> Vec<(LedgerKey, LedgerEntry, Option<u32>)> {
        self.cache
            .borrow()
            .iter()
            .filter_map(|(key, val)| {
                val.as_ref()
                    .map(|(entry, live_until)| (key.clone(), entry.as_ref().clone(), *live_until))
            })
            .collect()
    }

    fn fetch_from_rpc(&self, key: &LedgerKey) -> Option<EntryWithLiveUntil> {
        let count = {
            let mut c = self.fetch_count.borrow_mut();
            *c += 1;
            *c
        };

        eprintln!("[soroban-fork] fetch #{count}: {}", key_display(key));

        match rpc::fetch_entry(&self.client, &self.rpc_url, key) {
            Ok(Some(fetched)) => Some((Rc::new(fetched.entry), fetched.live_until)),
            Ok(None) => {
                eprintln!("[soroban-fork] fetch #{count}: not found on ledger");
                None
            }
            Err(e) => match self.fetch_mode {
                FetchMode::Strict => {
                    panic!("[soroban-fork] RPC fetch #{count} failed: {e}")
                }
                FetchMode::Lenient => {
                    eprintln!("[soroban-fork] RPC fetch #{count} error (lenient): {e}");
                    None
                }
            },
        }
    }
}

impl SnapshotSource for RpcSnapshotSource {
    fn get(&self, key: &Rc<LedgerKey>) -> Result<Option<EntryWithLiveUntil>, HostError> {
        // Check cache first
        if let Some(cached) = self.cache.borrow().get(key.as_ref()) {
            return Ok(cached.clone());
        }

        // Cache miss -> fetch from RPC
        let entry = self.fetch_from_rpc(key.as_ref());

        // Cache the result (including None for confirmed nonexistent)
        self.cache
            .borrow_mut()
            .insert(key.as_ref().clone(), entry.clone());

        Ok(entry)
    }
}

/// Human-readable description of a ledger key for diagnostic logging.
fn key_display(key: &LedgerKey) -> String {
    match key {
        LedgerKey::ContractData(cd) => {
            let addr = sc_address_short(&cd.contract);
            if cd.key == ScVal::LedgerKeyContractInstance {
                format!("ContractData({addr}, instance)")
            } else {
                let dur = match cd.durability {
                    ContractDataDurability::Temporary => "temp",
                    ContractDataDurability::Persistent => "persistent",
                };
                format!("ContractData({addr}, {dur})")
            }
        }
        LedgerKey::ContractCode(cc) => {
            let h = &cc.hash.0;
            format!(
                "ContractCode({:02x}{:02x}{:02x}{:02x}...)",
                h[0], h[1], h[2], h[3]
            )
        }
        LedgerKey::Account(a) => {
            let addr = account_id_short(&a.account_id);
            format!("Account({addr})")
        }
        LedgerKey::Trustline(t) => {
            let addr = account_id_short(&t.account_id);
            format!("Trustline({addr})")
        }
        LedgerKey::ConfigSetting(_) => "ConfigSetting".to_string(),
        LedgerKey::Ttl(_) => "Ttl".to_string(),
        _ => "Other".to_string(),
    }
}

/// Short display for ScAddress (first 4 + last 4 chars of strkey).
fn sc_address_short(addr: &ScAddress) -> String {
    let full = match addr {
        ScAddress::Contract(hash) => {
            let s = stellar_strkey::Contract(hash.0.clone().into());
            format!("{s}")
        }
        ScAddress::Account(id) => account_id_full(id),
        _ => "???".to_string(),
    };
    abbreviate(&full)
}

fn account_id_short(id: &soroban_env_host::xdr::AccountId) -> String {
    abbreviate(&account_id_full(id))
}

fn account_id_full(id: &soroban_env_host::xdr::AccountId) -> String {
    match &id.0 {
        PublicKey::PublicKeyTypeEd25519(k) => {
            let s = stellar_strkey::ed25519::PublicKey(k.0);
            format!("{s}")
        }
    }
}

fn abbreviate(s: &str) -> String {
    if s.len() > 12 {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}
