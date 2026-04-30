# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(with pre-1.0 latitude — minor versions can break in well-justified cases).

## [Unreleased]

## [0.8.8] — 2026-05-01

### Added
- **Common pitfalls** section in `README.md`, covering five paper-cuts
  reported by the first real integrator wiring soroban-fork into a
  Blend-style mainnet test:
  - When to use `mock_all_auths()` versus
    `mock_all_auths_allowing_non_root_auth()`, and why the relaxed
    variant silently masks missing `authorize_as_current_contract`
    declarations until you hit testnet.
  - `include_bytes!("…wasm")` does not trigger a rebuild of the wasm
    when the contract's `.rs` files change — workarounds + the v0.9.x
    `register_wasm_from_workspace` plan.
  - `cache_file` math for cheap CI (≈ 175 live RPC calls per build
    without it; zero with it).
  - Pointer to `tracing(true)` + `print_trace()` as the existing tool
    for debugging `Error(Auth, InvalidAction)` panics, with the
    `print_auth_tree()` / `last_auth_failure()` plan for v0.9.0.
  - `into_val(&env)` ergonomics on `ForkedEnv` and the planned
    `IntoVal` impl path.

### Notes
- Pure documentation release — no code changes, no API additions, no
  binary churn. Two of the five "missing features" the integrator
  reported (`tracing(true)` + `print_trace()`, `cache_file`) already
  shipped in v0.7/v0.8 and the gap was discoverability.
- v0.9.0 will be a diagnostics & DX release covering the genuinely
  missing items: `print_auth_tree`, `last_auth_failure`,
  `ForkConfig::strict_auth(true)`, and the wasm-rebuild trap.

## [0.8.7] — 2026-04-27

### Added
- `fork_setBalance` Soroban-token path. Extends v0.8.4's
  `deal()`-equivalent with a third asset variant
  `{ contract: "C..." }` that handles tokens whose balance lives in
  contract state. Handler simulates `balance(account)` to read the
  current value, then invokes `mint(to, delta)` or
  `burn(to, |delta|)` on the SEP-41 surface — trust-mode auth
  bypasses the SAC's admin / token's authorisation checks, so
  no signatures needed.
- `amount` for the contract path is parsed as `i128` (Soroban-side
  precision); Native and Credit paths still parse `i64` (Classic
  stroops fit). Same wire field, different range per branch.
- Smoke test exercises both mint (delta > 0) and burn (delta < 0)
  branches against the live mainnet USDC SAC.

### Notes
- The `deal()`-equivalent surface is now complete: Native XLM,
  Credit AlphaNum4/12 (mainnet USDC, EURC, …), and Soroban-native
  tokens (the SAC for any Classic asset, plus custom Soroban tokens
  that follow SEP-41) all routable through one wire method.

## [0.8.6] — 2026-04-27

### Added
- `fork_etch` — Foundry's `vm.etch`-equivalent in one wire call. Hot-swap
  the WASM under any contract address; **storage is preserved verbatim** so
  contract state survives the code swap (the hotfix scenario). Auto-creates
  the instance entry if the target address has none.
- Smoke test `server_fork_etch_installs_callable_contract_in_one_call` —
  etches `add_i32.wasm` at synthetic address `[0xEE; 32]`, simulates
  `add(7, 8)` → `I32(15)` against live mainnet.

## [0.8.5] — 2026-04-27

### Added
- Headline showcase test — `server_cheatcode_only_deploy_coexists_with_mainnet`.
  Proves a contract can be installed using only `fork_setCode` +
  `fork_setStorage` (no `UploadContractWasm` or `CreateContract` envelopes,
  no source-account ceremony, no salt), then invoked alongside live mainnet
  contracts in the same simulation context.
- README "headline showcase" section pointing at the new test as the
  canonical demo.

### Fixed
- Cheatcode-installed `ContractCode` and `ContractData` entries now require
  an explicit `liveUntilLedgerSeq` — without one the host's storage check
  treats the entry as archived and refuses to read with
  `Error(Storage, InternalError)`. Tests pass `999_999_999` (effectively
  forever) on all cheatcode-write calls.

