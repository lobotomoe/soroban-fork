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
    AccountId, ContractDataDurability, ContractId, Hash, HostFunction, Int128Parts,
    InvokeContractArgs, InvokeHostFunctionOp, LedgerKey, LedgerKeyContractData, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, PublicKey, ReadXdr, ScAddress, ScSymbol,
    ScVal, SequenceNumber, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, WriteXdr,
};
use soroban_fork::{server::Server, ForkConfig};

const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

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

    // `getEvents` is not in the v0.6 method set (planned for later).
    // Real Stellar RPC implements it; ours doesn't yet, so it should
    // hit the dispatch catch-all and return -32601.
    let resp = jsonrpc_call(&client, &url, "getEvents", serde_json::json!({})).await;
    assert_eq!(resp["error"]["code"], -32601);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("getEvents"));

    running.shutdown().await.expect("shutdown");
}

/// End-to-end write-persistence test:
///
/// 1. Build a `USDC.mint(alice, AMOUNT)` envelope.
/// 2. POST `sendTransaction` — expect status `"SUCCESS"` and
///    `appliedChanges > 0` (real writes hit the snapshot source).
/// 3. POST `getTransaction(hash)` — receipt is retrievable, status
///    matches.
/// 4. POST `simulateTransaction` for `USDC.balance(alice)` — must
///    return the minted amount, proving that the post-send snapshot
///    source surfaced the write to a fresh recording-mode sandbox.
///
/// Auth note: the worker runs `RecordingInvocationAuthMode::Recording(false)`,
/// which bypasses non-root authorizations. The SAC's admin
/// `require_auth` for `mint` is recorded but not enforced — same UX
/// as Anvil's default trust mode. Real `sendTransaction` against
/// stellar-rpc would reject this without a signed admin envelope.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_send_transaction_persists_state() {
    // 1,000,000 USDC at 7-decimal scale.
    const AMOUNT: i128 = 1_000_000 * 10_000_000;

    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    // Recipient is a synthetic *contract* address. Stellar SACs
    // require trustline entries for account recipients (we observed
    // `Error(Contract, #13)` "trustline entry is missing" when we
    // tried minting to an Account ScAddress) — but contract
    // recipients skip that check, which mirrors how DeFi vaults and
    // pools receive SAC tokens in production.
    let mut contract_id = [0u8; 32];
    contract_id[0] = 0xa1;
    contract_id[1] = 0x1c;
    contract_id[2] = 0xe0;
    let alice_addr = ScAddress::Contract(ContractId(Hash(contract_id)));

    let usdc_id = decode_contract_id(USDC_SAC);
    let mint_envelope_b64 = build_invoke_envelope(
        &usdc_id,
        "mint",
        vec![ScVal::Address(alice_addr.clone()), i128_to_scval(AMOUNT)],
    );

    // 1. sendTransaction
    let send_resp = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": mint_envelope_b64 }),
    )
    .await;
    eprintln!("\nsendTransaction(mint) response:\n{send_resp:#}\n");

    assert_eq!(
        send_resp["result"]["status"], "SUCCESS",
        "mint should succeed in trust-mode (Recording(false)); error: {:?}",
        send_resp["result"]["errorResultXdr"]
    );
    let applied: u64 = send_resp["result"]["appliedChanges"]
        .as_u64()
        .expect("appliedChanges field");
    assert!(
        applied > 0,
        "mint must apply at least one ledger change, got {applied}"
    );
    let hash = send_resp["result"]["hash"]
        .as_str()
        .expect("hash field")
        .to_string();
    assert_eq!(hash.len(), 64, "hash should be 32-byte hex (64 chars)");

    // 2. getTransaction(hash) — receipt round-trip
    let get_resp = jsonrpc_call(
        &client,
        &url,
        "getTransaction",
        serde_json::json!({ "hash": hash }),
    )
    .await;
    assert_eq!(
        get_resp["result"]["status"], "SUCCESS",
        "receipt status must reflect mint success"
    );
    assert!(
        get_resp["result"]["envelopeXdr"].is_string(),
        "receipt should echo envelopeXdr"
    );
    assert_eq!(
        get_resp["result"]["appliedChanges"], applied,
        "receipt's appliedChanges should match send response"
    );

    // 3. simulateTransaction(USDC.balance(alice)) — proves the mint
    //    persisted into the snapshot source. A fresh recording-mode
    //    sandbox reads from the same source and must see the new
    //    balance entry.
    let balance_envelope_b64 = build_invoke_envelope(
        &usdc_id,
        "balance",
        vec![ScVal::Address(alice_addr.clone())],
    );
    let bal_resp = jsonrpc_call(
        &client,
        &url,
        "simulateTransaction",
        serde_json::json!({ "transaction": balance_envelope_b64 }),
    )
    .await;
    assert!(
        bal_resp["result"]["error"].is_null(),
        "balance simulation should not error; got {bal_resp:#}"
    );

    let xdr_b64 = bal_resp["result"]["results"][0]["xdr"]
        .as_str()
        .expect("balance simulation results[0].xdr");
    let xdr_bytes = BASE64.decode(xdr_b64).expect("base64 decode");
    let scval = ScVal::from_xdr(&xdr_bytes, Limits::none()).expect("decode ScVal");
    let bal = match scval {
        ScVal::I128(parts) => ((parts.hi as i128) << 64) | (parts.lo as i128),
        other => panic!("expected I128 from balance(), got {other:?}"),
    };
    assert_eq!(
        bal, AMOUNT,
        "balance must reflect the mint we just sent (writes persisted)"
    );

    running.shutdown().await.expect("shutdown");
}

