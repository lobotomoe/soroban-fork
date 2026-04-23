//! Soroban JSON-RPC transport.
//!
//! A thin typed client around the handful of RPC methods the fork harness
//! actually needs: `getLedgerEntries`, `getLatestLedger`, and `getNetwork`.
//! Configurable retry + HTTP timeouts live here; XDR encode/decode sits here
//! too so the rest of the crate never touches wire formats directly.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use log::{debug, warn};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, Limits, ReadXdr, WriteXdr,
};

use crate::error::{ForkError, Result};

/// Tuning for the RPC transport layer.
#[derive(Clone, Debug)]
pub struct RpcConfig {
    /// Maximum number of retries for transient failures (network errors,
    /// HTTP 5xx, HTTP 429). Total attempts is `retries + 1`.
    pub retries: u32,
    /// Base delay between retries — doubles on each subsequent attempt.
    pub base_retry_delay: Duration,
    /// Per-request HTTP timeout. `None` delegates to reqwest's default.
    pub request_timeout: Option<Duration>,
    /// `getLedgerEntries` batch size. Soroban RPC caps this at 200; we
    /// default to the same and expose it for testing seams.
    pub max_keys_per_request: usize,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            retries: 3,
            base_retry_delay: Duration::from_millis(300),
            request_timeout: Some(Duration::from_secs(30)),
            max_keys_per_request: 200,
        }
    }
}

/// A single fetched ledger entry, with the live-until hint from the RPC.
pub struct FetchedEntry {
    /// The parsed entry body.
    pub entry: LedgerEntry,
    /// Ledger sequence at which this entry expires, if applicable.
    pub live_until: Option<u32>,
}

/// Lightweight summary of the `getLatestLedger` RPC response.
pub struct LatestLedger {
    /// Latest closed ledger sequence.
    pub sequence: u32,
    /// Current protocol version the network is running.
    pub protocol_version: u32,
}

/// Network metadata from the `getNetwork` RPC response.
pub struct NetworkMetadata {
    /// The network passphrase, e.g. `"Test SDF Network ; September 2015"`.
    pub passphrase: String,
    /// SHA-256 of the passphrase, as required by the VM's `LedgerInfo`.
    pub network_id: [u8; 32],
}

/// Typed RPC client. One instance per fork, reused across fetches so
/// reqwest can pool connections to the RPC.
pub struct RpcClient {
    http: reqwest::blocking::Client,
    url: String,
    config: RpcConfig,
}

impl RpcClient {
    /// Build an RPC client for `url` with the given transport config.
    pub fn new(url: impl Into<String>, config: RpcConfig) -> Result<Self> {
        let mut builder = reqwest::blocking::ClientBuilder::new();
        if let Some(timeout) = config.request_timeout {
            builder = builder.timeout(timeout);
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            url: url.into(),
            config,
        })
    }

    /// Retrieve the latest ledger sequence + protocol version.
    pub fn get_latest_ledger(&self) -> Result<LatestLedger> {
        let response: JsonRpcResponse<GetLatestLedgerResult> =
            self.rpc_post("getLatestLedger", serde_json::json!({}))?;
        let result = response.into_result()?;
        Ok(LatestLedger {
            sequence: result.sequence,
            protocol_version: result.protocol_version,
        })
    }

    /// Retrieve network metadata. The `network_id` is computed as
    /// SHA-256 of the returned passphrase (per Stellar convention).
    pub fn get_network(&self) -> Result<NetworkMetadata> {
        let response: JsonRpcResponse<GetNetworkResult> =
            self.rpc_post("getNetwork", serde_json::json!({}))?;
        let result = response.into_result()?;
        let mut hasher = Sha256::new();
        hasher.update(result.passphrase.as_bytes());
        let network_id: [u8; 32] = hasher.finalize().into();
        Ok(NetworkMetadata {
            passphrase: result.passphrase,
            network_id,
        })
    }

    /// Fetch ledger entries in batches of `max_keys_per_request`. Missing
    /// keys are simply absent from the returned vector — no error.
    pub fn fetch_entries(&self, keys: &[LedgerKey]) -> Result<Vec<FetchedEntry>> {
        let mut results = Vec::new();
        for chunk in keys.chunks(self.config.max_keys_per_request) {
            let encoded_keys = chunk.iter().map(encode_key).collect::<Result<Vec<_>>>()?;
            let response: JsonRpcResponse<GetLedgerEntriesResult> = self.rpc_post(
                "getLedgerEntries",
                serde_json::json!({ "keys": encoded_keys }),
            )?;
            let result = response.into_result()?;
            if let Some(entries) = result.entries {
                for wire in entries {
                    results.push(decode_entry(wire)?);
                }
            }
        }
        Ok(results)
    }

    /// Convenience: fetch a single key. Returns `None` if absent.
    pub fn fetch_entry(&self, key: &LedgerKey) -> Result<Option<FetchedEntry>> {
        Ok(self.fetch_entries(std::slice::from_ref(key))?.pop())
    }

    fn rpc_post<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<JsonRpcResponse<T>> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };

        let total_attempts = self.config.retries + 1;
        let mut last_error: Option<ForkError> = None;

        for attempt in 0..total_attempts {
            match self.try_once::<T>(&request) {
                Ok(parsed) => return Ok(parsed),
                Err(RetryDecision::Retry(err)) if attempt + 1 < total_attempts => {
                    let delay = backoff_delay(self.config.base_retry_delay, attempt);
                    warn!(
                        "soroban-fork: RPC {method} failed (attempt {}/{}): {err}; \
                         retrying in {delay:?}",
                        attempt + 1,
                        total_attempts
                    );
                    std::thread::sleep(delay);
                    last_error = Some(err);
                }
                Err(RetryDecision::Retry(err)) | Err(RetryDecision::Fatal(err)) => {
                    return Err(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ForkError::Transport("retry loop exhausted with no recorded error".into())
        }))
    }

    fn try_once<T: DeserializeOwned>(
        &self,
        request: &JsonRpcRequest<'_>,
    ) -> std::result::Result<JsonRpcResponse<T>, RetryDecision> {
        let response = self
            .http
            .post(&self.url)
            .json(request)
            .send()
            .map_err(|e| RetryDecision::Retry(ForkError::from(e)))?;

        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(RetryDecision::Retry(ForkError::Transport(format!(
                "HTTP {status}"
            ))));
        }
        if !status.is_success() {
            return Err(RetryDecision::Fatal(ForkError::Transport(format!(
                "HTTP {status}"
            ))));
        }

        response
            .json::<JsonRpcResponse<T>>()
            .map_err(|e| RetryDecision::Retry(ForkError::from(e)))
    }
}

