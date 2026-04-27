//! Soroban JSON-RPC transport.
//!
//! A thin typed client around the handful of RPC methods the fork harness
//! actually needs: `getLedgerEntries`, `getLatestLedger`, and `getNetwork`.
//! Configurable retry + HTTP timeouts live here; XDR encode/decode sits here
//! too so the rest of the crate never touches wire formats directly.

use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use log::{debug, warn};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soroban_env_host::xdr::{
    LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, Limits, ReadXdr, WriteXdr,
};

use crate::error::{ForkError, Result};

/// Maximum number of bytes of an HTTP response body to embed in a
/// transport error. Soroban RPC error pages from edge caches (Cloudflare,
/// gateway.fm) can be large; truncating keeps logs readable.
const ERROR_BODY_TRUNCATE_BYTES: usize = 256;

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

/// Snapshot of the latest closed ledger as reported by the RPC.
///
/// `close_time` is fetched separately via `getLedgers` because
/// `getLatestLedger` does not include it in its response — the extra
/// round-trip happens once at fork build time.
pub struct LatestLedger {
    /// Latest closed ledger sequence.
    pub sequence: u32,
    /// Current protocol version the network is running.
    pub protocol_version: u32,
    /// Unix-seconds close time of the latest ledger. Defaulting the
    /// forked `Env`'s timestamp to this value keeps tests reproducible —
    /// wall-clock defaults make every run depend on when it was started.
    pub close_time: u64,
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

    /// Retrieve the latest ledger sequence, protocol version, and close time.
    ///
    /// Issues two RPC calls: `getLatestLedger` (sequence + protocol) and
    /// `getLedgers` for that sequence (close time). Both succeed or the
    /// whole call fails — partial state would let the build proceed with
    /// a wall-clock timestamp, which is exactly the silent fallback this
    /// API is designed to avoid.
    pub fn get_latest_ledger(&self) -> Result<LatestLedger> {
        let response: JsonRpcResponse<GetLatestLedgerResult> =
            self.rpc_post("getLatestLedger", serde_json::json!({}))?;
        let result = response.into_result()?;
        let close_time = self.get_ledger_close_time(result.sequence)?;
        Ok(LatestLedger {
            sequence: result.sequence,
            protocol_version: result.protocol_version,
            close_time,
        })
    }

    /// Fetch the close time (Unix seconds) of a specific ledger via
    /// `getLedgers`. Returns an error if the ledger is outside the RPC's
    /// retention window.
    pub fn get_ledger_close_time(&self, sequence: u32) -> Result<u64> {
        let response: JsonRpcResponse<GetLedgersResult> = self.rpc_post(
            "getLedgers",
            serde_json::json!({
                "startLedger": sequence,
                "pagination": { "limit": 1 },
            }),
        )?;
        let result = response.into_result()?;
        let ledger = result.ledgers.into_iter().next().ok_or_else(|| {
            ForkError::RpcError(format!("getLedgers returned no entry for {sequence}"))
        })?;
        ledger.ledger_close_time.parse::<u64>().map_err(|e| {
            ForkError::RpcError(format!(
                "getLedgers returned non-numeric ledgerCloseTime '{}': {e}",
                ledger.ledger_close_time
            ))
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
        let code = status.as_u16();
        // 408 Request Timeout, 425 Too Early, 429 Too Many Requests + 5xx
        // are the canonical "try again" set. Everything else 4xx is a
        // permanent caller error (bad auth, bad URL, bad JSON-RPC method).
        let retryable = matches!(code, 408 | 425 | 429) || status.is_server_error();
        if retryable {
            let body = response_body_snippet(response);
            return Err(RetryDecision::Retry(ForkError::Transport(format!(
                "HTTP {status}: {body}"
            ))));
        }
        if !status.is_success() {
            let body = response_body_snippet(response);
            return Err(RetryDecision::Fatal(ForkError::Transport(format!(
                "HTTP {status}: {body}"
            ))));
        }

        response
            .json::<JsonRpcResponse<T>>()
            .map_err(|e| RetryDecision::Retry(ForkError::from(e)))
    }
}

/// Best-effort extraction of a short, printable body snippet for error
/// messages. Returns `<no body>` if the body can't be read.
fn response_body_snippet(response: reqwest::blocking::Response) -> String {
    match response.text() {
        Ok(body) => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                "<empty body>".to_string()
            } else {
                truncate_chars(trimmed, ERROR_BODY_TRUNCATE_BYTES)
            }
        }
        Err(_) => "<no body>".to_string(),
    }
}