/// Helper: build a base64-XDR `TransactionEnvelope` carrying a single
/// `InvokeHostFunctionOp` against `(contract_id, function_name, args)`.
/// Source account is the all-zeros key (recording-mode doesn't care).
fn build_invoke_envelope(contract_id: &[u8; 32], function_name: &str, args: Vec<ScVal>) -> String {
    let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(*contract_id))),
        function_name: ScSymbol(function_name.try_into().expect("symbol fits")),
        args: args.try_into().expect("args fit VecM"),
    });
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: host_fn,
            auth: vec![].try_into().expect("empty auth"),
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
        fee: 0,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().expect("ops fit VecM"),
        ext: TransactionExt::V0,
    };
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().expect("empty signatures"),
    });
    BASE64.encode(envelope.to_xdr(Limits::none()).expect("encode envelope"))
}

/// Helper: encode an `i128` as `ScVal::I128(Int128Parts { hi, lo })`.
fn i128_to_scval(v: i128) -> ScVal {
    ScVal::I128(Int128Parts {
        hi: (v >> 64) as i64,
        lo: v as u64,
    })
}

/// `getTransaction` for an unknown hash returns `NOT_FOUND`. Confirms
/// the receipt store correctly distinguishes "never sent" from "sent
/// and failed".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_get_transaction_unknown_hash_is_not_found() {
    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    let zero_hash = "0".repeat(64);
    let resp = jsonrpc_call(
        &client,
        &url,
        "getTransaction",
        serde_json::json!({ "hash": zero_hash }),
    )
    .await;
    assert_eq!(resp["result"]["status"], "NOT_FOUND");
    assert!(
        resp["result"]["envelopeXdr"].is_null(),
        "NOT_FOUND should not carry an envelope"
    );

    running.shutdown().await.expect("shutdown");
}

