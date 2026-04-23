# soroban-fork

Lazy-loading mainnet/testnet fork for Soroban tests. Think [Foundry's Anvil](https://book.getfoundry.sh/anvil/), but for Stellar Soroban.

When a test reads a ledger entry that isn't in the local cache, `soroban-fork` fetches it from the Soroban RPC on the fly. No need to pre-snapshot every contract your test might touch.

## Usage

```rust
use soroban_fork::ForkConfig;
use soroban_sdk::{Address, String, Symbol, vec};

#[test]
fn test_against_real_state() {
    let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
        .cache_file("test_cache.json")   // optional: persist for faster reruns
        .build();

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
    .cache_file("cache.json")      // optional: disk persistence + auto-save on drop
    .network_id(bytes)             // optional: override network ID
    .fetch_mode(FetchMode::Strict) // optional: Strict (default) or Lenient
    .build()                       // returns ForkedEnv (derefs to Env)
```

Network ID is auto-detected from the URL (`testnet` or `mainnet` in the hostname).

### `ForkedEnv`

Returned by `ForkConfig::build()`. Implements `Deref<Target = Env>` so all SDK
methods work transparently. Adds fork-specific capabilities:

```rust
let env = ForkConfig::new(rpc_url).cache_file("cache.json").build();

// Use like a regular Env (via Deref)
env.mock_all_auths();
let result: i128 = env.invoke_contract(&addr, &symbol, vec![&env]);

// Fork-specific methods
env.fetch_count();    // number of RPC calls made
env.save_cache()?;    // explicit save (also called automatically on drop)
env.env();            // &Env reference (for edge cases where Deref doesn't suffice)
```

### `FetchMode`

Controls error handling on RPC failures:

- **`Strict`** (default): panics on RPC errors. Best for tests -- a fetch failure means the test setup is wrong.
- **`Lenient`**: logs errors and returns `None`. Useful when partial state is acceptable.

### `RpcSnapshotSource`

The core primitive. Implements `soroban_env_host::storage::SnapshotSource`:

```rust
use soroban_fork::RpcSnapshotSource;

let source = RpcSnapshotSource::new("https://soroban-testnet.stellar.org:443".into());
// Pre-load entries from a snapshot file
source.preload(entries);
// After testing, export all cached entries
let all_entries = source.entries();
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
