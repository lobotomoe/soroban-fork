use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use stellar_xdr::curr::{
    LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, ReadXdr, WriteXdr,
};

const MAX_KEYS_PER_REQUEST: usize = 200;
const MAX_RETRIES: u32 = 3;
const BASE_RETRY_DELAY_MS: u64 = 300;

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

pub struct LedgerInfo {
    pub sequence: u32,
    pub protocol_version: u32,
    pub timestamp: u64,
    pub network_id: [u8; 32],
}

pub struct FetchedEntry {
    #[allow(dead_code)]
    pub key: LedgerKey,
    pub entry: LedgerEntry,
    pub live_until: Option<u32>,
}

/// Execute a JSON-RPC call with exponential backoff retry on transient errors.
fn rpc_post<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
    request: &JsonRpcRequest,
) -> Result<JsonRpcResponse<T>, String> {
    for attempt in 0..=MAX_RETRIES {
        let response = match client.post(rpc_url).json(request).send() {
            Ok(r) => r,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                eprintln!(
                    "[soroban-fork] RPC request error (attempt {}/{}): {e}, retrying in {delay:?}",
                    attempt + 1,
                    MAX_RETRIES
                );
                std::thread::sleep(delay);
                continue;
            }
            Err(e) => {
                return Err(format!(
                    "RPC request failed after {MAX_RETRIES} retries: {e}"
                ))
            }
        };

        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            if attempt < MAX_RETRIES {
                let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                eprintln!(
                    "[soroban-fork] RPC HTTP {status} (attempt {}/{}), retrying in {delay:?}",
                    attempt + 1,
                    MAX_RETRIES
                );
                std::thread::sleep(delay);
                continue;
            }
            return Err(format!("RPC HTTP {status} after {MAX_RETRIES} retries"));
        }

        match response.json::<JsonRpcResponse<T>>() {
            Ok(parsed) => return Ok(parsed),
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                eprintln!(
                    "[soroban-fork] RPC parse error (attempt {}/{}): {e}, retrying in {delay:?}",
                    attempt + 1,
                    MAX_RETRIES
                );
                std::thread::sleep(delay);
                continue;
            }
            Err(e) => {
                return Err(format!(
                    "RPC response parse failed after {MAX_RETRIES} retries: {e}"
                ))
            }
        }
    }
    unreachable!()
}

/// Fetch ledger entries from the Soroban RPC.
/// Batches into chunks of 200 (RPC limit). Retries on transient errors.
pub fn fetch_entries(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
    keys: &[LedgerKey],
) -> Result<Vec<FetchedEntry>, String> {
    let mut results = Vec::new();

    for chunk in keys.chunks(MAX_KEYS_PER_REQUEST) {
        let encoded_keys: Vec<String> = chunk
            .iter()
            .map(|k| {
                let xdr_bytes = k
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .map_err(|e| format!("XDR encode error: {e}"))?;
                Ok(BASE64.encode(&xdr_bytes))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getLedgerEntries",
            params: serde_json::json!({ "keys": encoded_keys }),
        };

        let response: JsonRpcResponse<GetLedgerEntriesResult> =
            rpc_post(client, rpc_url, &request)?;

        if let Some(err) = response.error {
            return Err(format!("RPC error: {err}"));
        }

        let result = response.result.ok_or("RPC returned no result")?;

        if let Some(entries) = result.entries {
            for entry in entries {
                let key_bytes = BASE64
                    .decode(&entry.key)
                    .map_err(|e| format!("base64 decode key: {e}"))?;
                let key = LedgerKey::from_xdr(key_bytes, stellar_xdr::curr::Limits::none())
                    .map_err(|e| format!("XDR decode key: {e}"))?;

                let entry_bytes = BASE64
                    .decode(&entry.xdr)
                    .map_err(|e| format!("base64 decode entry: {e}"))?;

                // RPC returns LedgerEntryData (not full LedgerEntry).
                // We wrap it with the per-entry lastModifiedLedgerSeq from the response.
                let entry_data =
                    LedgerEntryData::from_xdr(entry_bytes, stellar_xdr::curr::Limits::none())
                        .map_err(|e| format!("XDR decode LedgerEntryData: {e}"))?;
                let ledger_entry = LedgerEntry {
                    last_modified_ledger_seq: entry.last_modified_ledger_seq,
                    data: entry_data,
                    ext: LedgerEntryExt::V0,
                };

                results.push(FetchedEntry {
                    key,
                    entry: ledger_entry,
                    live_until: entry.live_until_ledger_seq,
                });
            }
        }
    }

    Ok(results)
}

/// Fetch a single ledger entry. Returns None if not found.
pub fn fetch_entry(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
    key: &LedgerKey,
) -> Result<Option<FetchedEntry>, String> {
    let mut entries = fetch_entries(client, rpc_url, std::slice::from_ref(key))?;
    Ok(entries.pop())
}

/// Get the latest ledger info from the RPC.
pub fn get_latest_ledger(
    client: &reqwest::blocking::Client,
    rpc_url: &str,
) -> Result<LedgerInfo, String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "getLatestLedger",
        params: serde_json::json!({}),
    };

    let response: JsonRpcResponse<GetLatestLedgerResult> = rpc_post(client, rpc_url, &request)?;

    if let Some(err) = response.error {
        return Err(format!("RPC error: {err}"));
    }

    let result = response.result.ok_or("RPC returned no result")?;

    let network_id = [0u8; 32]; // placeholder, set by ForkConfig

    Ok(LedgerInfo {
        sequence: result.sequence,
        protocol_version: result.protocol_version,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        network_id,
    })
}