/// Pre-funded test accounts: verify the fork minted them at build
/// time, expose them via `getLedgerEntries`, and confirm the seq_num
/// increments after each successful `sendTransaction` so chained
/// envelopes (the JS-SDK pattern of `getAccount` → build → send →
/// `getAccount` → build → send) line up correctly.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_pre_funded_account_seq_increments() {
    use soroban_env_host::xdr::LedgerEntryData;
    use soroban_fork::test_accounts;

    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    // Account 0 from the deterministic generator — same seed the
    // server's actor uses, so the LedgerKey we build here resolves
    // against an entry the fork pre-populated.
    let accounts = test_accounts::generate(1);
    let account_0 = &accounts[0];
    let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        account_0.public_key,
    )));
    let account_key = LedgerKey::Account(soroban_env_host::xdr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });
    let account_key_b64 = BASE64.encode(account_key.to_xdr(Limits::none()).unwrap());

    // 1. Pre-funded entry exists, balance is 100K XLM, seq is at
    //    fork-point shifted (Stellar convention: ledger << 32).
    let resp = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [account_key_b64.clone()] }),
    )
    .await;
    let entries = resp["result"]["entries"].as_array().expect("entries array");
    assert_eq!(
        entries.len(),
        1,
        "pre-funded test account 0 should be in the snapshot source"
    );

    let initial_seq = parse_account_seq(&entries[0]);
    let initial_balance = parse_account_balance(&entries[0]);
    assert_eq!(
        initial_balance,
        100_000 * 10_000_000,
        "test account 0 should hold the default 100K XLM balance"
    );
    assert!(
        initial_seq > 0,
        "seq should be ledger << 32, not zero (got {initial_seq})"
    );

    // 2. Send a tx FROM account 0. Use the simplest invocation —
    //    XLM SAC's `decimals()` — so success is independent of any
    //    auth or balance dynamics. A successful host invocation
    //    must bump the source's seq_num.
    let xlm_id = decode_contract_id(XLM_SAC);
    let envelope_b64 =
        build_invoke_envelope_with_source(&xlm_id, "decimals", vec![], &account_0.public_key);
    let send_resp = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": envelope_b64 }),
    )
    .await;
    assert_eq!(
        send_resp["result"]["status"], "SUCCESS",
        "decimals() send should succeed; got {send_resp:#}"
    );

    // 3. Re-read the account; seq should have advanced by exactly 1.
    let resp_after = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [account_key_b64.clone()] }),
    )
    .await;
    let entries_after = resp_after["result"]["entries"].as_array().unwrap();
    let after_seq = parse_account_seq(&entries_after[0]);
    assert_eq!(
        after_seq,
        initial_seq + 1,
        "seq_num should bump by exactly 1 per successful send"
    );

    // 4. Send another tx; seq advances again. Proves the bump is
    //    not a one-shot — two consecutive sends from the same
    //    account both advance state.
    let envelope_b64_2 =
        build_invoke_envelope_with_source(&xlm_id, "decimals", vec![], &account_0.public_key);
    let send_resp_2 = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": envelope_b64_2 }),
    )
    .await;
    assert_eq!(send_resp_2["result"]["status"], "SUCCESS");
    let resp_after_2 = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [account_key_b64] }),
    )
    .await;
    let entries_after_2 = resp_after_2["result"]["entries"].as_array().unwrap();
    let after_seq_2 = parse_account_seq(&entries_after_2[0]);
    assert_eq!(
        after_seq_2,
        initial_seq + 2,
        "two successful sends → seq advances by 2"
    );

    let _ = LedgerEntryData::Account; // keep the import alive at the test-data parser site
    running.shutdown().await.expect("shutdown");
}

/// Same as [`build_invoke_envelope`] but lets the caller specify a
/// non-zero source-account ed25519 public key — required for the
/// pre-funded-accounts test where the source must match a real
/// AccountEntry in the cache.
fn build_invoke_envelope_with_source(
    contract_id: &[u8; 32],
    function_name: &str,
    args: Vec<ScVal>,
    source_pk: &[u8; 32],
) -> String {
    let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(*contract_id))),
        function_name: ScSymbol(function_name.try_into().expect("symbol fits")),
        args: args.try_into().expect("args fit VecM"),
    });
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: host_fn,
            auth: vec![].try_into().expect("empty auth"),
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_pk)),
        fee: 0,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().expect("ops fit VecM"),
        ext: TransactionExt::V0,
    };
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().expect("empty signatures"),
    });
    BASE64.encode(envelope.to_xdr(Limits::none()).expect("encode envelope"))
}

