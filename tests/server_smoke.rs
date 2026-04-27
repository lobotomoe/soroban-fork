//! End-to-end server smoke tests.
//!
//! Boots an in-process server bound to `127.0.0.1:0` (OS-assigned port),
//! sends real JSON-RPC requests via `reqwest`, asserts on response
//! shapes against live mainnet state.
//!
//! Tests are `#[ignore]` so offline CI passes; opt in via:
//! ```sh
//! cargo test --features server --test server_smoke -- --ignored
//! ```

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::Value;
use soroban_env_host::xdr::{
    AccountId, ContractDataDurability, ContractId, Hash, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerKey, LedgerKeyContractData, Limits, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol, ScVal, SequenceNumber,
    Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, WriteXdr,
};
use soroban_fork::{server::Server, ForkConfig};

const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

fn mainnet_rpc() -> String {
    std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".to_string())
}

/// Decode a strkey-encoded contract ID (`C...`) into the underlying 32
/// bytes. The integration test stays self-contained — we don't pull in
/// `stellar-strkey` for tests since the crate already exposes encoding,
/// not decoding, on its public surface.
fn decode_contract_id(strkey: &str) -> [u8; 32] {
    let parsed: stellar_strkey::Contract = strkey.parse().expect("valid contract strkey");
    parsed.0
}

/// Build the dummy AccountId we use as the source for simulation. The
/// host doesn't enforce signatures during recording-mode invocation, so
/// any well-formed account works; we use the all-zeros key to make the
/// test deterministic.
fn dummy_source_account() -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])))
}

/// POST a JSON-RPC envelope to the running server and parse the
/// response as `serde_json::Value`. Panics on transport failure — the
/// test is in-process so any error is a real bug.
async fn jsonrpc_call(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Value {
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = client
        .post(url)
        .json(&envelope)
        .send()
        .await
        .expect("http request");
    assert!(resp.status().is_success(), "HTTP status not 2xx");
    resp.json::<Value>().await.expect("decode response JSON")
}

/// Spin up the server on an ephemeral port; return its URL plus the
/// running handle so the test can shut it down at the end.
async fn start_test_server() -> (String, soroban_fork::server::RunningServer) {
    let _ = env_logger::Builder::from_env(env_logger::Env::default())
        .is_test(true)
        .try_init();

    let config = ForkConfig::new(mainnet_rpc());
    let running = Server::builder(config)
        .listen("127.0.0.1:0".parse().unwrap())
        .start()
        .await
        .expect("server start");
    let url = format!("http://{}", running.local_addr());
    (url, running)
}

/// `getHealth`, `getVersionInfo`, `getNetwork`, `getLatestLedger` —
/// confirm the server is reachable, the wire shapes parse, and the
/// values reflect a live mainnet fork (passphrase matches Stellar
/// mainnet).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_serves_basic_metadata() {
    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    let health = jsonrpc_call(&client, &url, "getHealth", Value::Null).await;
    assert_eq!(health["result"]["status"], "healthy");
    assert!(health["result"]["latestLedger"].as_u64().unwrap() > 0);

    let version = jsonrpc_call(&client, &url, "getVersionInfo", Value::Null).await;
    assert_eq!(version["result"]["protocolVersion"], 25);

    let network = jsonrpc_call(&client, &url, "getNetwork", Value::Null).await;
    assert_eq!(
        network["result"]["passphrase"].as_str().unwrap(),
        "Public Global Stellar Network ; September 2015"
    );
    assert_eq!(network["result"]["protocolVersion"], 25);
    assert_eq!(
        network["result"]["networkId"].as_str().unwrap().len(),
        64,
        "network ID must be 32-byte hex (64 chars)"
    );

    let latest = jsonrpc_call(&client, &url, "getLatestLedger", Value::Null).await;
    assert!(latest["result"]["sequence"].as_u64().unwrap() > 0);

    running.shutdown().await.expect("shutdown");
}

/// `getLedgerEntries` for the XLM SAC contract instance — reaches the
/// upstream RPC if the cache is cold, returns the entry. Verifies the
/// wire shape: `key` (echo), `xdr` (base64 LedgerEntryData),
/// `lastModifiedLedgerSeq`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_resolves_real_ledger_key() {
    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    // LedgerKey for the XLM SAC's contract-instance entry.
    let xlm_id = decode_contract_id(XLM_SAC);
    let key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash(xlm_id))),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    });
    let key_xdr = key.to_xdr(Limits::none()).expect("encode key");
    let key_b64 = BASE64.encode(&key_xdr);

    let resp = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [key_b64.clone()] }),
    )
    .await;

    let result = &resp["result"];
    assert!(result["latestLedger"].as_u64().unwrap() > 0);
    let entries = result["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "XLM SAC instance must exist on mainnet");
    let entry = &entries[0];
    assert_eq!(entry["key"].as_str().unwrap(), key_b64);
    assert!(!entry["xdr"].as_str().unwrap().is_empty());
    assert!(entry["lastModifiedLedgerSeq"].as_u64().unwrap() > 0);

    running.shutdown().await.expect("shutdown");
}

