# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(with pre-1.0 latitude — minor versions can break in well-justified cases).

## [Unreleased]

## [0.9.2] — 2026-05-01

### Added
- **`workspace_wasm_in(manifest_dir, crate_name, target, profile)`** —
  the most general form of the workspace-wasm helper. Takes an
  optional explicit `manifest_dir` so the caller can root cargo
  invocations at a specific directory instead of inheriting the
  process's cwd. `workspace_wasm` and `workspace_wasm_with` are now
  thin wrappers that pass `None`.
- **`tests/workspace_wasm.rs`** — closes the testability gap that
  shipped with v0.9.1. Two scenarios:
  - `errors_when_crate_is_not_a_workspace_member` — runs
    unconditionally in CI. Spins up a fixture workspace in a tempdir
    and asserts that requesting an unknown crate produces a
    well-formed `ForkError::Workspace` with the missing name and the
    actual workspace members listed.
  - `builds_real_contract_in_isolated_workspace` — `#[ignore]`d
    end-to-end test. Writes a 2-crate fixture (root + a tiny
    `cdylib` Soroban contract), invokes `workspace_wasm_in`, and
    verifies the resulting WASM starts with the `\\0asm` magic. Run
    with `cargo test --test workspace_wasm -- --ignored`. Builds 619
    bytes on macOS / soroban-sdk 25.3.0 — exactly matching the
    one-time smoke test that backed the v0.9.1 release.

### Why this exists
- v0.9.1 shipped with the public `workspace_wasm` exercising no
  automated test path. The end-to-end was a manual smoke at
  `/tmp/sfork-smoke` that didn't survive past the release. v0.9.2
  closes that gap honestly: with the new `manifest_dir` seam, the
  same end-to-end coverage now lives in `tests/workspace_wasm.rs`
  and is reproducible by anyone running the test suite.
- The unconditional error-path test means even default `cargo test`
  exercises the metadata-parsing pipeline against a real cargo
  invocation, not just unit-test mocks.

### Notes
- No behavioural change to `workspace_wasm` / `workspace_wasm_with`
  for existing callers — they delegate to the new
  `workspace_wasm_in(None, …)` form.
- The integration test fixture is allocated under
  `std::env::temp_dir()` with a pid + nanosecond suffix so concurrent
  cargo `--test-threads=N` runs never collide.

## [0.9.1] — 2026-05-01

### Added
- **`workspace_wasm(crate_name)`** — closes the `include_bytes!`
  rebuild trap reported in the v0.8 user feedback round. Invokes
  `cargo build -p <crate_name> --target wasm32v1-none --release` at
  test runtime, then reads the resulting WASM bytes. Cargo's
  incremental compilation makes the rebuild sub-second when the source
  hasn't actually changed, so calling this helper on every test is
  cheap. Crate name is validated against `cargo metadata`'s
  `packages[].name` cross-referenced with `workspace_members` — names
  must match exactly (no substring matching). `CARGO_TARGET_DIR`
  overrides are honored.
- **`workspace_wasm_with(crate_name, target, profile)`** — escape hatch
  for projects pinned to an older Soroban target
  (`wasm32-unknown-unknown` for soroban-sdk <25, etc.) or non-`release`
  profiles for size optimisation.
- New `workspace` module exposing both helpers, re-exported at the
  crate root (`soroban_fork::workspace_wasm` and
  `soroban_fork::workspace_wasm_with`).
- `ForkError::Workspace(String)` variant — surfaced verbatim from
  cargo's stderr when `cargo metadata` or `cargo build` fails, so a
  failing test reports the underlying compilation error directly.
- Module-level docs explain the rebuild-trap rationale and document
  the `build.rs` alternative for test suites that load wasm in many
  places (cheaper than running `cargo build` per test).