/// Truncate at character (not byte) boundary so we never split a
/// multi-byte UTF-8 sequence in the middle. Appends `…` if truncated.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

enum RetryDecision {
    /// Transient failure — caller should back off and retry if budget remains.
    Retry(ForkError),
    /// Permanent failure — no point retrying.
    Fatal(ForkError),
}

/// Exponential backoff with full jitter: each delay is uniformly sampled
/// from `[base * 2^attempt, base * 2^attempt + base)`. The jitter prevents
/// a fleet of concurrent tests from synchronising their retries into a
/// thundering-herd pattern when the RPC is briefly degraded.
///
/// Kept as a free function so it's unit-testable without standing up an
/// `RpcClient`.
fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    // Saturating pow prevents overflow on absurd retry counts.
    let factor = 2u32.saturating_pow(attempt);
    let exponential = base.saturating_mul(factor);
    let jitter = jitter_under(base);
    exponential.saturating_add(jitter)
}

/// Random `Duration` in `[0, max)`. Returns `Duration::ZERO` if `max` is
/// zero. Uses a thread-local xorshift64* seeded from the system clock —
/// good enough for spreading retries, not cryptographically random.
fn jitter_under(max: Duration) -> Duration {
    let max_nanos = max.as_nanos() as u64;
    if max_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(next_rng_u64() % max_nanos)
}

thread_local! {
    static RNG_STATE: Cell<u64> = Cell::new(seed_rng());
}

fn seed_rng() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() as u64).wrapping_mul(0x9E3779B97F4A7C15))
        .unwrap_or(0xDEAD_BEEF_CAFE_BABE)
        | 1 // ensure non-zero — xorshift would degenerate on 0
}

fn next_rng_u64() -> u64 {
    RNG_STATE.with(|cell| {
        let mut x = cell.get();
        // xorshift64 — Marsaglia 2003. State is never zero (seeded with |1
        // and the transform preserves non-zero), so degeneracy isn't a risk.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    })
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLedgersResult {
    ledgers: Vec<LedgerInfoWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerInfoWire {
    #[allow(dead_code)]
    sequence: u32,
    /// `getLedgers` returns this as a string-encoded Unix-seconds value
    /// (per the Stellar RPC OpenRPC spec). We parse to `u64` at the
    /// `RpcClient::get_ledger_close_time` boundary so callers get a
    /// numeric type and obvious failure on protocol drift.
    ledger_close_time: String,
}

// ---------------------------------------------------------------------------
// Unit tests — no network required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delay_doubles_each_attempt_within_jitter_window() {
        // With full jitter, the delay falls in [base * 2^attempt, base * (2^attempt + 1)).
        // Sample many times to smoke out off-by-one boundary mistakes in the
        // jitter implementation.
        let base = Duration::from_millis(100);
        for attempt in 0u32..4 {
            let factor = 2u32.pow(attempt);
            let lower = base * factor;
            let upper = base * (factor + 1);
            for _ in 0..32 {
                let d = backoff_delay(base, attempt);
                assert!(
                    d >= lower && d < upper,
                    "attempt {attempt}: {d:?} not in [{lower:?}, {upper:?})"
                );
            }
        }
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
    fn backoff_delay_zero_base_is_zero() {
        // Edge case: zero base => zero delay, no panic from `% 0`.
        assert_eq!(backoff_delay(Duration::ZERO, 0), Duration::ZERO);
        assert_eq!(backoff_delay(Duration::ZERO, 5), Duration::ZERO);
    }

    #[test]
    fn truncate_chars_handles_multibyte_safely() {
        // Cyrillic + emoji: each char is 2-4 bytes. Naive byte slicing
        // would panic on a UTF-8 boundary.
        let s = "тест🚀тест";
        let out = truncate_chars(s, 5);
        // 5 chars + ellipsis (more than 5 chars in the source).
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }

    #[test]
    fn truncate_chars_short_input_unchanged() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("", 10), "");
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
