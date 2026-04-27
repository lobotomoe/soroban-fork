//! On-disk cache for fetched ledger entries.
//!
//! The format is the standard `stellar snapshot` JSON (`LedgerSnapshot`) so
//! snapshots produced by `stellar snapshot create` can be loaded as cache
//! files, and cache files this crate writes can be inspected with the same
//! `stellar` tooling.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use soroban_env_host::xdr::{LedgerEntry, LedgerKey};
use soroban_ledger_snapshot::LedgerSnapshot;

use crate::error::{ForkError, Result};

/// Default ledger-info fields written when persisting a snapshot. They
/// mirror the current Stellar network config and rarely need to change;
/// callers that do need custom values can construct a `LedgerSnapshot`
/// directly from [`super::ForkedEnv::entries_snapshot`].
pub(crate) const DEFAULT_BASE_RESERVE: u32 = 100;
pub(crate) const DEFAULT_MIN_PERSISTENT_ENTRY_TTL: u32 = 4_096;
pub(crate) const DEFAULT_MIN_TEMP_ENTRY_TTL: u32 = 16;
pub(crate) const DEFAULT_MAX_ENTRY_TTL: u32 = 6_312_000;

/// Load ledger entries from a snapshot JSON file.
pub fn load_snapshot(path: &Path) -> Result<Vec<(LedgerKey, LedgerEntry, Option<u32>)>> {
    let snapshot = LedgerSnapshot::read_file(path).map_err(|e| ForkError::Cache {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let entries = snapshot
        .ledger_entries
        .into_iter()
        .map(|(key, (entry, live_until))| (*key, *entry, live_until))
        .collect();

    Ok(entries)
}

/// Persist ledger entries to a snapshot JSON file. Overwrites any existing
/// file at `path`.
///
/// The write is atomic on POSIX: contents go to a sibling temp file first
/// and are renamed into place. A crash mid-write therefore leaves either
/// the previous cache intact or no change at all — never a half-written
/// JSON file that the next test would refuse to parse.
pub fn save_snapshot(
    path: &Path,
    entries: &[(LedgerKey, LedgerEntry, Option<u32>)],
    sequence: u32,
    timestamp: u64,
    network_id: [u8; 32],
    protocol_version: u32,
) -> Result<()> {
    let ledger_entries = entries
        .iter()
        .map(|(key, entry, live_until)| {
            (
                Box::new(key.clone()),
                (Box::new(entry.clone()), *live_until),
            )
        })
        .collect();

    let snapshot = LedgerSnapshot {
        protocol_version,
        sequence_number: sequence,
        timestamp,
        network_id,
        base_reserve: DEFAULT_BASE_RESERVE,
        min_persistent_entry_ttl: DEFAULT_MIN_PERSISTENT_ENTRY_TTL,
        min_temp_entry_ttl: DEFAULT_MIN_TEMP_ENTRY_TTL,
        max_entry_ttl: DEFAULT_MAX_ENTRY_TTL,
        ledger_entries,
    };

    let tmp = tmp_sibling(path);
    snapshot.write_file(&tmp).map_err(|e| ForkError::Cache {
        path: tmp.clone(),
        message: e.to_string(),
    })?;

    std::fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup: if rename failed, the tmp file is dead weight.
        let _ = std::fs::remove_file(&tmp);
        ForkError::Cache {
            path: path.to_path_buf(),
            message: format!("rename {} -> {}: {e}", tmp.display(), path.display()),
        }
    })
}

/// Build a sibling path for the temp file used during atomic writes.
///
/// The name is unique per `(pid, nanos)` so concurrent writers — e.g. two
/// test binaries sharing a cache file — don't stomp on each other's tmp
/// file before either rename completes.
fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("snapshot");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(".{filename}.tmp.{pid}.{nanos}");
    match parent {
        Some(p) => p.join(tmp_name),
        None => std::path::PathBuf::from(tmp_name),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host::xdr::{
        ConfigSettingId, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey,
        LedgerKeyConfigSetting,
    };
    use std::io::Write;

    /// A trivial ledger key + entry pair that doesn't require contract state
    /// setup — used to exercise the save/load roundtrip.
    fn dummy_entry() -> (LedgerKey, LedgerEntry, Option<u32>) {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractMaxSizeBytes,
        });
        // Write the config entry to match the key type so XDR decode on load
        // doesn't complain about a mismatch.
        let entry = LedgerEntry {
            last_modified_ledger_seq: 42,
            data: LedgerEntryData::ConfigSetting(
                soroban_env_host::xdr::ConfigSettingEntry::ContractMaxSizeBytes(65_536),
            ),
            ext: LedgerEntryExt::V0,
        };
        (key, entry, None)
    }

    #[test]
    fn save_then_load_preserves_entries() {
        let tmp = tempfile_path("roundtrip.json");
        let original = vec![dummy_entry()];

        save_snapshot(&tmp, &original, 100, 1_234_567, [0xAB; 32], 25).expect("save_snapshot");
        let loaded = load_snapshot(&tmp).expect("load_snapshot");

        assert_eq!(loaded.len(), original.len());
        assert_eq!(loaded[0].0, original[0].0);
        assert_eq!(loaded[0].2, original[0].2);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn load_missing_file_returns_cache_error() {
        let path = std::path::PathBuf::from("/nonexistent/path/soroban-fork-test.json");
        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(err, ForkError::Cache { .. }));
    }

    #[test]
    fn load_malformed_file_returns_cache_error() {
        let tmp = tempfile_path("malformed.json");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"{not json").unwrap();
        drop(f);

        let err = load_snapshot(&tmp).unwrap_err();
        assert!(matches!(err, ForkError::Cache { .. }));

        std::fs::remove_file(&tmp).ok();
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("soroban-fork-test-{}-{name}", std::process::id()));
        p
    }

    #[test]
    fn tmp_sibling_lives_next_to_target() {
        let target = std::path::PathBuf::from("/tmp/cache.json");
        let tmp = tmp_sibling(&target);
        assert_eq!(tmp.parent(), Some(std::path::Path::new("/tmp")));
        let name = tmp.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.starts_with(".cache.json.tmp."));
    }

    #[test]
    fn tmp_sibling_handles_bare_filename() {
        // No parent component — must still produce a usable tmp name in the cwd.
        let target = std::path::PathBuf::from("cache.json");
        let tmp = tmp_sibling(&target);
        let name = tmp.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.starts_with(".cache.json.tmp."));
    }

    #[test]
    fn save_is_atomic_no_tmp_left_behind() {
        let target = tempfile_path("atomic.json");
        save_snapshot(&target, &[dummy_entry()], 1, 1, [0; 32], 25).expect("save");

        // After a successful save, the tmp file must not exist.
        let parent = target.parent().unwrap();
        let leftover_tmps: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| {
                        n.starts_with(&format!(
                            ".{}.tmp.",
                            target.file_name().unwrap().to_str().unwrap()
                        ))
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftover_tmps.is_empty(), "tmp leftovers: {leftover_tmps:?}");

        std::fs::remove_file(&target).ok();
    }
}
