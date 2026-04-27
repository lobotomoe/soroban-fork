//! # Mainnet trace capture
//!
//! End-to-end check that [`ForkConfig::tracing`] threads through to the
//! host's diagnostic event buffer and that [`Trace::from_events`] parses
//! a real call stream produced by `Env::invoke_contract`.
//!
//! Run with:
//! ```sh
//! cargo test --test mainnet_trace -- --ignored --nocapture
//! ```

use soroban_fork::{ForkConfig, TraceResult};
use soroban_sdk::{Address, IntoVal, String as SorobanString, Symbol, Val};

const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

fn mainnet_rpc() -> String {
    std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".to_string())
}

fn addr(env: &soroban_sdk::Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

/// Sanity: tracing-off env returns an empty trace even after invocations.
/// Catches the "default-on" regression that would silently bloat memory.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn tracing_off_returns_empty_trace() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let xlm = addr(&env, XLM_SAC);
    let _: u32 = env.invoke_contract(
        &xlm,
        &Symbol::new(&env, "decimals"),
        soroban_sdk::vec![&env],
    );

    let trace = env.trace();
    assert!(
        trace.roots.is_empty(),
        "tracing(false) must capture no frames; got {} roots",
        trace.roots.len()
    );
}

/// Single SAC call → one root frame, function "decimals", returns U32(7).
/// Validates the simplest end-to-end path: enable, invoke, parse.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn tracing_captures_simple_invocation() {
    let env = ForkConfig::new(mainnet_rpc())
        .tracing(true)
        .build()
        .expect("fork setup");
    env.mock_all_auths();

    let xlm = addr(&env, XLM_SAC);
    let _: u32 = env.invoke_contract(
        &xlm,
        &Symbol::new(&env, "decimals"),
        soroban_sdk::vec![&env],
    );

    let trace = env.trace();
    assert!(!trace.roots.is_empty(), "expected at least one root frame");

    // Find the user-level call. The host may emit additional internal
    // diagnostic frames (e.g., authorization checks) — we don't assume
    // exact tree shape, only that our call appears.
    let decimals_frame =
        find_frame(&trace, "decimals").expect("trace must contain a 'decimals' frame");
    assert_eq!(decimals_frame.contract.to_string(), XLM_SAC);
    match &decimals_frame.result {
        TraceResult::Returned(soroban_env_host::xdr::ScVal::U32(7)) => {}
        other => panic!("expected Returned(U32(7)), got {other:?}"),
    }
    assert!(!decimals_frame.rolled_back);

    eprintln!("\n{trace}");
}

/// Cross-contract call: balance(addr) on USDC SAC. The SAC's balance
/// implementation reads ledger entries but doesn't make further
/// `invoke_contract` calls, so we expect a single user-visible frame.
/// This test exists mostly to confirm the renderer handles real
/// ScAddress arguments and large i128 returns without falling apart.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn tracing_captures_balance_call_with_address_arg() {
    let env = ForkConfig::new(mainnet_rpc())
        .tracing(true)
        .build()
        .expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    // Read the balance of an arbitrary contract address. Whether or not
    // the address has a balance is irrelevant; we just need the call to
    // execute so we can inspect the trace.
    let target = addr(&env, XLM_SAC);
    // `IntoVal` is implemented over the SDK's `&Env`, not `&ForkedEnv`.
    // `env.env()` hands us the inner reference unambiguously — calling
    // `into_val(&env)` would route through `&ForkedEnv` and fail to
    // resolve a trait impl, despite the Deref relationship.
    let target_val: Val = target.into_val(env.env());
    let args = soroban_sdk::vec![env.env(), target_val];
    let _: i128 = env.invoke_contract(&usdc, &Symbol::new(env.env(), "balance"), args);

    let trace = env.trace();
    let balance_frame =
        find_frame(&trace, "balance").expect("trace must contain a 'balance' frame");
    assert_eq!(balance_frame.args.len(), 1, "balance(addr) takes one arg");

    // Print so a human running --nocapture can eyeball the renderer.
    eprintln!("\n{trace}");
}

/// Per-invocation scoping: each top-level `invoke_contract` clears the
/// host's events buffer (`InvocationMeter::push_invocation` calls
/// `events.clear()`). Two sequential calls on different SACs must
/// therefore produce two distinct traces — and the second trace must
/// contain only the second call's frames.
///
/// This test pins the per-invocation-reset invariant. If a future host
/// release stops clearing on push, [`Trace`] semantics change and we
/// need to update the docs (and probably introduce a `clear_trace()`
/// helper for users who relied on the reset).
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn tracing_buffer_resets_per_top_level_invocation() {
    let env = ForkConfig::new(mainnet_rpc())
        .tracing(true)
        .build()
        .expect("fork setup");
    env.mock_all_auths();

    let xlm = addr(&env, XLM_SAC);
    let usdc = addr(&env, USDC_SAC);

    // First call: USDC.decimals
    let _: u32 = env.invoke_contract(
        &usdc,
        &Symbol::new(env.env(), "decimals"),
        soroban_sdk::vec![env.env()],
    );
    let trace_after_usdc = env.trace();

    // Second call: XLM.decimals
    let _: u32 = env.invoke_contract(
        &xlm,
        &Symbol::new(env.env(), "decimals"),
        soroban_sdk::vec![env.env()],
    );
    let trace_after_xlm = env.trace();

    // Both traces must be non-empty
    assert!(
        !trace_after_usdc.roots.is_empty(),
        "first trace must capture the USDC call"
    );
    assert!(
        !trace_after_xlm.roots.is_empty(),
        "second trace must capture the XLM call"
    );

    // After the second call, the buffer was cleared and now shows only XLM
    assert!(
        find_frame_with_contract(&trace_after_xlm, XLM_SAC).is_some(),
        "second trace must contain XLM frame"
    );
    assert!(
        find_frame_with_contract(&trace_after_xlm, USDC_SAC).is_none(),
        "second trace must NOT contain the USDC frame from the first call \
         — the events buffer should have been cleared"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Depth-first search for the first frame with the given function name.
/// Roots that aren't named the target are searched recursively for
/// nested calls.
fn find_frame<'a>(
    trace: &'a soroban_fork::Trace,
    function: &str,
) -> Option<&'a soroban_fork::TraceFrame> {
    for root in &trace.roots {
        if let Some(found) = find_in_frame(root, function) {
            return Some(found);
        }
    }
    None
}

fn find_in_frame<'a>(
    frame: &'a soroban_fork::TraceFrame,
    function: &str,
) -> Option<&'a soroban_fork::TraceFrame> {
    if frame.function == function {
        return Some(frame);
    }
    for child in &frame.children {
        if let Some(found) = find_in_frame(child, function) {
            return Some(found);
        }
    }
    None
}

/// Depth-first search for the first frame whose contract address (in
/// strkey form) matches `contract_id`.
fn find_frame_with_contract<'a>(
    trace: &'a soroban_fork::Trace,
    contract_id: &str,
) -> Option<&'a soroban_fork::TraceFrame> {
    for root in &trace.roots {
        if let Some(found) = find_contract_in_frame(root, contract_id) {
            return Some(found);
        }
    }
    None
}

fn find_contract_in_frame<'a>(
    frame: &'a soroban_fork::TraceFrame,
    contract_id: &str,
) -> Option<&'a soroban_fork::TraceFrame> {
    if frame.contract.to_string() == contract_id {
        return Some(frame);
    }
    for child in &frame.children {
        if let Some(found) = find_contract_in_frame(child, contract_id) {
            return Some(found);
        }
    }
    None
}
