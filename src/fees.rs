//! Live-network resource-fee computation for `simulateTransaction`.
//!
//! Stellar's Soroban resource-fee schedule is itself stored on-chain, in a
//! handful of [`ConfigSettingEntry`] ledger entries. The fee a real
//! `sendTransaction` would charge is
//! [`compute_transaction_resource_fee`] applied to that schedule and the
//! recorded transaction resources.
//!
//! Up through v0.5.1 the server stubbed `minResourceFee` as `"0"` — this
//! module replaces that stub with honest math: at first call we resolve
//! the six ConfigSetting keys through the snapshot source (one
//! upstream-RPC round-trip per key, then cached forever), decode the
//! schedule into [`FeeConfiguration`], and feed it to the host's own
//! [`compute_transaction_resource_fee`].
//!
//! [`compute_transaction_resource_fee`]: soroban_env_host::fees::compute_transaction_resource_fee

use std::rc::Rc;

use soroban_env_host::storage::SnapshotSource;
use soroban_env_host::xdr::{
    ConfigSettingEntry, ConfigSettingId, LedgerEntryData, LedgerKey, LedgerKeyConfigSetting,
};

pub use soroban_env_host::fees::{
    compute_transaction_resource_fee, FeeConfiguration, TransactionResources,
};

use crate::{ForkError, Result, RpcSnapshotSource};

/// Resolve the six on-chain ConfigSettingEntry rows that make up the
/// Soroban resource-fee schedule and decode them into one
/// [`FeeConfiguration`] suitable for [`compute_transaction_resource_fee`].
///
/// Each entry is fetched via the supplied snapshot source, so on the
/// second call (or after a cache preload) this is fully local.
pub fn fetch_fee_configuration(source: &RpcSnapshotSource) -> Result<FeeConfiguration> {
    let mut cfg = FeeConfiguration::default();

    apply_setting(source, ConfigSettingId::ContractComputeV0, &mut cfg)?;
    apply_setting(source, ConfigSettingId::ContractLedgerCostV0, &mut cfg)?;
    apply_setting(source, ConfigSettingId::ContractLedgerCostExtV0, &mut cfg)?;
    apply_setting(
        source,
        ConfigSettingId::ContractHistoricalDataV0,
        &mut cfg,
    )?;
    apply_setting(source, ConfigSettingId::ContractEventsV0, &mut cfg)?;
    apply_setting(source, ConfigSettingId::ContractBandwidthV0, &mut cfg)?;

    Ok(cfg)
}

fn apply_setting(
    source: &RpcSnapshotSource,
    id: ConfigSettingId,
    cfg: &mut FeeConfiguration,
) -> Result<()> {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: id,
    });
    let key_rc = Rc::new(key);

    let resolved = source.get(&key_rc).map_err(|e| {
        ForkError::Host(format!(
            "fee config fetch ({id:?}): snapshot source error: {e:?}"
        ))
    })?;
    let (entry_rc, _live_until) = resolved.ok_or_else(|| {
        ForkError::Host(format!(
            "fee config fetch ({id:?}): config setting absent on the ledger"
        ))
    })?;

    let setting = match &entry_rc.data {
        LedgerEntryData::ConfigSetting(s) => s,
        other => {
            return Err(ForkError::Host(format!(
                "fee config fetch ({id:?}): expected ConfigSetting entry, got {other:?}"
            )));
        }
    };

    match setting {
        ConfigSettingEntry::ContractComputeV0(c) => {
            cfg.fee_per_instruction_increment = c.fee_rate_per_instructions_increment;
        }
        ConfigSettingEntry::ContractLedgerCostV0(c) => {
            cfg.fee_per_disk_read_entry = c.fee_disk_read_ledger_entry;
            cfg.fee_per_write_entry = c.fee_write_ledger_entry;
            cfg.fee_per_disk_read_1kb = c.fee_disk_read1_kb;
        }
        ConfigSettingEntry::ContractLedgerCostExtV0(c) => {
            cfg.fee_per_write_1kb = c.fee_write1_kb;
        }
        ConfigSettingEntry::ContractHistoricalDataV0(c) => {
            cfg.fee_per_historical_1kb = c.fee_historical1_kb;
        }
        ConfigSettingEntry::ContractEventsV0(c) => {
            cfg.fee_per_contract_event_1kb = c.fee_contract_events1_kb;
        }
        ConfigSettingEntry::ContractBandwidthV0(c) => {
            cfg.fee_per_transaction_size_1kb = c.fee_tx_size1_kb;
        }
        other => {
            return Err(ForkError::Host(format!(
                "fee config fetch ({id:?}): unexpected variant {other:?}"
            )));
        }
    }

    Ok(())
}
