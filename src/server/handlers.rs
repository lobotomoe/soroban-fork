//! axum handlers — JSON-RPC dispatch + per-method functions.
//!
//! All Stellar RPC methods land at one HTTP route (`POST /`) carrying a
//! JSON-RPC envelope. We dispatch on the `method` field. New methods are
//! added by extending the `dispatch` match arm.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use log::warn;
use soroban_env_host::xdr::{Limits, ReadXdr, WriteXdr};

use crate::server::actor::{ActorHandle, Command, SimulationReply};
use crate::server::types::{
    CloseLedgersParams, CloseLedgersResponse, GetLedgerEntriesParams, GetLedgerEntriesResponse,
    GetLedgersParams, GetLedgersResponse, GetTransactionParams, GetTransactionResponse,
    HealthResponse, JsonRpcError, JsonRpcRequest, JsonRpcResponse, LatestLedgerResponse,
    LedgerEntryItem, LedgerInfo, NetworkResponse, SendTransactionParams, SendTransactionResponse,
    SetCodeParams, SetCodeResponse, SetLedgerEntryParams, SetLedgerEntryResponse, SetStorageParams,
    SimulateHostFunctionResult, SimulateTransactionParams, SimulateTransactionResponse,
    SimulationCost, StorageDurability, VersionInfoResponse,
};

/// Shared HTTP-layer state. Cheap to clone (the actor handle clones an
/// `Arc`-backed channel sender).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) actor: ActorHandle,
}

/// Hard-coded version info baked at compile time so an offline server
/// still answers correctly. `CARGO_PKG_VERSION` is set by Cargo;
/// `commit_hash` and `build_timestamp` are placeholders pending a
/// future `build.rs` that wires git/time at compile time.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT_HASH: &str = "unknown";
const BUILD_TIMESTAMP: &str = "unknown";
const CAPTIVE_CORE_VERSION: &str = "n/a (forked-RPC mode)";

/// Single POST handler that decodes the JSON-RPC envelope, dispatches
/// to a method, and re-wraps the result.
///
/// **JSON-RPC errors are returned as HTTP 200 with an `error` field in
/// the body** — that's per the JSON-RPC 2.0 spec. We only emit non-200
/// for envelope-level failures (malformed JSON, wrong jsonrpc version)
/// where the client wouldn't even have a request `id` to attach to.
pub(crate) async fn jsonrpc_handler(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    if req.jsonrpc != "2.0" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("unsupported jsonrpc version: {}", req.jsonrpc),
            })),
        )
            .into_response();
    }

    let id = req.id.clone();
    let response = match dispatch(&state, &req).await {
        Ok(value) => JsonRpcResponse::ok(id, value),
        Err(err) => JsonRpcResponse::err(id, err),
    };
    Json(response).into_response()
}

async fn dispatch(
    state: &AppState,
    req: &JsonRpcRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    match req.method.as_str() {
        "getHealth" => handle_get_health(state).await,
        "getVersionInfo" => handle_get_version_info(state).await,
        "getNetwork" => handle_get_network(state).await,
        "getLatestLedger" => handle_get_latest_ledger(state).await,
        "getLedgers" => handle_get_ledgers(state, &req.params).await,
        "getLedgerEntries" => handle_get_ledger_entries(state, &req.params).await,
        "simulateTransaction" => handle_simulate_transaction(state, &req.params).await,
        "sendTransaction" => handle_send_transaction(state, &req.params).await,
        "getTransaction" => handle_get_transaction(state, &req.params).await,
        "fork_setLedgerEntry" => handle_fork_set_ledger_entry(state, &req.params).await,
        "fork_setStorage" => handle_fork_set_storage(state, &req.params).await,
        "fork_setCode" => handle_fork_set_code(state, &req.params).await,
        "fork_closeLedgers" => handle_fork_close_ledgers(state, &req.params).await,
        unknown => {
            warn!("soroban-fork: unsupported RPC method: {unknown}");
            Err(JsonRpcError::method_not_found(unknown))
        }
    }
}