## [0.8.4] — 2026-04-27

### Added
- `fork_setBalance` — Foundry's `deal()`-equivalent for Stellar Classic
  assets. Sets an account's balance for native XLM (`AccountEntry`) or a
  credit asset (`TrustLineEntry`); **auto-creates** the underlying entry
  if it doesn't exist yet. Auto-created native accounts get master
  threshold 1; auto-created trustlines get `flags = AUTHORIZED`,
  `limit = i64::MAX` — the post-`ChangeTrust` shape.
- Wire takes `account` (G-strkey), `amount` (i64 stroops as a decimal
  string — Stellar precision-safe convention), and optional `asset`
  (`"native"` default, or `{ code, issuer }` for credit assets).
- Smoke test covers all three branches: native RMW, credit RMW on existing
  trustline, credit auto-create for never-existing asset.

### Notes
- Soroban-native token mint/burn (the SAC `mint(to, amount)` invocation
  path) is intentionally not in scope for v0.8.4. For Classic-routed
  Soroban tokens (mainnet USDC SAC reads the AlphaNum4 USDC trustline)
  the credit-asset path covers the use case directly — write the
  trustline, the SAC reads from the same entry.

## [0.8.3] — 2026-04-27

### Added
- `fork_setCode` — sugar over `fork_setLedgerEntry` for installing WASM
  bytes as a `ContractCode` entry without an `UploadContractWasm`
  envelope. Wire: `{ wasm: base64, liveUntilLedgerSeq?: u32 }` →
  `{ ok, hash, latestLedger }`. The hash is **server-derived** (sha256 of
  bytes — same way the host computes it) so a buggy or malicious client
  can't install bytes under a non-matching hash.

### Notes
- Local `cargo fmt` and CI's stable rustfmt continue to disagree on import
  grouping. Workflow: run `cargo fmt --all` (no `--check`) before pushing.
  `--check` exits 0 locally even when CI rustfmt would rewrite the file.

## [0.8.2] — 2026-04-27

### Added
- `fork_setStorage` — first ergonomic `fork_*` wrapper. Sugar over
  `fork_setLedgerEntry` for the common case of writing a ScVal into a
  contract's storage. Wire takes `contract` (strkey), `key` (base64 ScVal),
  `value` (base64 ScVal), and optional `durability`
  (`"persistent"` default / `"temporary"`) and `liveUntilLedgerSeq`. The
  handler builds the multi-level `ContractData` XDR server-side so clients
  don't have to assemble it themselves.

