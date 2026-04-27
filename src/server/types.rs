//! JSON-RPC 2.0 envelope + Stellar RPC method response types.
//!
//! These mirror the wire format documented at
//! <https://developers.stellar.org/docs/data/apis/rpc/api-reference> so a
//! consumer using `@stellar/stellar-sdk` (JS) or `stellar-rpc-client`
//! (Rust) can point at our `serve` endpoint without code changes.
//!
//! Every numeric ledger close-time on the wire is **a string of Unix
//! seconds** — that's the Stellar RPC convention; we follow it
//! verbatim.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JSON-RPC envelope
// ---------------------------------------------------------------------------

/// Inbound JSON-RPC 2.0 request. `params` stays as raw `serde_json::Value`
/// so each handler does typed deserialization on its own shape.
#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcRequest {
    pub(crate) jsonrpc: String,
    pub(crate) id: serde_json::Value,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: serde_json::Value,
}

/// Outbound JSON-RPC 2.0 response. Either `result` or `error` is set,
/// never both.
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: &'static str,
    pub(crate) id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcError {
    pub(crate) code: i32,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
}

impl JsonRpcError {
    /// `-32601 Method not found` per JSON-RPC 2.0 spec.
    pub(crate) fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    /// `-32602 Invalid params` per JSON-RPC 2.0 spec.
    pub(crate) fn invalid_params(detail: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: detail.into(),
            data: None,
        }
    }

    /// `-32603 Internal error` per JSON-RPC 2.0 spec.
    pub(crate) fn internal_error(detail: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: detail.into(),
            data: None,
        }
    }
}

