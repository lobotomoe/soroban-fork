use soroban_ledger_snapshot::LedgerSnapshot;
use std::path::Path;
use stellar_xdr::curr::{LedgerEntry, LedgerKey};

/// Load entries from a LedgerSnapshot JSON file.
/// Returns entries as (key, entry, live_until) tuples.
pub fn load_snapshot(path: &Path) -> Result<Vec<(LedgerKey, LedgerEntry, Option<u32>)>, String> {
    let snapshot = LedgerSnapshot::read_file(path)
        .map_err(|e| format!("Failed to read snapshot {}: {e}", path.display()))?;

    let entries = snapshot
        .ledger_entries
        .into_iter()
        .map(|(key, (entry, live_until))| (*key, *entry, live_until))
        .collect();

    Ok(entries)
}

/// Save entries to a LedgerSnapshot JSON file.
/// The snapshot is compatible with `stellar snapshot create` output.
pub fn save_snapshot(
    path: &Path,
    entries: &[(LedgerKey, LedgerEntry, Option<u32>)],
    sequence: u32,
    timestamp: u64,
    network_id: [u8; 32],
    protocol_version: u32,
) -> Result<(), String> {
    let ledger_entries: Vec<_> = entries
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
        base_reserve: 100,
        min_persistent_entry_ttl: 4096,
        min_temp_entry_ttl: 16,
        max_entry_ttl: 6_312_000,
        ledger_entries,
    };

    snapshot
        .write_file(path)
        .map_err(|e| format!("Failed to write snapshot {}: {e}", path.display()))
}
