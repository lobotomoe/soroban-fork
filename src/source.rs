//! `SnapshotSource` implementation that fetches ledger entries on demand.
//!
//! The VM asks for a ledger entry via [`SnapshotSource::get`]; we check the
//! in-memory cache first and, on miss, defer to an [`RpcClient`]. Results
//! (including confirmed-missing entries) are memoized so the second lookup
//! of the same key is always local.
//!
//! # Thread-safety & internal representation
//!
//! Cached entries live as **XDR-encoded bytes** in a `Mutex<BTreeMap>`,
//! decoded back into a fresh `Rc<LedgerEntry>` only when the SDK's
//! [`SnapshotSource::get`] hands one out across the trait boundary.
//! Storing bytes (rather than `Rc<LedgerEntry>` directly) is what lets the
//! struct be `Send + Sync`: `Rc` is intentionally single-threaded, so any
//! `Rc` inside a shared field would taint the whole type. With bytes, the
//! shared cache is fully thread-safe; the per-call `Rc` is created on the
//! consumer's thread inside `get` and never crosses a boundary.
//!
//! Decode cost on a cache hit is the XDR-parse of one `LedgerEntry`
//! (microseconds for typical entries). The cost is paid on every call to
//! `get`, including hits — if profiling ever shows this on a hot path, a
//! per-thread parsed-entry memoization layer can be added without changing
//! the public API.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use log::{info, warn};
use soroban_env_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_env_host::xdr::{
    AccountId, ContractDataDurability, LedgerEntry, LedgerEntryData, LedgerKey, LedgerKeyAccount,
    Limits, PublicKey, ReadXdr, ScAddress, ScVal, SequenceNumber, WriteXdr,
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

/// Internal cache value: XDR-encoded `LedgerEntry` bytes plus the
/// `live_until` ledger hint from the RPC. Storing bytes (not a parsed
/// `LedgerEntry` wrapped in `Rc`) is what gives the source its
/// `Send + Sync` guarantee — see the module-level docs.
type CachedBytes = (Vec<u8>, Option<u32>);

/// A [`SnapshotSource`] backed by a Soroban RPC + local cache.
///
/// Cache semantics:
/// - `Some(Some(entry))` → we've seen it, entry exists.
/// - `Some(None)` → we've asked, RPC said the entry doesn't exist. Negative
///   cache — stops us re-asking for keys we know are absent.
/// - `None` → we haven't asked yet.
///
/// Thread-safety: this type is `Send + Sync`. Wrap it in [`Arc`] to share
/// across threads — for example, between a future RPC-server worker pool
/// and the `Env` instances handed to each request. The SDK's
/// `SnapshotSource` trait still expects an [`Rc`] at its boundary, so a
/// fresh `Rc<LedgerEntry>` is built per `get` call from cached bytes; the
/// `Rc` never escapes its caller's thread.
pub struct RpcSnapshotSource {
    cache: Mutex<BTreeMap<LedgerKey, Option<CachedBytes>>>,
    client: Arc<RpcClient>,
    fetch_count: AtomicU32,
    fetch_mode: FetchMode,
}

// Compile-time guarantee that the source is `Send + Sync`. Living at
// module scope (not inside `cfg(test)`) means a future change that
// reintroduces `Rc`/`RefCell` breaks `cargo build`, not just `cargo test`
// — the safety net runs in every developer's local edit cycle.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<RpcSnapshotSource>();
    assert_sync::<RpcSnapshotSource>();
};