/// Decode the JSON-RPC `getLedgerEntries` result item's base64-XDR
/// `LedgerEntryData::Account` and return its sequence number.
fn parse_account_seq(item: &Value) -> i64 {
    use soroban_env_host::xdr::LedgerEntryData;
    let xdr_b64 = item["xdr"].as_str().expect("xdr field");
    let bytes = BASE64.decode(xdr_b64).expect("base64 decode");
    let data = LedgerEntryData::from_xdr(&bytes, Limits::none()).expect("decode LedgerEntryData");
    match data {
        LedgerEntryData::Account(a) => a.seq_num.0,
        other => panic!("expected Account entry, got {other:?}"),
    }
}

/// Decode the same item's balance.
fn parse_account_balance(item: &Value) -> i64 {
    use soroban_env_host::xdr::LedgerEntryData;
    let xdr_b64 = item["xdr"].as_str().expect("xdr field");
    let bytes = BASE64.decode(xdr_b64).expect("base64 decode");
    match LedgerEntryData::from_xdr(&bytes, Limits::none()).expect("decode LedgerEntryData") {
        LedgerEntryData::Account(a) => a.balance,
        other => panic!("expected Account entry, got {other:?}"),
    }
}

/// Full deploy + invoke flow against a forked mainnet:
///
/// 1. `sendTransaction(UploadContractWasm)` — install a tiny custom
///    `add(a: i32, b: i32) -> i32` WASM. The receipt's
///    `returnValueXdr` is the wasm hash.
/// 2. `sendTransaction(CreateContract)` — instantiate the deployed
///    contract from that hash, salt zero, source = test account 0.
///    Receipt's `returnValueXdr` is the new `ScAddress::Contract`.
/// 3. `simulateTransaction(InvokeContract { contract, "add", [2, 3] })`
///    — call the deployed function. Return value must be `I32(5)`.
///
/// Proves the user's full v0.7 workflow: deploy a custom contract on
/// the fork, then invoke it. The cross-protocol case (custom contract
/// calls a mainnet contract) follows from this — once a contract is
/// on the fork, normal cross-contract calls just work because lazy
/// fetch resolves the dependency.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_deploy_and_invoke_custom_contract() {
    use soroban_env_host::xdr::{
        ContractExecutable, ContractIdPreimage, ContractIdPreimageFromAddress, CreateContractArgs,
    };
    use soroban_fork::test_accounts;

    /// Tiny precompiled Soroban contract exporting `add(i32, i32) -> i32`.
    /// Same WASM env-host's own metering benchmark uses; copied
    /// into our fixtures dir so we don't depend on registry paths.
    const ADD_I32_WASM: &[u8] = include_bytes!("fixtures/add_i32.wasm");

    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    let account_0 = &test_accounts::generate(1)[0];
    let source_account_address = ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(
        Uint256(account_0.public_key),
    )));

    // ---- Phase 1: Upload WASM ----
    let upload_envelope_b64 = build_upload_envelope(ADD_I32_WASM, &account_0.public_key);
    let upload_resp = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": upload_envelope_b64 }),
    )
    .await;
    eprintln!("\nUpload response:\n{upload_resp:#}\n");
    assert_eq!(
        upload_resp["result"]["status"], "SUCCESS",
        "WASM upload should succeed"
    );

    // Pull the wasm hash from the receipt's return value.
    let upload_hash = upload_resp["result"]["hash"]
        .as_str()
        .expect("hash field")
        .to_string();
    let upload_receipt = jsonrpc_call(
        &client,
        &url,
        "getTransaction",
        serde_json::json!({ "hash": upload_hash }),
    )
    .await;
    let return_b64 = upload_receipt["result"]["returnValueXdr"]
        .as_str()
        .expect("upload return value");
    let return_bytes = BASE64.decode(return_b64).unwrap();
    let wasm_hash: [u8; 32] = match ScVal::from_xdr(&return_bytes, Limits::none()).unwrap() {
        ScVal::Bytes(b) => b
            .as_slice()
            .try_into()
            .expect("wasm hash should be 32 bytes"),
        other => panic!("expected ScVal::Bytes for wasm hash, got {other:?}"),
    };

    // ---- Phase 2: Create Contract ----
    let create_args = CreateContractArgs {
        contract_id_preimage: ContractIdPreimage::Address(ContractIdPreimageFromAddress {
            address: source_account_address.clone(),
            salt: Uint256([0u8; 32]),
        }),
        executable: ContractExecutable::Wasm(Hash(wasm_hash)),
    };
    let create_envelope_b64 = build_envelope_with_host_fn(
        HostFunction::CreateContract(create_args),
        &account_0.public_key,
    );
    let create_resp = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": create_envelope_b64 }),
    )
    .await;
    eprintln!("\nCreate response:\n{create_resp:#}\n");
    assert_eq!(
        create_resp["result"]["status"], "SUCCESS",
        "CreateContract should succeed"
    );

    // The receipt return value is the new contract's `ScAddress`.
    let create_hash = create_resp["result"]["hash"].as_str().unwrap().to_string();
    let create_receipt = jsonrpc_call(
        &client,
        &url,
        "getTransaction",
        serde_json::json!({ "hash": create_hash }),
    )
    .await;
    let create_return = create_receipt["result"]["returnValueXdr"]
        .as_str()
        .expect("create return value");
    let create_bytes = BASE64.decode(create_return).unwrap();
    let new_contract_id: [u8; 32] = match ScVal::from_xdr(create_bytes, Limits::none()).unwrap() {
        ScVal::Address(ScAddress::Contract(ContractId(Hash(id)))) => id,
        other => panic!("expected ScVal::Address(Contract), got {other:?}"),
    };

    // ---- Phase 3: Invoke deployed contract ----
    let invoke_envelope_b64 = build_invoke_envelope_with_source(
        &new_contract_id,
        "add",
        vec![ScVal::I32(2), ScVal::I32(3)],
        &account_0.public_key,
    );
    let sim_resp = jsonrpc_call(
        &client,
        &url,
        "simulateTransaction",
        serde_json::json!({ "transaction": invoke_envelope_b64 }),
    )
    .await;
    assert!(
        sim_resp["result"]["error"].is_null(),
        "invoke simulation should not error; got {sim_resp:#}"
    );
    let result_xdr = sim_resp["result"]["results"][0]["xdr"]
        .as_str()
        .expect("invoke results[0].xdr");
    let result_bytes = BASE64.decode(result_xdr).unwrap();
    let result_scval = ScVal::from_xdr(result_bytes, Limits::none()).unwrap();
    match result_scval {
        ScVal::I32(5) => {} // 2 + 3
        other => panic!("expected I32(5) from deployed add(2, 3), got {other:?}"),
    }

    eprintln!("Deployed custom contract at {new_contract_id:02x?}; add(2, 3) = 5 ✓");
    running.shutdown().await.expect("shutdown");
}