impl JsonRpcResponse {
    pub(crate) fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn err(id: serde_json::Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// `getHealth`
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    pub(crate) status: &'static str,
    #[serde(rename = "latestLedger")]
    pub(crate) latest_ledger: u32,
    #[serde(rename = "oldestLedger")]
    pub(crate) oldest_ledger: u32,
    /// Always 0 — we serve only the fork-point ledger of state, no
    /// retention window. Real Stellar RPCs typically retain ~7 days of
    /// ledgers; that's not what a fork is.
    #[serde(rename = "ledgerRetentionWindow")]
    pub(crate) ledger_retention_window: u32,
}

// ---------------------------------------------------------------------------
// `getVersionInfo`
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct VersionInfoResponse {
    pub(crate) version: &'static str,
    #[serde(rename = "commitHash")]
    pub(crate) commit_hash: &'static str,
    #[serde(rename = "buildTimestamp")]
    pub(crate) build_timestamp: &'static str,
    #[serde(rename = "captiveCoreVersion")]
    pub(crate) captive_core_version: &'static str,
    #[serde(rename = "protocolVersion")]
    pub(crate) protocol_version: u32,
}

// ---------------------------------------------------------------------------
// `getNetwork`
// ---------------------------------------------------------------------------

/// Stellar's RPC includes a `friendbotUrl` field for testnet — we leave
/// it unset (skipped via `Option::None`) on a fork.
#[derive(Debug, Serialize)]
pub(crate) struct NetworkResponse {
    pub(crate) passphrase: String,
    #[serde(rename = "protocolVersion")]
    pub(crate) protocol_version: u32,
    /// Hex-encoded SHA-256 of the passphrase.
    #[serde(rename = "networkId")]
    pub(crate) network_id: String,
    #[serde(rename = "friendbotUrl", skip_serializing_if = "Option::is_none")]
    pub(crate) friendbot_url: Option<String>,
}

// ---------------------------------------------------------------------------
// `getLatestLedger`
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct LatestLedgerResponse {
    pub(crate) id: String,
    pub(crate) sequence: u32,
    #[serde(rename = "protocolVersion")]
    pub(crate) protocol_version: u32,
}

// ---------------------------------------------------------------------------
// `getLedgers`
// ---------------------------------------------------------------------------

/// Stellar's `getLedgers({ startLedger, pagination })` request shape.
/// `start_ledger` is honored only insofar as we accept any value; the
/// fork has exactly one ledger of state, so we always answer with our
/// own sequence. `pagination` is parsed-but-ignored to keep the wire
/// contract identical to upstream.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // both fields accepted for wire compat; never inspected
pub(crate) struct GetLedgersParams {
    pub(crate) start_ledger: Option<u32>,
    #[serde(default)]
    pub(crate) pagination: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetLedgersResponse {
    pub(crate) ledgers: Vec<LedgerInfo>,
    pub(crate) latest_ledger: u32,
    /// Stellar wire convention: this is a Unix seconds string, not a number.
    pub(crate) latest_ledger_close_time: String,
    pub(crate) oldest_ledger: u32,
    pub(crate) oldest_ledger_close_time: String,
    /// Empty cursor — we don't paginate.
    pub(crate) cursor: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LedgerInfo {
    pub(crate) hash: String,
    pub(crate) sequence: u32,
    /// Unix seconds as a string — Stellar convention.
    pub(crate) ledger_close_time: String,
}

// ---------------------------------------------------------------------------
// `getLedgerEntries`
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetLedgerEntriesParams {
    /// Base64-encoded XDR `LedgerKey`s, one per requested entry.
    pub(crate) keys: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetLedgerEntriesResponse {
    /// Entries that exist on the ledger. Missing keys are simply absent
    /// from this list — Stellar's wire convention. The client compares
    /// returned `key`s against the request to detect absence.
    pub(crate) entries: Vec<LedgerEntryItem>,
    pub(crate) latest_ledger: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LedgerEntryItem {
    /// Echo of the request's `LedgerKey` base64 string.
    pub(crate) key: String,
    /// Base64-encoded `LedgerEntryData` XDR (NOT the full `LedgerEntry`
    /// — Stellar's RPC strips the wrapper and returns just the inner
    /// `data` field; our handler does the same to match).
    pub(crate) xdr: String,
    pub(crate) last_modified_ledger_seq: u32,
    /// Optional TTL hint. Present only for entries that have one
    /// (contract-data, contract-code).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) live_until_ledger_seq: Option<u32>,
}

// ---------------------------------------------------------------------------
// `simulateTransaction`
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SimulateTransactionParams {
    /// Base64-encoded `TransactionEnvelope` XDR. Must contain exactly
    /// one `InvokeHostFunctionOp`; classic operations or multi-op
    /// transactions are rejected with `invalid_params`.
    pub(crate) transaction: String,
    /// Resource config knob for the upstream RPC (e.g. instruction
    /// budget overrides). Accepted but currently unused — we run with
    /// a fixed default budget. Field kept for wire compatibility.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) resource_config: Option<serde_json::Value>,
}

/// Response shape per
/// <https://developers.stellar.org/docs/data/apis/rpc/api-reference/methods/simulateTransaction>.
///
/// Either `error` is present (with all other fields elided) on simulation
/// failure, or `results`/`transactionData`/etc. are populated on success.
/// Stellar's RPC sometimes returns BOTH error and partial data — we only
/// emit one or the other to keep the contract clean.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SimulateTransactionResponse {
    /// Base64-encoded `SorobanTransactionData` XDR. Carries the
    /// recorded footprint and a stub `resourceFee=0`. Fee estimation
    /// against the live Stellar fee schedule is intentionally stubbed
    /// in v0.5 — clients that need real fee numbers should use the
    /// upstream RPC for that step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transaction_data: Option<String>,
    /// Stub: always `"0"`. See `transaction_data` note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min_resource_fee: Option<String>,
    /// Base64-encoded `DiagnosticEvent` XDRs emitted during
    /// simulation. Includes contract events plus host
    /// `fn_call`/`fn_return` diagnostics if tracing was enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) events: Option<Vec<String>>,
    /// Per-host-function results. We only support single-op
    /// transactions so this always has length 0 (on error) or 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) results: Option<Vec<SimulateHostFunctionResult>>,
    /// CPU instructions and memory bytes consumed during simulation.
    /// Stellar wire convention: numbers as strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost: Option<SimulationCost>,
    pub(crate) latest_ledger: u32,
    /// Set when simulation failed. When present, all other result
    /// fields are absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SimulateHostFunctionResult {
    /// Base64-encoded auth entries that the transaction needs to be
    /// signed with for `sendTransaction` to succeed. Comes from the
    /// host's recording-mode auth tracking.
    pub(crate) auth: Vec<String>,
    /// Base64-encoded `ScVal` — the function's return value.
    pub(crate) xdr: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SimulationCost {
    /// CPU instructions, as a decimal string.
    pub(crate) cpu_insns: String,
    /// Memory bytes, as a decimal string.
    pub(crate) mem_bytes: String,
}
