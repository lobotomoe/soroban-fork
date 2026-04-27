# soroban-fork

[![crates.io](https://img.shields.io/crates/v/soroban-fork.svg)](https://crates.io/crates/soroban-fork)
[![docs.rs](https://docs.rs/soroban-fork/badge.svg)](https://docs.rs/soroban-fork)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Lazy-loading mainnet/testnet fork for Soroban tests. Think [Foundry's Anvil](https://book.getfoundry.sh/anvil/), but for Stellar Soroban.

When a test reads a ledger entry that isn't in the local cache, `soroban-fork` fetches it from the Soroban RPC on the fly. No need to pre-snapshot every contract your test might touch.

## Install

```toml
[dev-dependencies]
soroban-fork = "0.5"
```

## Usage

```rust,no_run
use soroban_fork::ForkConfig;
use soroban_sdk::{Address, String, Symbol, vec};

#[test]
fn test_against_real_state() {
    let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
        .cache_file("test_cache.json")   // optional: persist for faster reruns
        .build()
        .expect("fork setup");

    env.mock_all_auths();

    let contract = Address::from_string(&String::from_str(
        &env,
        "CABC...YOUR_CONTRACT_ID",
    ));

    // This lazily fetches the contract's instance, WASM code,
    // and any storage entries from the real network.
    let result: i128 = env.invoke_contract(
        &contract,
        &Symbol::new(&env, "total_assets"),
        vec![&env],
    );

    assert!(result >= 0);

    // env.fetch_count() tells you how many RPC calls were made.
    // Cache is auto-saved on drop (includes lazy-fetched entries).
}
```

## How it works

```
Your test calls contract.total_assets()
         |
         v
  Soroban VM needs a ledger entry
         |
         v
  RpcSnapshotSource.get(key)
         |
    +----+----+
    |         |
  Cache     Cache miss
  hit         |
    |         v
    |    getLedgerEntries RPC call
    |         |
    |         v
    |    Cache result locally
    |         |
    +----+----+
         |
         v
  Return entry to VM
```

- **First run**: entries are fetched from the Soroban RPC as needed. Each unique entry = one HTTP call (batched in chunks of 200 if pre-fetching).
- **Subsequent runs**: if `cache_file` is set, entries are loaded from disk. Only new entries trigger RPC calls.
- **State changes are local**: the real network is never modified. Deposits, transfers, and other mutations happen in memory only.

## API

### `ForkConfig`

```rust
ForkConfig::new(rpc_url)           // Soroban RPC endpoint
    .cache_file("cache.json")             // optional: disk persistence + auto-save on drop
    .network_id(bytes)                    // optional: override the SHA-256 network id
    .fetch_mode(FetchMode::Strict)        // optional: Strict (default) or Lenient
    .at_ledger(1_234_567)                 // optional: pin the Env's reported sequence
    .pinned_timestamp(1_700_000_000)      // optional: pin the Env's close time
    .max_protocol_version(25)             // optional: cap the protocol the VM reports
    .tracing(true)                        // optional: capture cross-contract call tree
    .rpc_config(RpcConfig { retries: 5, ..RpcConfig::default() })
    .build()?                             // returns Result<ForkedEnv, ForkError>
```

Network metadata (passphrase + SHA-256 id) is fetched from the RPC's
`getNetwork` method at build time — no URL heuristics, no silent defaults.
Override with `.network_id(bytes)` only if you actually need to.

The `Env`'s reported timestamp defaults to the close time of the latest
ledger, fetched via `getLedgers` at build time. Tests are reproducible
across runs out of the box — pin an explicit value via
`.pinned_timestamp(...)` only when you need to anchor to a specific
moment (e.g. reproducing a known historical scenario).

### `ForkedEnv`

Returned by `ForkConfig::build()`. Implements `Deref<Target = Env>` so all SDK
methods work transparently. Adds fork-specific capabilities:

```rust,ignore
let env = ForkConfig::new(rpc_url).cache_file("cache.json").build()?;

// Use like a regular Env (via Deref)
env.mock_all_auths();
let result: i128 = env.invoke_contract(&addr, &symbol, vec![&env]);

// Fork-specific methods
env.fetch_count();                 // number of RPC calls made
env.save_cache()?;                 // explicit save (also called automatically on drop)
env.warp_time(86_400);             // advance ledger timestamp + sequence
env.deal_token(&usdc, &who, amt);  // Foundry-style balance deal
env.env();                         // &Env (for edge cases where Deref doesn't suffice)
```

### `FetchMode`

Controls behavior when the RPC fails from inside the VM loop (where the
`SnapshotSource` trait can't return a typed error):

- **`Strict`** (default): panic. Best for tests — a fetch failure means the test setup is wrong, and you want the stack trace.
- **`Lenient`**: log at `warn!` level and return `None`. Useful when partial state is acceptable.

### `RpcConfig`

Transport tunables. Defaults: 3 retries with 300 ms exponential backoff
plus full jitter (so concurrent test runners don't synchronise their
retries into a thundering herd), 30 s per-request timeout, 200-key batch
size (Soroban RPC cap). Customize via `.rpc_config(RpcConfig { .. })` on
the builder. HTTP 408, 425, 429, and 5xx responses are retried; other
4xx codes fail fast and include the response body for diagnostics.

### Tracing — Foundry-style call trees

Set `.tracing(true)` on the builder to capture cross-contract call trees.
The host runs in `DiagnosticLevel::Debug`, every `fn_call`/`fn_return`
emits a diagnostic event, and `env.trace()` reconstructs the tree:

```rust
let env = ForkConfig::new(rpc_url)
    .tracing(true)
    .build()?;

env.invoke_contract::<i128>(&vault, &Symbol::new(&env, "deposit"), args);
env.print_trace();
```

```text
[TRACE]
  [CABC…XYZ1] deposit(GACC…QRST, 1000000)
    [CCDE…UVW2] transfer_from(GACC…QRST, CABC…XYZ1, 1000000)
      ← ()
    [CFGH…IJK3] invest(1000000)
      ← 1010000
    ← 1010000
```

Programmatic access via `env.trace()` returns a `Trace` with structured
`TraceFrame`s — useful for asserting call structure or balances inside
a test. Failed calls render as `[rolled back]`; WASM traps show as
`TRAPPED (no fn_return)`.

**Per-invocation scoping.** The host's `InvocationMeter` clears the
events buffer at the start of every top-level `invoke_contract`, so each
`trace()` reflects only the most recent top-level call. Capture before
the next call if you need history. See the
[`trace` module docs](https://docs.rs/soroban-fork/latest/soroban_fork/trace/index.html)
for wire-format details and caveats (single-`Vec`-arg ambiguity).

## JSON-RPC server mode

Library mode (everything above) is for Rust tests. **Server mode** turns
soroban-fork into a Stellar Soroban RPC drop-in that any tooling — JS,
Python, Go SDKs, Stellar Lab, Freighter, custom clients — can point at:

```sh
cargo install soroban-fork --features server
soroban-fork serve --rpc https://soroban-rpc.mainnet.stellar.gateway.fm
# → serving JSON-RPC on http://127.0.0.1:8000
```

Then any client speaking the Stellar RPC dialect:

```js
import { SorobanRpc } from "@stellar/stellar-sdk";
const server = new SorobanRpc.Server("http://localhost:8000");
const account = await server.getAccount("GA5...");
const result = await server.simulateTransaction(tx);  // hits the fork
```

Or via the Rust `stellar-rpc-client`, raw curl, or anything that
understands the spec.

### Pre-funded test accounts *(new in v0.7)*

The fork mints **10 deterministic test accounts** at build time, each
with 100K XLM and a USDC trustline ready to receive — Anvil's "10
accounts × 10K ETH" UX for Stellar. Same seed produces the same
accounts every run, so test code can hard-code addresses by index.
The CLI prints them on startup:

```
soroban-fork v0.7
Listening on http://127.0.0.1:8000

Available test accounts:
(0) GBXXX...AB12  (100000.0000000 XLM)  ->  SAXXX...CD34
(1) GCYYY...EF56  (100000.0000000 XLM)  ->  SAXXX...GH78
...
```

Pass them to JS-SDK's `Keypair.fromSecret(...)` to sign envelopes.
After every successful `sendTransaction`, the source account's
sequence number auto-increments — so chained `getAccount` →
`TransactionBuilder` → `sendTransaction` loops just work.

**Real DEX flow works end-to-end.** A test account can swap XLM →
USDC against the live Phoenix DEX (or Soroswap, Aquarius, …) and
the USDC actually lands in its trustline. Smoke-tested:
1000 XLM → 167.4020548 USDC at the live mainnet pool reserves.

**No hidden hardcode.** The trustline default targets the mainnet
USDC issuer (Circle); for testnet, futurenet, or a custom fork,
override via `ForkConfig::test_account_trustlines(vec![...])`. The
trustlines are written with `flags = AUTHORIZED_FLAG`, `limit = i64::MAX` —
shape-equivalent to running `ChangeTrust` then having the issuer
authorize, just bootstrapped at build time. Auth runs in trust mode
(`Recording(false)`), Anvil-equivalent for tests.

Override count via `--accounts N` (set to `0` to disable). For
library users, `ForkConfig::test_account_count(n).build()` exposes
the same machinery; read accounts back with `env.test_accounts()`.

### Deploy your own contracts onto the fork *(new in v0.7)*

The same `sendTransaction` accepts `HostFunction::UploadContractWasm`
and `HostFunction::CreateContract`, so you can deploy custom contracts
straight onto the forked mainnet state and have them call live
production contracts. The test suite's
`server_deploy_and_invoke_custom_contract` covers the full loop:

1. Upload a tiny `add(i32, i32) -> i32` WASM
2. Create the contract instance from the uploaded hash
3. Invoke `add(2, 3)` on the deployed contract — returns 5

Cross-protocol scenarios (your contract calls Blend, Phoenix,
Soroswap, etc.) follow the same pattern: dependencies the deployed
contract reaches into get lazy-fetched from mainnet and cached
locally.

### Methods supported in v0.8

- **`getHealth`** — fork status + latest ledger
- **`getVersionInfo`** — server version + protocol version
- **`getNetwork`** — passphrase + protocol version + network ID (proxied
  from the upstream RPC at fork-build time, then served locally)
- **`getLatestLedger`** — fork's reported ledger sequence + protocol
- **`getLedgers`** — single-element page describing the fork point with
  real `ledgerCloseTime` (Unix-seconds string, per Stellar convention)
- **`getLedgerEntries`** — base64-XDR `LedgerKey` array → array of
  entries; routed through the fork's lazy-fetch cache, so first hit
  proxies upstream and subsequent hits are local
- **`simulateTransaction`** — accepts a base64-XDR `TransactionEnvelope`
  with one `InvokeHostFunctionOp`, runs it via the host's recording-mode
  primitive, returns:
  - `results[0].xdr` — the function's return value (`ScVal`)
  - `results[0].auth` — auth entries `sendTransaction` would need
  - `transactionData` — `SorobanTransactionData` with recorded footprint
    and `resourceFee` matching `minResourceFee`
  - `events` — diagnostic events emitted during simulation
  - `cost.cpuInsns` / `cost.memBytes` — real numbers from the host's
    `Budget`, *not* a `write_bytes` proxy
  - `minResourceFee` — derived from the live on-chain Soroban fee
    schedule via `compute_transaction_resource_fee` (since v0.5.2)
  - `latestLedger` — fork's reported ledger
- **`sendTransaction`** *(new in v0.6)* — applies the host invocation's
  writes back to the snapshot source so subsequent reads see them.
  Auth runs in trust mode (`Recording(false)`) — same UX as Anvil's
  default for EVM tests, so unsigned envelopes from test code work
  without ceremony. Returns `status` (`"SUCCESS"` / `"ERROR"`),
  `hash` (sha256 of the envelope), `appliedChanges` (number of
  `LedgerEntryChange`s written), and the original envelope echo.
- **`getTransaction`** *(new in v0.6)* — receipt lookup by hash.
  Returns `"SUCCESS"` / `"FAILED"` / `"NOT_FOUND"`, plus the original
  envelope, the host function's `ScVal` return value, and the
  applied-changes count when found.
- **`anvil_setLedgerEntry`** *(new in v0.8)* — Anvil-style cheatcode:
  force-write a base64-XDR `LedgerEntry` to any `LedgerKey` directly
  in the snapshot source, bypassing host-level checks. Load-bearing
  primitive for stress-test scenarios — oracle price manipulation,
  force-set token balances, replace contract code, all reduce to
  this one entry write.
- **`anvil_mine`** *(new in v0.8)* — advance the fork's reported
  ledger sequence by `blocks` (default 1) and bump close-time by
  `timestampAdvanceSeconds` (default `blocks * 5` — Stellar's
  average close rate). Pushes time-sensitive contract logic
  (vesting cliffs, oracle staleness) past thresholds without
  orchestrating real transactions.

### What v0.8 server does NOT support

Listed up front so nothing surprises you:

- **`getEvents`** — historical event filtering. Diagnostic events
  emitted during simulation are reachable via `simulateTransaction`'s
  response.
- **Ergonomic `anvil_*` wrappers** — `setBalance`, `setStorage`,
  `setCode`, `setNonce`, `impersonate`. The primitive
  `anvil_setLedgerEntry` covers all of these once the client
  constructs the right XDR; sugar wrappers are a v0.8.x followup.
- **`anvil_snapshot` / `anvil_revert`** — saved-state checkpoints.
  Scoped to v0.9 (the `Rc<HostImpl>` snapshot model needs its own
  design pass — either a journaling layer over `RpcSnapshotSource`
  or a clone-on-snapshot of the entire cache map).
- **Block production by `sendTransaction` side-effect** — each send
  applies its writes and bumps the source's `seq_num`, but does
  *not* automatically advance `env.ledger().sequence_number()`. Use
  `anvil_mine` (or `env.warp(...)` from lib mode) to push the
  ledger forward. Auto-mine on send is a v0.8.x ergonomic followup.
- **`resultMetaXdr` on `getTransaction`** — Stellar's
  `TransactionMeta::V3` carries state-change deltas in a
  Stellar-core-XDR-heavy shape; v0.6 returns `returnValueXdr` and
  `appliedChanges` instead. Full meta XDR is a v0.6.x followup.

### Architecture: single-threaded actor

axum HTTP handlers run on a multi-thread tokio runtime; commands flow
through a bounded `mpsc` channel to one OS thread that owns the
`ForkedEnv`. The SDK's `Env` contains `Rc<HostImpl>` and is `!Send`, so
it can't live behind `Arc<RwLock>` — single-thread ownership with
explicit messaging is the load-bearing constraint of this design. Same
trade-off Foundry's Anvil makes for the EVM.

```
[HTTP handler 1]──┐
[HTTP handler 2]──┼──mpsc::channel──→ [worker thread] owns ForkedEnv
[HTTP handler N]──┘                           │
                                              └─→ snapshot_source.get()
                                                  └─→ on cache miss → upstream RPC
```

Cache misses on `getLedgerEntries` block the worker for one upstream
round-trip. Steady state (after first contact) is local.

### Library API for server mode

If you'd rather embed the server in your own Rust process (CI test
harness, custom Stellar tooling), use the library API:

```rust,ignore
use soroban_fork::{ForkConfig, server::Server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ForkConfig::new("https://soroban-rpc.mainnet.stellar.gateway.fm");

    Server::builder(config)
        .listen("127.0.0.1:8000".parse().unwrap())
        .serve()        // runs until SIGINT/SIGTERM
        .await?;

    Ok(())
}
```

For tests that need to bind ephemeral ports and shut down programmatically:

```rust,ignore
let running = Server::builder(config)
    .listen("127.0.0.1:0".parse().unwrap())   // OS-assigned port
    .start()
    .await?;
let url = format!("http://{}", running.local_addr());
// ... drive the server with a real client ...
running.shutdown().await?;
```

### `RpcSnapshotSource`

The core primitive. Implements `soroban_env_host::storage::SnapshotSource`:

```rust,ignore
use std::sync::Arc;
use soroban_fork::{RpcSnapshotSource, RpcConfig};
use soroban_fork::RpcClient; // re-exported

let client = Arc::new(RpcClient::new("https://soroban-testnet.stellar.org:443", RpcConfig::default())?);
let source = Arc::new(RpcSnapshotSource::new(client));
source.preload(entries);          // pre-load entries from a snapshot file
let all_entries = source.entries();  // export for persistence
```

`RpcSnapshotSource` is `Send + Sync`, so it can be wrapped in `Arc` and
shared across threads — useful for parallel test runners and the
upcoming RPC-server mode. Internally the cache stores XDR-encoded bytes
and parses to `LedgerEntry` only at the SDK boundary, so no `Rc` ever
crosses threads.

### Errors

Every public fallible API returns `Result<T, ForkError>`. The error enum
discriminates transport failures, RPC-level errors, XDR codec failures,
cache I/O, and protocol-violation cases — no string-typed errors.

### Logging

Uses the [`log`](https://docs.rs/log) facade — no output unless a logger
is initialized in the test binary. Typical setup:

```bash
RUST_LOG=soroban_fork=info cargo test -- --ignored
```

## Examples

Runnable demos against live Stellar mainnet. Each one targets a real
contract — Blend lending, Phoenix DEX — to show where lazy-fork pays
off compared to fabricated reserves in a snapshot test.

```sh
# What does my 50K USDC deposit do to the Blend Fixed pool?
cargo run --release --example blend_lending

# What's my fill price market-selling 1M XLM into Phoenix?
cargo run --release --example phoenix_slippage

# Phoenix vs Soroswap on the same XLM/USDC trade — how big is the
# cross-DEX price gap right now?
cargo run --release --example cross_dex_arbitrage
```

`MAINNET_RPC_URL` overrides the upstream RPC. Each example prints
the forked ledger sequence and the number of RPC fetches it triggered.

For server-mode tooling (`@stellar/stellar-sdk`, Stellar Lab,
Freighter), `examples/server_demo.mjs` shows the JSON-RPC dialect
working from Node — no npm install, no XDR dance, just `fetch()`:

```sh
# shell A — start the fork server
cargo run --release --features server --bin soroban-fork -- \
    serve --rpc https://soroban-rpc.mainnet.stellar.gateway.fm

# shell B — drive it from Node
node examples/server_demo.mjs
```

## Combining with `stellar snapshot create`

For maximum speed, pre-snapshot known contracts and let `soroban-fork` handle the rest lazily:

```bash
# Snapshot the main contracts you know about
stellar snapshot create \
  --address $VAULT_CONTRACT \
  --network testnet --output json --out vault_state.json

stellar snapshot create \
  --address $STRATEGY_CONTRACT \
  --network testnet --output json --out strategy_state.json

stellar snapshot merge \
  --input vault_state.json --input strategy_state.json \
  --output merged.json
```

```rust
let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
    .cache_file("merged.json")  // pre-loaded entries skip RPC
    .build();

// Calls to vault/strategy use cached entries (fast).
// Calls to USDC token or other dependencies are fetched lazily from RPC.
```

## Diagnostics

Every lazy fetch is logged to stderr with human-readable key types:

```
[soroban-fork] forked at ledger 2070078 (protocol 25)
[soroban-fork] fetch #1: ContractData(instance)
[soroban-fork] fetch #2: ContractCode(dee2d494...)
[soroban-fork] fetch #3: ContractData(persistent)
[soroban-fork] saved 3 entries to test_cache.json
```

`env.fetch_count()` returns the total number of RPC calls for programmatic assertions.

## Cache format

The cache file uses the same JSON format as `stellar snapshot create` (`LedgerSnapshot`). You can:
- Use a `stellar snapshot create` output as the cache input
- Share cache files between team members for reproducible tests
- Inspect cached entries with `stellar xdr decode`

Cache is saved automatically when `ForkedEnv` is dropped, including all entries
that were lazy-fetched during the test. This means the second run of a test with
`cache_file` set will be fully local -- zero RPC calls.

## Limitations

What soroban-fork does NOT yet do — listed up front so nothing surprises you in production:

- ~~**No `sendTransaction` / state mutation through RPC.**~~ *(closed in v0.6.)* Server-mode `sendTransaction` applies writes back to the snapshot source so subsequent reads see them; `getTransaction` retrieves receipts by hash. *(closed in v0.7:)* the fork now mints 10 pre-funded test accounts at build, auto-increments `seq_num` after every successful send, and accepts `UploadContractWasm` + `CreateContract` host functions — full deploy-then-call workflow against forked mainnet works. `anvil_*` cheatcodes (impersonate / setBalance / setCode / setStorage) are scoped to v0.8; `anvil_snapshot` / `anvil_revert` to v0.9.
- **No TTL / archival simulation.** Soroban entries carry a `live_until_ledger_seq`; on real mainnet they become archived past that ledger and need a `RestoreFootprint` operation. We track `live_until` in the cache but do not yet model expiry — bumping `env.ledger()` past an entry's `live_until` will not flip it to archived. Tests that depend on TTL-expiry semantics will see false-positives.
- **No historical state.** `at_ledger(N)` shifts only what `env.ledger().sequence_number()` reports; the actual ledger entries are always fetched at the RPC's *current* latest. Pin to a specific ledger only when paired with `cache_file` for reproducibility, not when expecting historical state.
- **Tracing renders structure, not metering.** `env.trace()` captures the call tree with decoded args and return values. It does **not** yet render per-frame gas / cost units, contract events, or decoded `HostError` reasons. (Diagnostic events from the host carry call structure but not metering numbers; metering is planned. Server-mode `simulateTransaction` does return real `cost.cpuInsns` separately.)
- ~~**Server `simulateTransaction` fee fields are stubbed.**~~ *(closed in v0.5.2.)* `minResourceFee` is now derived from the live on-chain fee schedule via `compute_transaction_resource_fee`, and `cost.memBytes` reads `Budget::get_mem_bytes_consumed` directly. Bandwidth + historical-data fees use the actual envelope size received over the wire.
- **Footprint discovery.** Soroban requires declaring the transaction footprint before execution. The fork tool handles this transparently via the recording-mode footprint in the test environment.

## Requirements

- Rust 1.91+ (the Soroban SDK 25.3.1 floor)
- `soroban-sdk` 25.x (with `testutils` feature)
- Network access to a Soroban RPC endpoint

## Why this exists

The Stellar SDK supports snapshot-based fork testing via `stellar snapshot create` + `Env::from_snapshot_file()`. But you must know every contract address your test will touch in advance. Miss one dependency and the test fails.

This tool adds the missing piece: **lazy loading on cache miss**. It implements `SnapshotSource` (the trait that feeds ledger entries to the Soroban VM) with an RPC fallback. The standard `soroban_sdk::Env` works unchanged.

See [stellar/rs-soroban-sdk#1440](https://github.com/stellar/rs-soroban-sdk/issues/1440) for the upstream issue tracking this gap.

## License

MIT OR Apache-2.0