// ---------------------------------------------------------------------------
// Method handlers — each follows: actor.send → typed reply → JSON value
// ---------------------------------------------------------------------------

async fn handle_get_health(state: &AppState) -> Result<serde_json::Value, JsonRpcError> {
    let reply = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let body = HealthResponse {
        status: "healthy",
        latest_ledger: reply.sequence,
        oldest_ledger: reply.sequence,
        ledger_retention_window: 0,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_version_info(state: &AppState) -> Result<serde_json::Value, JsonRpcError> {
    let reply = state
        .actor
        .send(|tx| Command::GetNetwork { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let body = VersionInfoResponse {
        version: SERVER_VERSION,
        commit_hash: COMMIT_HASH,
        build_timestamp: BUILD_TIMESTAMP,
        captive_core_version: CAPTIVE_CORE_VERSION,
        protocol_version: reply.protocol_version,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_network(state: &AppState) -> Result<serde_json::Value, JsonRpcError> {
    let reply = state
        .actor
        .send(|tx| Command::GetNetwork { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let body = NetworkResponse {
        passphrase: reply.passphrase,
        protocol_version: reply.protocol_version,
        network_id: reply.network_id_hex,
        friendbot_url: None,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_latest_ledger(state: &AppState) -> Result<serde_json::Value, JsonRpcError> {
    let reply = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let body = LatestLedgerResponse {
        id: reply.id,
        sequence: reply.sequence,
        protocol_version: reply.protocol_version,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_ledgers(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    // `getLedgers` accepts `{ startLedger?, pagination? }`. Both are
    // optional in our impl — the fork has one ledger, served regardless.
    // Parse params for wire-shape compatibility; the values are
    // intentionally not threaded through since we only have one ledger
    // of state to serve regardless of the requested range.
    let _parsed: GetLedgersParams = if params.is_null() {
        GetLedgersParams {
            start_ledger: None,
            pagination: None,
        }
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("getLedgers params: {e}")))?
    };

    let reply = state
        .actor
        .send(|tx| Command::GetLedgersPage { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let close_time_str = reply.close_time.to_string();
    let body = GetLedgersResponse {
        ledgers: vec![LedgerInfo {
            // Synthesised hash: we don't have a real ledger hash for
            // the fork, but the wire field is required. Encode the
            // sequence into the hash placeholder so at least it's
            // unique per fork-point.
            hash: format!("forked-ledger-hash-{}", reply.sequence),
            sequence: reply.sequence,
            ledger_close_time: close_time_str.clone(),
        }],
        latest_ledger: reply.sequence,
        latest_ledger_close_time: close_time_str.clone(),
        oldest_ledger: reply.sequence,
        oldest_ledger_close_time: close_time_str,
        cursor: String::new(),
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_ledger_entries(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let parsed: GetLedgerEntriesParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("getLedgerEntries params: {e}")))?;

    if parsed.keys.is_empty() {
        return Err(JsonRpcError::invalid_params(
            "getLedgerEntries: keys array must be non-empty",
        ));
    }

    // Decode base64+XDR `LedgerKey`s in the handler thread. Failing
    // here with `invalid_params` rather than passing garbage to the
    // worker keeps error attribution clean.
    let mut decoded_keys = Vec::with_capacity(parsed.keys.len());
    for (i, raw) in parsed.keys.iter().enumerate() {
        let bytes = BASE64
            .decode(raw)
            .map_err(|e| JsonRpcError::invalid_params(format!("keys[{i}]: base64 decode: {e}")))?;
        let key = soroban_env_host::xdr::LedgerKey::from_xdr(&bytes, Limits::none())
            .map_err(|e| JsonRpcError::invalid_params(format!("keys[{i}]: XDR decode: {e}")))?;
        decoded_keys.push(key);
    }

    let reply = state
        .actor
        .send(|tx| Command::GetLedgerEntries {
            keys: decoded_keys,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    // Wire format: only present entries make it into the array; absent
    // ones are simply omitted. The client matches by re-encoding their
    // request keys and looking them up in the response.
    let mut items = Vec::with_capacity(reply.entries.len());
    for resolved in reply.entries.into_iter().flatten() {
        let (key, entry, live_until) = resolved;
        let key_xdr = key
            .to_xdr(Limits::none())
            .map_err(|e| JsonRpcError::internal_error(format!("encode response LedgerKey: {e}")))?;
        // Stellar's RPC returns `LedgerEntryData`, not the full
        // `LedgerEntry`. Strip the wrapper and emit just the data.
        let data_xdr = entry.data.to_xdr(Limits::none()).map_err(|e| {
            JsonRpcError::internal_error(format!("encode response LedgerEntryData: {e}"))
        })?;
        items.push(LedgerEntryItem {
            key: BASE64.encode(&key_xdr),
            xdr: BASE64.encode(&data_xdr),
            last_modified_ledger_seq: entry.last_modified_ledger_seq,
            live_until_ledger_seq: live_until,
        });
    }

    let body = GetLedgerEntriesResponse {
        entries: items,
        latest_ledger: reply.latest_ledger,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_simulate_transaction(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let parsed: SimulateTransactionParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("simulateTransaction params: {e}")))?;

    // Decode the transaction envelope from base64+XDR.
    let envelope_bytes = BASE64
        .decode(&parsed.transaction)
        .map_err(|e| JsonRpcError::invalid_params(format!("transaction: base64 decode: {e}")))?;
    let envelope =
        soroban_env_host::xdr::TransactionEnvelope::from_xdr(&envelope_bytes, Limits::none())
            .map_err(|e| JsonRpcError::invalid_params(format!("transaction: XDR decode: {e}")))?;

    // Stellar's bandwidth + historical-data fee components depend on
    // the on-the-wire envelope length. We measure once here so the
    // worker can fold it into `compute_transaction_resource_fee`.
    let transaction_size_bytes: u32 = envelope_bytes.len().try_into().map_err(|_| {
        JsonRpcError::invalid_params(format!(
            "transaction: envelope too large for u32 ({} bytes)",
            envelope_bytes.len()
        ))
    })?;

    // Extract the host function and source account. Only single-op
    // InvokeHostFunction transactions are supported in v0.5; classic
    // operations and multi-op envelopes get a clear `invalid_params`.
    let (host_function, source_account) = extract_invoke_op(&envelope)?;

    let reply = state
        .actor
        .send(|tx| Command::SimulateTransaction {
            host_function,
            source_account,
            transaction_size_bytes,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    encode_simulation_reply(reply)
}

/// Pull the `HostFunction` and source account out of a Soroban
/// transaction envelope. Stellar supports several envelope variants
/// (V0, V1, FeeBump); for Soroban contract calls we accept V1 and
/// FeeBump-wrapping-V1.
fn extract_invoke_op(
    env: &soroban_env_host::xdr::TransactionEnvelope,
) -> Result<
    (
        soroban_env_host::xdr::HostFunction,
        soroban_env_host::xdr::AccountId,
    ),
    JsonRpcError,
> {
    use soroban_env_host::xdr::{
        FeeBumpTransactionInnerTx, MuxedAccount, Operation, OperationBody, TransactionEnvelope,
    };

    // Resolve the inner V1 transaction regardless of envelope shape.
    let (operations, source) = match env {
        TransactionEnvelope::TxV0(_) => {
            return Err(JsonRpcError::invalid_params(
                "simulateTransaction: V0 transaction envelopes do not support Soroban operations",
            ));
        }
        TransactionEnvelope::Tx(tx) => (tx.tx.operations.as_slice(), tx.tx.source_account.clone()),
        TransactionEnvelope::TxFeeBump(fb) => match &fb.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => (
                inner.tx.operations.as_slice(),
                inner.tx.source_account.clone(),
            ),
        },
    };

    if operations.len() != 1 {
        return Err(JsonRpcError::invalid_params(format!(
            "simulateTransaction: expected exactly 1 operation, got {}",
            operations.len()
        )));
    }
    let op: &Operation = &operations[0];

    let invoke_op = match &op.body {
        OperationBody::InvokeHostFunction(ihf) => ihf,
        other => {
            return Err(JsonRpcError::invalid_params(format!(
                "simulateTransaction: only InvokeHostFunction operations supported, got {other:?}"
            )));
        }
    };

    // The op may override the transaction's source_account. If so, that
    // wins (matches Stellar core semantics).
    let source_muxed: MuxedAccount = op.source_account.clone().unwrap_or(source);
    let source_account = match source_muxed {
        MuxedAccount::Ed25519(uint256) => soroban_env_host::xdr::AccountId(
            soroban_env_host::xdr::PublicKey::PublicKeyTypeEd25519(uint256),
        ),
        MuxedAccount::MuxedEd25519(muxed) => soroban_env_host::xdr::AccountId(
            soroban_env_host::xdr::PublicKey::PublicKeyTypeEd25519(muxed.ed25519),
        ),
    };

    Ok((invoke_op.host_function.clone(), source_account))
}

/// Take a worker-side `SimulationReply` and wire-encode it as the
/// JSON-RPC response body.
fn encode_simulation_reply(reply: SimulationReply) -> Result<serde_json::Value, JsonRpcError> {
    use soroban_env_host::xdr::{SorobanTransactionData, SorobanTransactionDataExt};

    let latest_ledger = reply.latest_ledger;

    // Failure path — emit only `latestLedger` + `error`.
    let scval = match reply.result {
        Ok(scval) => scval,
        Err(error) => {
            let body = SimulateTransactionResponse {
                transaction_data: None,
                min_resource_fee: None,
                events: None,
                results: None,
                cost: None,
                latest_ledger,
                error: Some(error),
            };
            return serde_json::to_value(body)
                .map_err(|e| JsonRpcError::internal_error(e.to_string()));
        }
    };
    let scval_xdr = scval
        .to_xdr(Limits::none())
        .map_err(|e| JsonRpcError::internal_error(format!("encode result ScVal: {e}")))?;

    // Encode auth entries.
    let mut auth_b64 = Vec::with_capacity(reply.auth.len());
    for entry in &reply.auth {
        let bytes = entry
            .to_xdr(Limits::none())
            .map_err(|e| JsonRpcError::internal_error(format!("encode auth entry: {e}")))?;
        auth_b64.push(BASE64.encode(&bytes));
    }

    // Build SorobanTransactionData. The `resourceFee` field on the
    // wire-format `SorobanTransactionData` is the same number we emit
    // as the top-level `minResourceFee`; pass it through so signed
    // envelopes built from this response carry the right declaration.
    // When the schedule isn't resolvable we fall back to 0 — clients
    // that care about correctness watch the top-level field.
    let resource_fee = reply.min_resource_fee.unwrap_or(0);
    let txn_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: reply.resources.clone(),
        resource_fee,
    };
    let txn_data_xdr = txn_data
        .to_xdr(Limits::none())
        .map_err(|e| JsonRpcError::internal_error(format!("encode SorobanTransactionData: {e}")))?;

    // Combine contract events + diagnostic events into the wire
    // `events: string[]` (each is base64 XDR DiagnosticEvent).
    // Contract events get wrapped into DiagnosticEvent { in_successful_contract_call: true, event }.
    let mut events_b64 = Vec::new();
    for ce in reply.contract_events {
        let de = soroban_env_host::xdr::DiagnosticEvent {
            in_successful_contract_call: true,
            event: ce,
        };
        let bytes = de
            .to_xdr(Limits::none())
            .map_err(|e| JsonRpcError::internal_error(format!("encode contract event: {e}")))?;
        events_b64.push(BASE64.encode(&bytes));
    }
    for de in reply.diagnostic_events {
        let bytes = de
            .to_xdr(Limits::none())
            .map_err(|e| JsonRpcError::internal_error(format!("encode diagnostic event: {e}")))?;
        events_b64.push(BASE64.encode(&bytes));
    }

    // `cost.cpuInsns` is the host-budget instruction count; `cost.memBytes`
    // is the host-budget memory consumption queried from the same Budget
    // the invocation ran against. When the failure path hands us no
    // metering at all, omit the cost block rather than emit zeros.
    let cost = reply.mem_bytes.map(|mem| SimulationCost {
        cpu_insns: reply.resources.instructions.to_string(),
        mem_bytes: mem.to_string(),
    });

    let body = SimulateTransactionResponse {
        transaction_data: Some(BASE64.encode(&txn_data_xdr)),
        min_resource_fee: reply.min_resource_fee.map(|n| n.to_string()),
        events: Some(events_b64),
        results: Some(vec![SimulateHostFunctionResult {
            auth: auth_b64,
            xdr: BASE64.encode(&scval_xdr),
        }]),
        cost,
        latest_ledger,
        error: None,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_send_transaction(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let parsed: SendTransactionParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("sendTransaction params: {e}")))?;

    let envelope_bytes = BASE64
        .decode(&parsed.transaction)
        .map_err(|e| JsonRpcError::invalid_params(format!("transaction: base64 decode: {e}")))?;
    let envelope =
        soroban_env_host::xdr::TransactionEnvelope::from_xdr(&envelope_bytes, Limits::none())
            .map_err(|e| JsonRpcError::invalid_params(format!("transaction: XDR decode: {e}")))?;

    let (host_function, source_account) = extract_invoke_op(&envelope)?;
    let envelope_b64 = parsed.transaction.clone();

    let send_reply = state
        .actor
        .send(|tx| Command::SendTransaction {
            envelope_bytes,
            host_function,
            source_account,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    // Pull the latest-ledger metadata in a separate command — keeps
    // the actor message focused on the work it has to do, and the
    // wire response gets the live ledger info clients expect.
    let latest = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let close_time = state
        .actor
        .send(|tx| Command::GetLedgersPage { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?
        .close_time;

    let (status, error_message) = match &send_reply.receipt.result {
        Ok(_) => ("SUCCESS", None),
        Err(msg) => ("ERROR", Some(msg.clone())),
    };

    let body = SendTransactionResponse {
        status,
        hash: hex_lower(&send_reply.hash),
        latest_ledger: latest.sequence,
        latest_ledger_close_time: close_time.to_string(),
        envelope_xdr: envelope_b64,
        error_message,
        applied_changes: send_reply.receipt.applied_changes,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_get_transaction(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let parsed: GetTransactionParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("getTransaction params: {e}")))?;

    let hash = parse_hex32(&parsed.hash)
        .ok_or_else(|| JsonRpcError::invalid_params("hash: must be 64-char hex"))?;

    let receipt = state
        .actor
        .send(|tx| Command::GetTransaction { hash, reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;
    let latest = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let body = match receipt {
        None => GetTransactionResponse {
            status: "NOT_FOUND",
            latest_ledger: latest.sequence,
            ledger: None,
            created_at: None,
            envelope_xdr: None,
            return_value_xdr: None,
            error_message: None,
            applied_changes: None,
        },
        Some(r) => {
            let envelope_xdr = Some(BASE64.encode(&r.envelope_bytes));
            match &r.result {
                Ok(scval) => {
                    let bytes = scval.to_xdr(Limits::none()).map_err(|e| {
                        JsonRpcError::internal_error(format!("encode return ScVal: {e}"))
                    })?;
                    GetTransactionResponse {
                        status: "SUCCESS",
                        latest_ledger: latest.sequence,
                        ledger: Some(r.ledger),
                        created_at: Some(r.created_at.to_string()),
                        envelope_xdr,
                        return_value_xdr: Some(BASE64.encode(&bytes)),
                        error_message: None,
                        applied_changes: Some(r.applied_changes),
                    }
                }
                Err(msg) => GetTransactionResponse {
                    status: "FAILED",
                    latest_ledger: latest.sequence,
                    ledger: Some(r.ledger),
                    created_at: Some(r.created_at.to_string()),
                    envelope_xdr,
                    return_value_xdr: None,
                    error_message: Some(msg.clone()),
                    applied_changes: Some(r.applied_changes),
                },
            }
        }
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_fork_set_ledger_entry(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let parsed: SetLedgerEntryParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("fork_setLedgerEntry params: {e}")))?;

    let key_bytes = BASE64
        .decode(&parsed.key)
        .map_err(|e| JsonRpcError::invalid_params(format!("key: base64 decode: {e}")))?;
    let key = soroban_env_host::xdr::LedgerKey::from_xdr(&key_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("key: XDR decode: {e}")))?;

    let entry_bytes = BASE64
        .decode(&parsed.entry)
        .map_err(|e| JsonRpcError::invalid_params(format!("entry: base64 decode: {e}")))?;
    let entry = soroban_env_host::xdr::LedgerEntry::from_xdr(&entry_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("entry: XDR decode: {e}")))?;

    state
        .actor
        .send(|tx| Command::SetLedgerEntry {
            key,
            entry,
            live_until: parsed.live_until_ledger_seq,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let latest = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let body = SetLedgerEntryResponse {
        ok: true,
        latest_ledger: latest.sequence,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

/// `fork_setStorage` — sugar over `fork_setLedgerEntry` for the
/// common case of writing a value into a contract's storage.
///
/// The handler builds the `LedgerKey::ContractData` +
/// `LedgerEntry::ContractData` server-side from the inputs
/// (contract strkey + key/value ScVal + durability), then routes
/// to the same `Command::SetLedgerEntry` the primitive uses. No
/// new actor command — the worker does not need to know whether
/// the entry came from the primitive or this wrapper.
///
/// `last_modified_ledger_seq` is set to `0`. The host treats it as
/// metadata for caching (the same `getLedgerEntries` response
/// surfaces this value back), and it doesn't affect any host-side
/// decisions during simulation. Setting to `0` keeps the wrapper
/// honest about being synthesised — there's no real ledger close
/// behind this write.
async fn handle_fork_set_storage(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    use soroban_env_host::xdr::{
        ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, Hash, LedgerEntry,
        LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyContractData, ScAddress, ScVal,
    };

    let parsed: SetStorageParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("fork_setStorage params: {e}")))?;

    let contract_strkey = stellar_strkey::Contract::from_string(&parsed.contract).map_err(|e| {
        JsonRpcError::invalid_params(format!("contract: not a valid C... strkey: {e}"))
    })?;
    let contract_address = ScAddress::Contract(ContractId(Hash(contract_strkey.0)));

    let key_bytes = BASE64
        .decode(&parsed.key)
        .map_err(|e| JsonRpcError::invalid_params(format!("key: base64 decode: {e}")))?;
    let key_scval = ScVal::from_xdr(&key_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("key: ScVal XDR decode: {e}")))?;

    let value_bytes = BASE64
        .decode(&parsed.value)
        .map_err(|e| JsonRpcError::invalid_params(format!("value: base64 decode: {e}")))?;
    let value_scval = ScVal::from_xdr(&value_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("value: ScVal XDR decode: {e}")))?;

    let durability = match parsed.durability.unwrap_or_default() {
        StorageDurability::Persistent => ContractDataDurability::Persistent,
        StorageDurability::Temporary => ContractDataDurability::Temporary,
    };

    let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: contract_address.clone(),
        key: key_scval.clone(),
        durability,
    });
    let ledger_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract_address,
            key: key_scval,
            durability,
            val: value_scval,
        }),
        ext: LedgerEntryExt::V0,
    };

    state
        .actor
        .send(|tx| Command::SetLedgerEntry {
            key: ledger_key,
            entry: ledger_entry,
            live_until: parsed.live_until_ledger_seq,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let latest = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let body = SetLedgerEntryResponse {
        ok: true,
        latest_ledger: latest.sequence,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

/// `fork_setCode` — sugar over `fork_setLedgerEntry` for uploading
/// WASM bytes as a `ContractCode` entry.
///
/// The entry's lookup hash is sha256 of the bytes — server-derived
/// so a malicious or buggy client can't install bytes under a
/// different hash than the host would compute. The hash is echoed
/// back in the response so callers can wire a `CreateContract` to
/// point at this code, or compose with `fork_setStorage` over the
/// contract's instance ScVal for a full etch-equivalent.
///
/// `last_modified_ledger_seq` is set to `0` for the same reason
/// `fork_setStorage` does: the wrapper is honest about being
/// synthesised — there's no real ledger close behind this write.
async fn handle_fork_set_code(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    use sha2::{Digest, Sha256};
    use soroban_env_host::xdr::{
        ContractCodeEntry, ContractCodeEntryExt, Hash, LedgerEntry, LedgerEntryData,
        LedgerEntryExt, LedgerKey, LedgerKeyContractCode,
    };

    let parsed: SetCodeParams = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("fork_setCode params: {e}")))?;

    let wasm = BASE64
        .decode(&parsed.wasm)
        .map_err(|e| JsonRpcError::invalid_params(format!("wasm: base64 decode: {e}")))?;

    let hash_bytes: [u8; 32] = Sha256::digest(&wasm).into();
    let hash = Hash(hash_bytes);

    let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode { hash: hash.clone() });
    let ledger_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash,
            code: wasm.try_into().map_err(|_| {
                JsonRpcError::invalid_params("wasm: bytes exceed XDR BytesM<u32::MAX> capacity")
            })?,
        }),
        ext: LedgerEntryExt::V0,
    };

    state
        .actor
        .send(|tx| Command::SetLedgerEntry {
            key: ledger_key,
            entry: ledger_entry,
            live_until: parsed.live_until_ledger_seq,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let latest = state
        .actor
        .send(|tx| Command::GetLatestLedger { reply: tx })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let body = SetCodeResponse {
        ok: true,
        hash: hex_lower(&hash_bytes),
        latest_ledger: latest.sequence,
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

async fn handle_fork_close_ledgers(
    state: &AppState,
    params: &serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    // Defaults: 1 ledger, +5s — Stellar's average ledger close rate.
    // Both fields optional; null/absent params is valid (close one
    // ledger of default duration).
    let parsed: CloseLedgersParams = if params.is_null() {
        CloseLedgersParams {
            ledgers: None,
            timestamp_advance_seconds: None,
        }
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("fork_closeLedgers params: {e}")))?
    };
    let ledgers = parsed.ledgers.unwrap_or(1);
    let timestamp_advance_seconds = parsed
        .timestamp_advance_seconds
        .unwrap_or(ledgers as u64 * 5);

    let reply = state
        .actor
        .send(|tx| Command::CloseLedgers {
            ledgers,
            timestamp_advance_seconds,
            reply: tx,
        })
        .await
        .map_err(|e| JsonRpcError::internal_error(e.to_string()))?;

    let body = CloseLedgersResponse {
        new_sequence: reply.new_sequence,
        new_close_time: reply.new_close_time.to_string(),
    };
    serde_json::to_value(body).map_err(|e| JsonRpcError::internal_error(e.to_string()))
}

/// Lower-case hex of a 32-byte hash for the `hash` wire field.
fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Inverse of `hex_lower` for `getTransaction` lookups. Returns `None`
/// on any malformed input rather than threading a typed error — the
/// caller turns it into `invalid_params`.
fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