### Fixed
- Cargo ≥ 1.77 emits workspace-member IDs in two formats: with an
  explicit `<name>@<version>` suffix when the crate name differs from
  the directory basename, and *without* the `@<version>` suffix when
  they match. The first iteration of `workspace_wasm` only handled
  the explicit form and rejected legitimate workspace members. Caught
  by an end-to-end smoke test against a real `demo_contract`
  workspace fixture; fixed by switching to the `packages[].name`
  cross-reference, which is stable across Cargo versions.

### Notes
- This is the smallest of the v0.8 paper-cuts left after v0.8.8 docs
  and v0.9.0 auth_tree introspection. With it, every actionable item
  from the original integrator-feedback round is closed except the
  ones that need upstream `rs-soroban-env` cooperation
  (`strict_auth(true)`, `last_auth_failure()`).

## [0.9.0] — 2026-05-01

### Added
- **`ForkedEnv::auth_tree()`** — Foundry-`-vvvv`-style introspection of
  the recording auth manager's payload set after a top-level
  `invoke_contract`. Names every signer, nonce, contract, function, and
  arg list that `require_auth` was demanded for. Closes the
  longest-running paper-cut from the v0.8 user feedback round
  ("`Error(Auth, InvalidAction)` is a black box; the panic message
  alone isn't enough").
- **`ForkedEnv::print_auth_tree()`** — convenience wrapper that prints
  to stderr, parallel to `print_trace()`.
- **`ForkedEnv::auth_payloads()`** — raw access to
  `Vec<RecordedAuthPayload>` for programmatic assertions, parallel to
  `diagnostic_events()`.
- New `auth_tree` module exposing `AuthTree` with `payload_count()`,
  `invocation_count()` (recursive across `sub_invocations`), and
  `is_empty()` accessors.
- `tests/auth_debug.rs` — live-mainnet showcase. Verified output:
  ```text
  [AUTH]
    payload #0  signer=GA62…J3CT  nonce=5541220902715666415
      [CCW6…MI75] transfer(GA62…J3CT, GB7O…7AMM, 250000000)
  ```
- README "Auth introspection" section under the `## API` heading,
  cross-linked from "Common pitfalls / Debugging cross-contract auth
  chains".

### Changed
- `trace::render_scval` and `trace::render_address` are now
  `pub(crate)` so the auth-tree renderer produces arguments in the
  same shape as the trace renderer (single source of truth for compact
  `ScVal` / `ScAddress` formatting).

### Dropped from scope (will not ship)
- ~~`ForkConfig::strict_auth(true)`~~ — research showed the host's
  `disable_non_root_auth` flag has no public getter. Any
  implementation we shipped would have been a half-measure with no
  real enforcement; we'd rather document the trap (which v0.8.8 already
  did in the "Common pitfalls" README section) than ship a stub.
  Re-evaluate when `rs-soroban-env` exposes the flag.
- ~~`ForkedEnv::last_auth_failure()`~~ — research showed
  `(Auth, InvalidAction)` is constructed locally inside the host with
  only the address in its diagnostic args; the failed contract,
  function, and expected authorizer are not persisted to any accessor
  we can read out. The honest partial answer (`auth_tree()` after a
  failed call shows what was recorded before the failure point) is
  documented in the `auth_tree` module docs and the README.
  Re-evaluate when `rs-soroban-env` persists the failure context.

### Notes
- This release responds directly to the first integrator feedback
  round (see v0.8.8 CHANGELOG). v0.8.8 shipped pure docs covering all
  five paper-cuts; v0.9.0 ships the largest of the auth-related
  features the docs pointed at as "planned".
- The `wasm rebuild trap` mitigation
  (`ForkConfig::register_wasm_from_workspace`) is deferred to v0.9.x.

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

[Unreleased]: https://github.com/lobotomoe/soroban-fork/compare/v0.9.2...HEAD
[0.9.2]: https://github.com/lobotomoe/soroban-fork/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/lobotomoe/soroban-fork/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/lobotomoe/soroban-fork/compare/v0.8.8...v0.9.0
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