/// Build a single-op envelope carrying any `HostFunction` (Invoke /
/// Create / Upload), with the given source-account public key.
fn build_envelope_with_host_fn(host_fn: HostFunction, source_pk: &[u8; 32]) -> String {
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: host_fn,
            auth: vec![].try_into().expect("empty auth"),
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_pk)),
        fee: 0,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().expect("ops fit VecM"),
        ext: TransactionExt::V0,
    };
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().expect("empty signatures"),
    });
    BASE64.encode(envelope.to_xdr(Limits::none()).expect("encode envelope"))
}

/// Build an `UploadContractWasm` envelope. Convenience wrapper so the
/// test reads top-down without inline `HostFunction::Upload(...)`
/// boilerplate.
fn build_upload_envelope(wasm: &[u8], source_pk: &[u8; 32]) -> String {
    let host_fn = HostFunction::UploadContractWasm(wasm.to_vec().try_into().expect("wasm fits"));
    build_envelope_with_host_fn(host_fn, source_pk)
}

/// Real-world cross-protocol scenario: a pre-funded test account
/// (the "Anvil-equivalent EOA") swaps live mainnet XLM for USDC on
/// Phoenix DEX through a single `sendTransaction`. After the swap
/// alice's Trustline(USDC) balance must be positive — proving the
/// fork's pre-funded account, USDC trustline, and `apply_changes`
/// pipeline all line up to support the full DEX flow that any
/// frontend would exercise.
///
/// Why this matters: without a pre-created Trustline entry the SAC
/// would fail with `Error(Contract, #13) "trustline missing"` when
/// crediting alice. v0.7's fork build writes that trustline at the
/// same time it writes the Account — same shape mainnet uses post-
/// `ChangeTrust`, no host hacks involved.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
async fn server_test_account_swaps_xlm_for_usdc_on_phoenix() {
    use soroban_env_host::xdr::{AlphaNum4, AssetCode4, TrustLineAsset};
    use soroban_fork::test_accounts;

    /// Phoenix XLM/USDC pair contract.
    const PHOENIX_XLM_USDC: &str = "CBHCRSVX3ZZ7EGTSYMKPEFGZNWRVCSESQR3UABET4MIW52N4EVU6BIZX";
    const USDC_SAC_STR: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    /// 7-decimal stroops: 1000 XLM offer.
    const OFFER_XLM_STROOPS: i128 = 1_000 * 10_000_000;

    let (url, running) = start_test_server().await;
    let client = reqwest::Client::new();

    let account_0 = &test_accounts::generate(1)[0];
    let alice_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        account_0.public_key,
    )));
    let alice_address = ScAddress::Account(alice_account_id.clone());
    let xlm_sac_id = decode_contract_id(XLM_SAC);
    let _ = USDC_SAC_STR; // referenced in the docstring scenario only
    let phoenix_id = decode_contract_id(PHOENIX_XLM_USDC);

    // Build the USDC trustline LedgerKey we'll read before/after.
    // `usdc_mainnet_trustline_asset` is private to the crate, so
    // reconstruct the same shape here from public constants.
    let usdc_issuer_strkey: stellar_strkey::ed25519::PublicKey = test_accounts::USDC_MAINNET_ISSUER
        .parse()
        .expect("issuer strkey parses");
    let usdc_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USDC"),
        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            usdc_issuer_strkey.0,
        ))),
    });
    let usdc_trustline_key = LedgerKey::Trustline(soroban_env_host::xdr::LedgerKeyTrustLine {
        account_id: alice_account_id.clone(),
        asset: usdc_asset,
    });
    let usdc_trustline_key_b64 = BASE64.encode(usdc_trustline_key.to_xdr(Limits::none()).unwrap());

    // 1. Pre-condition: alice's USDC trustline exists at zero balance.
    let pre = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [usdc_trustline_key_b64.clone()] }),
    )
    .await;
    let pre_entries = pre["result"]["entries"].as_array().expect("entries array");
    assert_eq!(
        pre_entries.len(),
        1,
        "v0.7 fork must pre-create a USDC trustline for each test account"
    );
    let pre_balance = parse_trustline_balance(&pre_entries[0]);
    assert_eq!(pre_balance, 0, "trustline starts at zero balance");

    // 2. Build the swap envelope: Phoenix.swap(alice, XLM_SAC,
    //    1000 XLM, None, None, None, None). All Options encode as
    //    ScVal::Void (Soroban's None representation).
    let swap_envelope_b64 = build_invoke_envelope_with_source(
        &phoenix_id,
        "swap",
        vec![
            ScVal::Address(alice_address.clone()),
            ScVal::Address(ScAddress::Contract(soroban_env_host::xdr::ContractId(
                Hash(xlm_sac_id),
            ))),
            i128_to_scval(OFFER_XLM_STROOPS),
            ScVal::Void, // ask_asset_min_amount
            ScVal::Void, // max_spread_bps
            ScVal::Void, // deadline
            ScVal::Void, // max_allowed_fee_bps
        ],
        &account_0.public_key,
    );

    let send_resp = jsonrpc_call(
        &client,
        &url,
        "sendTransaction",
        serde_json::json!({ "transaction": swap_envelope_b64 }),
    )
    .await;
    eprintln!("\nPhoenix swap response:\n{send_resp:#}\n");
    assert_eq!(
        send_resp["result"]["status"], "SUCCESS",
        "real Phoenix swap must succeed against live mainnet state"
    );
    let applied: u64 = send_resp["result"]["appliedChanges"].as_u64().unwrap();
    assert!(
        applied > 0,
        "swap should mutate ledger entries (alice's account, alice's trustline, pool's reserves)"
    );

    // 3. Post-condition: alice's USDC balance is now positive.
    let post = jsonrpc_call(
        &client,
        &url,
        "getLedgerEntries",
        serde_json::json!({ "keys": [usdc_trustline_key_b64] }),
    )
    .await;
    let post_balance = parse_trustline_balance(&post["result"]["entries"][0]);
    assert!(
        post_balance > 0,
        "alice should have received USDC from the swap; got balance {post_balance}"
    );

    // 4. Verify the receipt's return value matches what the
    //    trustline gained. Phoenix.swap returns i128 = USDC out.
    let hash = send_resp["result"]["hash"].as_str().unwrap().to_string();
    let receipt = jsonrpc_call(
        &client,
        &url,
        "getTransaction",
        serde_json::json!({ "hash": hash }),
    )
    .await;
    let return_b64 = receipt["result"]["returnValueXdr"]
        .as_str()
        .expect("swap return value");
    let return_bytes = BASE64.decode(return_b64).unwrap();
    let scval = ScVal::from_xdr(return_bytes, Limits::none()).unwrap();
    let returned_usdc = match scval {
        ScVal::I128(parts) => ((parts.hi as i128) << 64) | (parts.lo as i128),
        other => panic!("expected I128 return from swap, got {other:?}"),
    };
    assert_eq!(
        returned_usdc as i64, post_balance,
        "swap return value must match the trustline delta exactly"
    );

    eprintln!(
        "alice swapped {} stroops XLM → {} stroops USDC on live Phoenix; \
         pre-funded account ↔ real DEX flow works",
        OFFER_XLM_STROOPS, post_balance
    );

    running.shutdown().await.expect("shutdown");
}

/// Decode a `LedgerEntryData::Trustline` from a `getLedgerEntries`
/// response item and return its balance.
fn parse_trustline_balance(item: &Value) -> i64 {
    use soroban_env_host::xdr::LedgerEntryData;
    let xdr_b64 = item["xdr"].as_str().expect("xdr field");
    let bytes = BASE64.decode(xdr_b64).expect("base64 decode");
    match LedgerEntryData::from_xdr(&bytes, Limits::none()).expect("decode LedgerEntryData") {
        LedgerEntryData::Trustline(t) => t.balance,
        other => panic!("expected Trustline entry, got {other:?}"),
    }
}

// `dummy_source_account` is referenced from a docstring example only;
// silence the unused-but-needed-as-sample warning.
#[allow(dead_code)]
fn _keep_dummy_source_in_scope() -> AccountId {
    dummy_source_account()
}