/// `simulateTransaction` for `XLM_SAC.decimals()` — the simplest
/// possible Soroban call. Verifies the response carries
/// `results[0].xdr` decoding to `ScVal::U32(7)`, plus a non-empty
/// footprint and `transactionData`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_simulates_xlm_decimals() {
    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    // Build an InvokeHostFunction op calling `decimals()` on XLM SAC.
    let xlm_id = decode_contract_id(XLM_SAC);
    let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(xlm_id))),
        function_name: ScSymbol("decimals".try_into().unwrap()),
        args: vec![].try_into().unwrap(),
    });
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: host_fn,
            auth: vec![].try_into().unwrap(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
        fee: 0,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().unwrap(),
    });
    let envelope_xdr = envelope.to_xdr(Limits::none()).expect("encode envelope");
    let envelope_b64 = BASE64.encode(&envelope_xdr);

    let resp = jsonrpc_call(
        &client,
        &url,
        "simulateTransaction",
        serde_json::json!({ "transaction": envelope_b64 }),
    )
    .await;

    eprintln!("\nsimulateTransaction response:\n{resp:#}\n");

    // Wire shape assertions
    assert!(
        resp["result"]["error"].is_null(),
        "simulation should not have errored"
    );
    let results = resp["result"]["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "single-op tx → single result");

    // Decode the return value: ScVal::U32(7)
    let xdr_b64 = results[0]["xdr"].as_str().expect("results[0].xdr");
    let xdr_bytes = BASE64.decode(xdr_b64).expect("base64 decode");
    let scval =
        soroban_env_host::xdr::ScVal::from_xdr(&xdr_bytes, soroban_env_host::xdr::Limits::none())
            .expect("decode ScVal");
    match scval {
        ScVal::U32(7) => {} // expected
        other => panic!("expected U32(7) for XLM.decimals(), got {other:?}"),
    }

    // transactionData must be present with a non-empty footprint.
    assert!(
        resp["result"]["transactionData"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "transactionData should be a non-empty base64 string"
    );

    // Cost should be reported with REAL host-budget numbers — not the
    // pre-v0.5.2 stub where memBytes was a write_bytes proxy.
    let cpu_str = resp["result"]["cost"]["cpuInsns"]
        .as_str()
        .expect("cost.cpuInsns");
    let cpu: u64 = cpu_str.parse().expect("cpuInsns parses as u64");
    assert!(cpu > 0, "decimals() should consume non-zero CPU");

    let mem_str = resp["result"]["cost"]["memBytes"]
        .as_str()
        .expect("cost.memBytes");
    let mem: u64 = mem_str.parse().expect("memBytes parses as u64");
    // `decimals()` is a pure read — `write_bytes` is 0. The pre-v0.5.2
    // proxy would report 0 here; the real Budget always reports
    // non-zero memory for any host invocation.
    assert!(
        mem > 0,
        "memBytes should reflect real host memory consumption, got {mem}"
    );

    // `minResourceFee` should be the live mainnet fee schedule applied
    // to this transaction — non-zero, fits in i64, sourced from the
    // six on-chain ConfigSetting entries. The pre-v0.5.2 stub was "0".
    let fee_str = resp["result"]["minResourceFee"]
        .as_str()
        .expect("minResourceFee");
    let fee: i64 = fee_str.parse().expect("minResourceFee parses as i64");
    assert!(
        fee > 0,
        "minResourceFee should be derived from live fee schedule, got {fee}"
    );

    running.shutdown().await.expect("shutdown");
}

/// Unknown method → `-32601 Method not found`. Confirms the dispatch
/// catch-all is reachable and the JSON-RPC error body shape is correct.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_returns_method_not_found_for_unknown() {
    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    let resp = jsonrpc_call(&client, &url, "sendTransaction", serde_json::json!({})).await;
    assert_eq!(resp["error"]["code"], -32601);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sendTransaction"));

    running.shutdown().await.expect("shutdown");
}

// `dummy_source_account` is referenced from a docstring example only;
// silence the unused-but-needed-as-sample warning.
#[allow(dead_code)]
fn _keep_dummy_source_in_scope() -> AccountId {
    dummy_source_account()
}