enum RetryDecision {
    /// Transient failure — caller should back off and retry if budget remains.
    Retry(ForkError),
    /// Permanent failure — no point retrying.
    Fatal(ForkError),
}

/// Exponential backoff: `base * 2^attempt`. Kept as a free function so it's
/// unit-testable without standing up an `RpcClient`.
fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    // Saturating pow prevents overflow on absurd retry counts.
    let factor = 2u32.saturating_pow(attempt);
    base.saturating_mul(factor)
}

fn encode_key(key: &LedgerKey) -> Result<String> {
    let bytes = key
        .to_xdr(Limits::none())
        .map_err(|e| ForkError::Xdr(format!("encode LedgerKey: {e}")))?;
    Ok(BASE64.encode(&bytes))
}

fn decode_entry(wire: EntryResult) -> Result<FetchedEntry> {
    let entry_bytes = BASE64.decode(&wire.xdr)?;
    // RPC returns `LedgerEntryData`, not a full `LedgerEntry`. We reconstruct
    // the wrapper using the per-entry `lastModifiedLedgerSeq` the server
    // delivers alongside.
    let entry_data = LedgerEntryData::from_xdr(&entry_bytes, Limits::none())
        .map_err(|e| ForkError::Xdr(format!("decode LedgerEntryData: {e}")))?;
    let entry = LedgerEntry {
        last_modified_ledger_seq: wire.last_modified_ledger_seq,
        data: entry_data,
        ext: LedgerEntryExt::V0,
    };
    debug!(
        "soroban-fork: decoded entry, last_modified={}, live_until={:?}",
        wire.last_modified_ledger_seq, wire.live_until_ledger_seq
    );
    Ok(FetchedEntry {
        entry,
        live_until: wire.live_until_ledger_seq,
    })
}

// ---------------------------------------------------------------------------
// Wire types — isolated from the rest of the crate.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

impl<T> JsonRpcResponse<T> {
    fn into_result(self) -> Result<T> {
        if let Some(err) = self.error {
            return Err(ForkError::RpcError(err.to_string()));
        }
        self.result.ok_or(ForkError::RpcNoResult)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLedgerEntriesResult {
    entries: Option<Vec<EntryResult>>,
    #[allow(dead_code)]
    latest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryResult {
    #[allow(dead_code)]
    key: String,
    xdr: String,
    last_modified_ledger_seq: u32,
    live_until_ledger_seq: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLatestLedgerResult {
    #[allow(dead_code)]
    id: String,
    protocol_version: u32,
    sequence: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetNetworkResult {
    passphrase: String,
    #[allow(dead_code)]
    #[serde(default)]
    friendbot_url: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    protocol_version: Option<u32>,
}

// ---------------------------------------------------------------------------
// Unit tests — no network required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delay_doubles_each_attempt() {
        let base = Duration::from_millis(100);
        assert_eq!(backoff_delay(base, 0), Duration::from_millis(100));
        assert_eq!(backoff_delay(base, 1), Duration::from_millis(200));
        assert_eq!(backoff_delay(base, 2), Duration::from_millis(400));
        assert_eq!(backoff_delay(base, 3), Duration::from_millis(800));
    }

    #[test]
    fn backoff_delay_saturates_on_absurd_attempt() {
        let base = Duration::from_secs(1);
        // 2^100 overflows u32; saturating arithmetic prevents panic.
        let d = backoff_delay(base, 100);
        // Any finite result is acceptable; the important property is "doesn't panic".
        assert!(d >= base);
    }

    #[test]
    fn json_rpc_response_into_result_returns_result_when_ok() {
        let response: JsonRpcResponse<u32> = JsonRpcResponse {
            result: Some(42),
            error: None,
        };
        assert_eq!(response.into_result().unwrap(), 42);
    }

    #[test]
    fn json_rpc_response_into_result_propagates_error_field() {
        let response: JsonRpcResponse<u32> = JsonRpcResponse {
            result: None,
            error: Some(serde_json::json!({"code": -32000, "message": "boom"})),
        };
        let err = response.into_result().unwrap_err();
        assert!(matches!(err, ForkError::RpcError(_)));
    }

    #[test]
    fn json_rpc_response_into_result_errors_when_no_result_no_error() {
        let response: JsonRpcResponse<u32> = JsonRpcResponse {
            result: None,
            error: None,
        };
        let err = response.into_result().unwrap_err();
        assert!(matches!(err, ForkError::RpcNoResult));
    }

    #[test]
    fn rpc_config_default_has_sensible_values() {
        let cfg = RpcConfig::default();
        assert!(cfg.retries >= 1);
        assert!(cfg.base_retry_delay >= Duration::from_millis(50));
        assert_eq!(cfg.max_keys_per_request, 200);
        assert!(cfg.request_timeout.is_some());
    }
}