impl RpcSnapshotSource {
    /// Wrap the given RPC client. `Arc` so the source can be cloned cheaply
    /// and shared with other harnesses (e.g. a pre-warmer).
    pub fn new(client: Arc<RpcClient>) -> Self {
        Self {
            cache: Mutex::new(BTreeMap::new()),
            client,
            fetch_count: AtomicU32::new(0),
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
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        for (key, entry, live_until) in entries {
            cache.insert(key, Some((encode_entry(&entry), live_until)));
        }
    }

    /// How many RPC fetches this source has *attempted* since creation
    /// (counts both successful and failed attempts — the counter
    /// increments before the network call, so a connect timeout still
    /// shows up here). Useful for asserting cache hit-rates in tests.
    pub fn fetch_count(&self) -> u32 {
        self.fetch_count.load(Ordering::Relaxed)
    }

    /// Force-write a single `LedgerEntry` into the cache, replacing
    /// whatever was there (or creating a fresh entry if the key was
    /// absent). Powers the JSON-RPC `anvil_setLedgerEntry` cheatcode
    /// — clients hand us an XDR-encoded entry, we trust them and
    /// install it. Subsequent reads (including via the host's
    /// recording-mode storage in `simulateTransaction` /
    /// `sendTransaction`) see the new entry.
    ///
    /// This is the load-bearing primitive for Anvil-style stress
    /// testing: any state mutation Anvil's `setStorageAt` /
    /// `setBalance` / `setCode` cheatcodes do is just an entry
    /// write, and Stellar's storage model maps cleanly to one
    /// LedgerEntry-per-key. Higher-level wrappers (`setBalance`,
    /// `setCode`, etc.) compose on top.
    ///
    /// `live_until` carries forward an optional TTL hint — pass
    /// `None` for entries that don't have one (Account, Trustline)
    /// or when the test doesn't care about expiry.
    pub fn set_entry(&self, key: LedgerKey, entry: LedgerEntry, live_until: Option<u32>) {
        let bytes = encode_entry(&entry);
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        cache.insert(key, Some((bytes, live_until)));
    }

    /// Bump the `seq_num` of an Account ledger entry that's already
    /// in the cache. Returns the new sequence on success, `None` if
    /// the account isn't cached or the cached entry isn't an
    /// `AccountEntry` (so the caller can decide whether absence is
    /// an error or just "first send from a never-touched account").
    ///
    /// Stellar's transaction validation expects `tx.seq_num ==
    /// account.seq_num + 1` and post-success leaves the account at
    /// `tx.seq_num`. The fork's trust mode skips the pre-check but
    /// still must increment so the *next* envelope a JS-SDK client
    /// builds (via `getAccount` → `tx.seq_num + 1`) lines up with
    /// what the host expects.
    pub fn bump_account_seq(&self, account_id: &AccountId) -> Option<i64> {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        let cached = cache.get_mut(&key)?;
        let bytes_and_ttl = cached.as_mut()?;
        let bytes = &bytes_and_ttl.0;
        let mut entry = LedgerEntry::from_xdr(bytes, Limits::none()).ok()?;
        let new_seq = match &mut entry.data {
            LedgerEntryData::Account(account) => {
                let SequenceNumber(current) = account.seq_num;
                let next = current.wrapping_add(1);
                account.seq_num = SequenceNumber(next);
                next
            }
            _ => return None,
        };
        let new_bytes = entry.to_xdr(Limits::none()).ok()?;
        bytes_and_ttl.0 = new_bytes;
        Some(new_seq)
    }

    /// Apply a batch of `LedgerEntryChange`s back to the cache so that
    /// subsequent reads see the writes. Powers the JSON-RPC server's
    /// `sendTransaction` — recording-mode invocation gives us a list
    /// of changes; we walk them and update the cached bytes.
    ///
    /// Semantics per change:
    /// - `read_only == true` → ignored. The host won't have produced
    ///   a `new_value` and TTL bumps don't change entry contents.
    /// - `encoded_new_value == Some(bytes)` → overwrite the cached
    ///   entry. Existing live-until is kept (TTL changes are tracked
    ///   separately by the host but not yet plumbed here).
    /// - `encoded_new_value == None` (read-write) → entry was
    ///   removed; flip the cache to the negative-cache `None` marker
    ///   so subsequent `get`s see absence locally without re-asking
    ///   upstream RPC.
    ///
    /// `key` and `entry` decode panics in this method are the same
    /// "structural bug, not recoverable" class as elsewhere in this
    /// file: bytes came directly from the host that just produced
    /// them, so a decode failure means the host violated its own
    /// XDR-shape invariant.
    pub fn apply_changes<I>(&self, changes: I) -> u32
    where
        I: IntoIterator<Item = soroban_env_host::e2e_invoke::LedgerEntryChange>,
    {
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        let mut applied: u32 = 0;
        for change in changes {
            if change.read_only {
                continue;
            }
            let key = LedgerKey::from_xdr(&change.encoded_key, Limits::none())
                .unwrap_or_else(|e| panic!("apply_changes: bad LedgerKey from host: {e}"));
            match change.encoded_new_value {
                Some(bytes) => {
                    let live_until = change.ttl_change.as_ref().map(|t| t.new_live_until_ledger);
                    cache.insert(key, Some((bytes, live_until)));
                }
                None => {
                    cache.insert(key, None);
                }
            }
            applied = applied.saturating_add(1);
        }
        applied
    }

    /// Export the cache for persistence. Negative-cache entries (confirmed
    /// missing) are intentionally omitted — they aren't useful across
    /// processes and bloat the on-disk snapshot.
    ///
    /// The decode step runs **outside** the cache lock: we snapshot raw
    /// bytes under the lock, release it, then parse. With a 10k-entry
    /// fork this avoids blocking concurrent `get` and `fetch` calls for
    /// the duration of a few-ms parse loop.
    pub fn entries(&self) -> Vec<(LedgerKey, LedgerEntry, Option<u32>)> {
        let raw: Vec<(LedgerKey, Vec<u8>, Option<u32>)> = {
            let cache = self.cache.lock().expect("cache mutex poisoned");
            cache
                .iter()
                .filter_map(|(key, val)| {
                    val.as_ref()
                        .map(|(bytes, live_until)| (key.clone(), bytes.clone(), *live_until))
                })
                .collect()
        };
        raw.into_iter()
            .map(|(key, bytes, live_until)| (key, decode_entry(&bytes), live_until))
            .collect()
    }

    /// Issue an RPC fetch and memoize whatever we get — including a
    /// `None` on Lenient errors, matching the original RefCell-era
    /// behavior. Caching the negative result on a Lenient error means
    /// later `get`s return `None` immediately without retrying; users
    /// who want retries should rebuild the env (the cache is per-Source).
    fn fetch_from_rpc(&self, key: &LedgerKey) -> Option<EntryWithLiveUntil> {
        // `fetch_add` returns the prior value; `+ 1` gives a 1-based
        // monotonic fetch number that survives concurrent racers.
        let count = self.fetch_count.fetch_add(1, Ordering::Relaxed) + 1;

        info!("soroban-fork: fetch #{count}: {}", key_display(key));

        let result: Option<EntryWithLiveUntil> = match self.client.fetch_entry(key) {
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
        };

        // Encode the positive case into bytes for shared storage; persist
        // negative case as `None` so re-asks for absent keys are local.
        let cached = result
            .as_ref()
            .map(|(rc, live_until)| (encode_entry(rc.as_ref()), *live_until));
        self.cache
            .lock()
            .expect("cache mutex poisoned")
            .insert(key.clone(), cached);

        result
    }
}

impl SnapshotSource for RpcSnapshotSource {
    fn get(
        &self,
        key: &Rc<LedgerKey>,
    ) -> std::result::Result<Option<EntryWithLiveUntil>, HostError> {
        // Lock briefly: take a clone of the cached value (cheap memcpy of a
        // small Vec), drop the lock, decode outside the critical section.
        // Holding the lock across decode would needlessly serialise
        // concurrent readers.
        let cached = self
            .cache
            .lock()
            .expect("cache mutex poisoned")
            .get(key.as_ref())
            .cloned();

        if let Some(value) = cached {
            return Ok(value.map(|(bytes, live_until)| (Rc::new(decode_entry(&bytes)), live_until)));
        }

        Ok(self.fetch_from_rpc(key.as_ref()))
    }
}

// ---------------------------------------------------------------------------
// XDR codec helpers
// ---------------------------------------------------------------------------

/// Encode a `LedgerEntry` to its XDR byte representation.
///
/// Panics if encoding fails. With `Limits::none()` there are no size caps,
/// so a failure here means the input was structurally invalid (e.g.
/// malformed XDR enum discriminant) — that's a bug in whoever produced the
/// `LedgerEntry`, not a runtime condition we can recover from gracefully.
fn encode_entry(entry: &LedgerEntry) -> Vec<u8> {
    entry
        .to_xdr(Limits::none())
        .unwrap_or_else(|e| panic!("soroban-fork: LedgerEntry encode failed (structural bug): {e}"))
}

/// Decode an XDR-encoded `LedgerEntry`.
///
/// Panics if decoding fails. The bytes always come from `encode_entry`
/// in this module (RPC fetches and JSON-loaded `LedgerSnapshot` entries
/// are both routed through `encode_entry` before they hit the cache),
/// so a failure here means the cache contents are corrupted — memory
/// damage, a process bug, or someone replaced the bytes externally.
/// Not recoverable.
fn decode_entry(bytes: &[u8]) -> LedgerEntry {
    LedgerEntry::from_xdr(bytes, Limits::none()).unwrap_or_else(|e| {
        panic!(
            "soroban-fork: cached LedgerEntry decode failed — cache corruption or \
             XDR-version mismatch: {e}"
        )
    })
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
    use soroban_env_host::xdr::{
        ConfigSettingEntry, ConfigSettingId, LedgerEntryData, LedgerEntryExt,
        LedgerKeyConfigSetting,
    };

    fn dummy_client() -> Arc<RpcClient> {
        Arc::new(
            RpcClient::new("http://localhost:0", crate::rpc::RpcConfig::default())
                .expect("client construction should not fail"),
        )
    }

    fn dummy_entry(last_modified: u32) -> (LedgerKey, LedgerEntry, Option<u32>) {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractMaxSizeBytes,
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: last_modified,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractMaxSizeBytes(65_536)),
            ext: LedgerEntryExt::V0,
        };
        (key, entry, None)
    }

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
        let src = RpcSnapshotSource::new(dummy_client());
        assert_eq!(src.fetch_mode, FetchMode::Strict);
    }

    #[test]
    fn xdr_round_trip_preserves_ledger_entry() {
        // The cache stores XDR bytes; correctness depends on encode/decode
        // being a true identity. If a future XDR-codec change ever broke
        // round-tripping, every cache hit would silently corrupt state —
        // this test pins the invariant.
        let (_, entry, _) = dummy_entry(42);
        let encoded = encode_entry(&entry);
        let decoded = decode_entry(&encoded);
        assert_eq!(entry, decoded);
        // And re-encoding the decoded value must reproduce the same bytes.
        assert_eq!(encoded, encode_entry(&decoded));
    }

    #[test]
    fn preload_then_entries_round_trips() {
        let src = RpcSnapshotSource::new(dummy_client());
        let original = vec![dummy_entry(7)];
        src.preload(original.clone());
        let exported = src.entries();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].0, original[0].0);
        assert_eq!(exported[0].1, original[0].1);
        assert_eq!(exported[0].2, original[0].2);
    }

    #[test]
    fn get_returns_preloaded_entry() {
        let src = RpcSnapshotSource::new(dummy_client());
        let (key, entry, live_until) = dummy_entry(99);
        src.preload(vec![(key.clone(), entry.clone(), live_until)]);

        let key_rc = Rc::new(key);
        let result = src.get(&key_rc).expect("get should not error");
        let (got_entry, got_live_until) = result.expect("preloaded entry should be present");
        assert_eq!(got_entry.as_ref(), &entry);
        assert_eq!(got_live_until, live_until);
        // Preloads do not count as fetches.
        assert_eq!(src.fetch_count(), 0);
    }

    #[test]
    fn concurrent_reads_of_preloaded_entry_are_race_free() {
        // Eight threads racing through `get` on the same preloaded key.
        // No fetch should fire (count stays 0). With RefCell the borrow
        // would panic; with Mutex<bytes>, we get correct shared access.
        use std::thread;
        let src = Arc::new(RpcSnapshotSource::new(dummy_client()));
        let (key, entry, live_until) = dummy_entry(123);
        src.preload(vec![(key.clone(), entry.clone(), live_until)]);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let src = Arc::clone(&src);
            let key = key.clone();
            let entry = entry.clone();
            handles.push(thread::spawn(move || {
                let key_rc = Rc::new(key);
                for _ in 0..100 {
                    let got = src
                        .get(&key_rc)
                        .expect("get should not error")
                        .expect("entry should be present");
                    assert_eq!(got.0.as_ref(), &entry);
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(src.fetch_count(), 0, "no RPC fetches should have fired");
    }
}
