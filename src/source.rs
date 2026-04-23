//! `SnapshotSource` implementation that fetches ledger entries on demand.
//!
//! The VM asks for a ledger entry via [`SnapshotSource::get`]; we check the
//! in-memory cache first and, on miss, defer to an [`RpcClient`]. Results
//! (including confirmed-missing entries) are memoized so the second lookup
//! of the same key is always local.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use log::{info, warn};
use soroban_env_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_env_host::xdr::{
    ContractDataDurability, LedgerEntry, LedgerKey, PublicKey, ScAddress, ScVal,
};
use soroban_env_host::HostError;

use crate::rpc::RpcClient;

/// Policy for how the source reacts to transport failures inside the VM loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchMode {
    /// Surface the error as a panic. Appropriate for tests where a missing
    /// entry is always a real bug — you want to see the failing key.
    Strict,
    /// Log the error and return `None`. Useful when you're OK with the VM
    /// observing a non-existent entry if the RPC is flaky, e.g. when
    /// probing endpoints you don't strictly require.
    Lenient,
}

/// A [`SnapshotSource`] backed by a Soroban RPC + local cache.
///
/// Cache semantics:
/// - `Some(Some(entry))` → we've seen it, entry exists.
/// - `Some(None)` → we've asked, RPC said the entry doesn't exist. Negative
///   cache — stops us re-asking for keys we know are absent.
/// - `None` → we haven't asked yet.
pub struct RpcSnapshotSource {
    cache: RefCell<BTreeMap<LedgerKey, Option<EntryWithLiveUntil>>>,
    client: Arc<RpcClient>,
    fetch_count: RefCell<u32>,
    fetch_mode: FetchMode,
}

impl RpcSnapshotSource {
    /// Wrap the given RPC client. `Arc` so the source can be cloned cheaply
    /// and shared with other harnesses (e.g. a pre-warmer).
    pub fn new(client: Arc<RpcClient>) -> Self {
        Self {
            cache: RefCell::new(BTreeMap::new()),
            client,
            fetch_count: RefCell::new(0),
            fetch_mode: FetchMode::Strict,
        }
    }

    /// Set the fetch mode. Builder-style — consumes `self` and returns it.
    pub fn with_fetch_mode(mut self, mode: FetchMode) -> Self {
        self.fetch_mode = mode;
        self
    }

    /// Pre-populate the cache from a snapshot file. Entries loaded this
    /// way do not count towards `fetch_count`.
    pub fn preload(
        &self,
        entries: impl IntoIterator<Item = (LedgerKey, LedgerEntry, Option<u32>)>,
    ) {
        let mut cache = self.cache.borrow_mut();
        for (key, entry, live_until) in entries {
            cache.insert(key, Some((Rc::new(entry), live_until)));
        }
    }

    /// How many RPC round-trips were served through this source since
    /// creation. Useful for asserting cache hit-rates in tests.
    pub fn fetch_count(&self) -> u32 {
        *self.fetch_count.borrow()
    }

    /// Export the cache for persistence. Negative-cache entries (confirmed
    /// missing) are intentionally omitted — they aren't useful across
    /// processes and bloat the on-disk snapshot.
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

        info!("soroban-fork: fetch #{count}: {}", key_display(key));

        match self.client.fetch_entry(key) {
            Ok(Some(fetched)) => Some((Rc::new(fetched.entry), fetched.live_until)),
            Ok(None) => {
                info!("soroban-fork: fetch #{count}: not found on ledger");
                None
            }
            Err(e) => match self.fetch_mode {
                FetchMode::Strict => {
                    // Panicking inside `SnapshotSource::get` would be caught
                    // and reformatted by the host; surfacing via panic here
                    // keeps the error readable in standard test output.
                    panic!("soroban-fork: RPC fetch #{count} failed (strict): {e}")
                }
                FetchMode::Lenient => {
                    warn!("soroban-fork: RPC fetch #{count} error (lenient): {e}");
                    None
                }
            },
        }
    }
}

impl SnapshotSource for RpcSnapshotSource {
    fn get(
        &self,
        key: &Rc<LedgerKey>,
    ) -> std::result::Result<Option<EntryWithLiveUntil>, HostError> {
        if let Some(cached) = self.cache.borrow().get(key.as_ref()) {
            return Ok(cached.clone());
        }
        let entry = self.fetch_from_rpc(key.as_ref());
        self.cache
            .borrow_mut()
            .insert(key.as_ref().clone(), entry.clone());
        Ok(entry)
    }
}

// ---------------------------------------------------------------------------
// Diagnostic formatting helpers — kept private but exercised by unit tests.
// ---------------------------------------------------------------------------

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
            format!("Account({})", account_id_short(&a.account_id))
        }
        LedgerKey::Trustline(t) => {
            format!("Trustline({})", account_id_short(&t.account_id))
        }
        LedgerKey::ConfigSetting(_) => "ConfigSetting".to_string(),
        LedgerKey::Ttl(_) => "Ttl".to_string(),
        _ => "Other".to_string(),
    }
}

fn sc_address_short(addr: &ScAddress) -> String {
    // `stellar_strkey`'s `Display` impl writes to `fmt::Formatter`, returning
    // `std::String` via `format!`. Calling `.to_string()` directly on some
    // of these types picks up a `heapless::String<N>` via the `SerdeSeq`
    // surface — we always want the heap-allocating std one.
    let full = match addr {
        ScAddress::Contract(hash) => {
            format!("{}", stellar_strkey::Contract(hash.0.clone().into()))
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
    let PublicKey::PublicKeyTypeEd25519(k) = &id.0;
    format!("{}", stellar_strkey::ed25519::PublicKey(k.0))
}

fn abbreviate(s: &str) -> String {
    if s.len() > 12 {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests — pure, no network required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host::xdr::{ConfigSettingId, LedgerKeyConfigSetting};

    #[test]
    fn abbreviate_short_string_is_unchanged() {
        assert_eq!(abbreviate("abc"), "abc");
        assert_eq!(abbreviate("12345678"), "12345678");
    }

    #[test]
    fn abbreviate_long_string_collapses_middle() {
        // 56-char Stellar address shape — first 4 + last 4 chars, with "..."
        // between. Keep as a concrete-value assertion rather than a shape
        // assertion: this is exactly what humans see in the logs, so an
        // accidental format change should fail loudly here.
        let full = "GABCDEFGHIJKLMNOPQRSTUVWXYZ01234567890ABCDEFGHIJKLMNOPQR";
        let short = abbreviate(full);
        assert_eq!(short, "GABC...OPQR");
        assert!(short.len() < full.len());
    }

    #[test]
    fn key_display_renders_config_setting() {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractMaxSizeBytes,
        });
        assert_eq!(key_display(&key), "ConfigSetting");
    }

    #[test]
    fn fetch_mode_default_is_strict() {
        // Documented contract: new sources start in Strict until explicitly
        // opted down. Keep this test as a guard against silent regressions.
        let client = Arc::new(
            RpcClient::new("http://localhost:0", crate::rpc::RpcConfig::default())
                .expect("client construction should not fail"),
        );
        let src = RpcSnapshotSource::new(client);
        assert_eq!(src.fetch_mode, FetchMode::Strict);
    }
}
