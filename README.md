# soroban-fork

[![crates.io](https://img.shields.io/crates/v/soroban-fork.svg)](https://crates.io/crates/soroban-fork)
[![docs.rs](https://docs.rs/soroban-fork/badge.svg)](https://docs.rs/soroban-fork)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Lazy-loading mainnet/testnet fork for Soroban tests. Think [Foundry's Anvil](https://book.getfoundry.sh/anvil/), but for Stellar Soroban.

When a test reads a ledger entry that isn't in the local cache, `soroban-fork` fetches it from the Soroban RPC on the fly. No need to pre-snapshot every contract your test might touch.

## Install

```toml
[dev-dependencies]
soroban-fork = "0.4"
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

- **No block production**: there's no `evm_mine` equivalent. The ledger timestamp/sequence is fixed at the fork point.
- **No impersonation**: there's no `vm.prank()`. Use `env.mock_all_auths()` for auth bypassing.
- **No RPC server**: unlike Anvil, this doesn't expose a JSON-RPC endpoint. It's a library for Rust tests.
- **Footprint discovery**: Soroban requires declaring the transaction footprint before execution. The fork tool handles this transparently via the recording-mode footprint in the test environment.

## Requirements

- Rust 1.80+
- `soroban-sdk` 25.x (with `testutils` feature)
- Network access to a Soroban RPC endpoint

## Why this exists

The Stellar SDK supports snapshot-based fork testing via `stellar snapshot create` + `Env::from_snapshot_file()`. But you must know every contract address your test will touch in advance. Miss one dependency and the test fails.

This tool adds the missing piece: **lazy loading on cache miss**. It implements `SnapshotSource` (the trait that feeds ledger entries to the Soroban VM) with an RPC fallback. The standard `soroban_sdk::Env` works unchanged.

See [stellar/rs-soroban-sdk#1440](https://github.com/stellar/rs-soroban-sdk/issues/1440) for the upstream issue tracking this gap.

## License

MIT OR Apache-2.0