### Fixed
- Folded in the v0.8.1 rustfmt CI fix (one `assert_eq!` that local rustfmt
  left as multi-line but CI's stable rustfmt collapses to one line).

## [0.8.1] — 2026-04-27

### Changed (breaking, pre-1.0 latitude)
- `anvil_setLedgerEntry` → `fork_setLedgerEntry`
- `anvil_mine` → `fork_closeLedgers` (Stellar's verb for finalising a
  ledger is *close*, not *mine*)
- `MineParams.blocks` → `CloseLedgersParams.ledgers`

The `fork_*` prefix marks the namespace boundary explicitly: these are
non-standard extensions, not bare overrides of Stellar RPC methods.

### Migration

```diff
- await client.request("anvil_setLedgerEntry", { key, entry });
+ await client.request("fork_setLedgerEntry", { key, entry });

- await client.request("anvil_mine", { blocks: 5 });
+ await client.request("fork_closeLedgers", { ledgers: 5 });
```

### Notes
- README pruned of "Anvil-equivalent" recurrences. Kept the elevator-pitch
  reference ("Think Anvil, but for Stellar") and rename breadcrumbs in
  the method docs so v0.8.0 users know what changed.
- Doc comments swapped "Anvil-style cheatcode" → "fork-mode primitive" /
  "fork-mode extension". The library is now Stellar-idiomatic; Anvil is
  inspiration, not template.

## [0.8.0] — 2026-04-27

### Added
- Fork-mode cheatcodes over JSON-RPC (renamed in v0.8.1 — see above):
  - `anvil_setLedgerEntry` (now `fork_setLedgerEntry`) — force-write any
    `LedgerEntry` to any `LedgerKey` directly in the snapshot source.
    Load-bearing primitive that every state-mutation cheatcode reduces to
    (Stellar's storage model is one entry per key).
  - `anvil_mine` (now `fork_closeLedgers`) — advance reported ledger
    sequence + close-time without orchestrating real transactions.
- `RpcSnapshotSource::set_entry` lib API.

### Changed
- `ForkedEnv::ledger_sequence()` and `::ledger_close_time()` now read live
  from `env.ledger().get()` so warps are visible from the wire. Fork-point
  fields preserved on the struct for `save_cache` provenance.
- `ForkedEnv::warp` uses `saturating_add` since it's now wire-reachable —
  prevents wire-driven misuse from panicking the worker thread.

## [0.7.0] — 2026-04-27

### Added
- **Pre-funded test accounts.** 10 deterministic ed25519 keypairs minted
  at fork-build time, derived from `sha256("soroban-fork test account {i}")`.
  Each carries 100K XLM and a USDC trustline with
  `flags = AUTHORIZED`, `limit = i64::MAX`. Same seed → same accounts
  across runs and machines, so test code can reference accounts by index.
- New `src/test_accounts.rs` public module exposing the keypairs.
- `RpcSnapshotSource::bump_account_seq` — auto-increments the source
  account's `seq_num` after every successful `sendTransaction`, so chained
  `getAccount` → `TransactionBuilder` → `sendTransaction` loops just work.
- Custom contract deploy verified end-to-end. The fork accepts
  `HostFunction::UploadContractWasm` and `HostFunction::CreateContract`;
  smoke test deploys a 584-byte `add_i32.wasm` and calls `add(2, 3)` → `5`.
- **Headline test:** pre-funded account swaps 1000 XLM for 167.4020548
  USDC against the live Phoenix mainnet pool through one `sendTransaction`,
  USDC lands in the trustline balance.
- `ForkConfig::test_account_count(n)` builder method.
- `ForkConfig::test_account_trustlines(...)` builder method (for testnet
  / custom-fork issuer overrides).
- CLI: `--accounts N` flag (default 10), prints account addresses on
  startup.
- `ed25519-dalek` 2.x added as a dependency.

### Notes
- USDC default targets Circle's mainnet issuer
  (`GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN`). For testnet,
  futurenet, or a custom fork override via
  `ForkConfig::test_account_trustlines(...)`.

## [0.6.0] — 2026-04-27

### Added
- `sendTransaction` — applies a transaction's writes to the snapshot
  source so subsequent reads see them. Trust-mode auth
  (`Recording(false)`) — unsigned envelopes from test code apply without
  ceremony.
- `getTransaction` — receipt lookup by hash.
- `RpcSnapshotSource::apply_changes` — feeds host's `ledger_changes` back
  into the cache.
- Canonical Stellar tx hash via `TransactionSignaturePayload` (matches
  what `stellar-rpc` and JS-SDK clients compute), not raw envelope sha256.

### Notes
- Avoided lying with a fake `errorResultXdr` field — emit `errorMessage`
  (plain text) instead since real `TransactionResult` XDR is multi-step.
  Real XDR is a follow-up.

## [0.5.3] — 2026-04-27

### Added
- `examples/cross_dex_arbitrage.rs` — Phoenix `simulate_swap` vs Soroswap
  `router_get_amounts_out` on XLM/USDC against live mainnet. At ledger
  62308945, 100K XLM sell → +26.2% gap (Phoenix pays more — Soroswap's
  pool is shallower).

## [0.5.2] — 2026-04-27

### Changed
- `simulateTransaction` now returns honest fee + memory numbers:
  - `minResourceFee` computed via `compute_transaction_resource_fee`
    against the 6 on-chain `ConfigSetting` entries (lazy-fetched,
    `OnceCell`-cached on `ForkedEnv`).
  - `cost.memBytes` reads `Budget::get_mem_bytes_consumed` directly.
    Was a `write_bytes` proxy in v0.5.1 — that was a lie.
  - `transactionData.resourceFee` matches `minResourceFee`.
- New `src/fees.rs` module.

### Notes
- Slight underestimate (~70 bytes per signer) — same approximation
  `stellar-rpc` makes; documented.

## [0.5.1] — 2026-04-27

### Added
- `examples/blend_lending.rs` — Blend V1 Fixed pool deposit scenario
  against live mainnet.
- `examples/phoenix_slippage.rs` — slippage table for swaps against the
  Phoenix DEX.
- `examples/server_demo.mjs` — Node 18+ zero-dependency JS demo of the
  JSON-RPC server.
- README "Examples" section.

## [0.5.0] — 2026-04-27

### Added
- **JSON-RPC server mode** behind the `server` cargo feature:
  `getHealth`, `getVersionInfo`, `getNetwork`, `getLatestLedger`,
  `getLedgers`, `getLedgerEntries`, `simulateTransaction`.
- CLI binary `soroban-fork serve`.
- Single-threaded actor pattern — the SDK's `Env` is `!Send`, so it
  lives on one OS thread and HTTP handlers send commands via mpsc.

## [0.4.1] — 2026-04-27

### Changed
- `RpcSnapshotSource::entries()` decodes outside the cache lock —
  concurrent `get` and `fetch` calls aren't blocked for the parse loop.
- Compile-time `Send + Sync` assert for `RpcSnapshotSource` (catches
  any future `Rc`/`RefCell` reintroduction at `cargo build` time, not
  just `cargo test`).
- Doc accuracy fixes.

## [0.4.0] — 2026-04-27

### Changed
- `RpcSnapshotSource` is now `Send + Sync`. Internal cache stores
  XDR-encoded bytes in a `Mutex<BTreeMap>`; `Rc<LedgerEntry>` is
  reconstructed per `get` call on the caller's thread (the SDK's
  `SnapshotSource::get` boundary expects `Rc`). Foundation for the
  v0.5 server.

## [0.3.0] — 2026-04-27

### Added
- Call-tree tracing via the host's diagnostic event stream
  (Foundry-`-vvvv`-style). `ForkConfig::tracing(true)` flips the host
  into `DiagnosticLevel::Debug`; `ForkedEnv::trace()` reconstructs the
  cross-contract call tree.

## [0.2.0] — 2026-04-27

### Changed
- Ledger close-time defaults to the upstream RPC's reported close time
  via `getLedgers` — was wall-clock `SystemTime::now()`, which made test
  timestamps depend on when the test ran.
- Cache writes are atomic (write-tmp-then-rename).
- Backoff has full jitter (concurrent test runners no longer synchronise
  retries into a thundering herd).
- HTTP 408 / 425 / 429 / 5xx are retried with the same backoff schedule;
  other 4xx fail fast and include the response body.

### Removed
- Silent fallbacks (returning empty/default values on errors). Every
  fallible API now returns `Result<T, ForkError>`.

## [0.1.0] — 2026-04-23

### Added
- Lazy-loading mainnet/testnet fork for Soroban tests. `RpcSnapshotSource`
  implements the SDK's `SnapshotSource` trait, fetching entries on demand
  from a Soroban RPC endpoint. Compatible with the `LedgerSnapshot` JSON
  format (`stellar snapshot create` interop).

[Unreleased]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.8...HEAD
[0.8.8]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.7...v0.8.8
[0.8.7]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.6...v0.8.7
[0.8.6]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.5...v0.8.6
[0.8.5]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.4...v0.8.5
[0.8.4]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.3...v0.8.4
[0.8.3]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/lobotomoe/soroban-fork/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/lobotomoe/soroban-fork/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/lobotomoe/soroban-fork/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/lobotomoe/soroban-fork/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lobotomoe/soroban-fork/releases/tag/v0.1.0
