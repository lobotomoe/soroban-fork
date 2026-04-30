//! # Auth-tree end-to-end showcase
//!
//! Live-mainnet check that [`ForkedEnv::auth_tree`] threads through to
//! the host's recording auth manager and that the rendered output
//! matches the call we just made.
//!
//! The two scenarios this file proves:
//!
//! 1. **Empty before any invoke.** Calling `auth_tree()` before any
//!    `invoke_contract` ran returns an empty tree (the host has no
//!    "previous invocation" to read payloads from). This is the path
//!    the README's "Common pitfalls" section points debug sessions to,
//!    so the empty state has to render cleanly without panicking.
//!
//! 2. **Captures a real transfer's `require_auth` demand.** After
//!    `usdc.transfer(alice, bob, …)`, `auth_tree()` exposes a payload
//!    naming the `transfer` function and the signer (`alice`).
//!    `print_auth_tree()` writes a Foundry-style indented view to
//!    stderr.
//!
//! Run with:
//! ```sh
//! cargo test --test auth_debug -- --ignored --nocapture
//! ```

use soroban_fork::ForkConfig;
use soroban_sdk::{Address, IntoVal, String as SorobanString, Symbol, Val};

/// USDC (Circle) Stellar Asset Contract — picked because the SAC's
/// `transfer` demands `require_auth` from `from`, which is exactly
/// what we want the auth recorder to capture.
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

fn mainnet_rpc() -> String {
    std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".to_string())
}

fn addr(env: &soroban_sdk::Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

/// Sanity check: with no top-level invocation having run yet, the
/// host's `previous_authorization_manager` is `None` and our wrapper
/// gracefully renders an empty tree rather than panicking.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn auth_tree_is_empty_before_any_invocation() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let tree = env.auth_tree();
    assert!(
        tree.is_empty(),
        "expected empty tree before any invoke, got {} payloads",
        tree.payload_count()
    );
    assert_eq!(tree.invocation_count(), 0);

    let rendered = tree.to_string();
    assert!(
        rendered.starts_with("[AUTH] (empty"),
        "empty tree should render the explanatory placeholder; got:\n{rendered}"
    );
}

/// End-to-end: a successful USDC `transfer(alice, bob, amount)` should
/// record one auth payload — Alice's signature on the `transfer`
/// invocation. Verifies both the structured accessors and the
/// rendered Foundry-style output.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn transfer_records_alice_auth_payload() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);

    // Pick the first two pre-funded test accounts. Account 0 is the
    // sender (Alice), account 1 is the recipient (Bob). Both already
    // have an authorized USDC trustline pre-created at fork build,
    // so the transfer doesn't need any extra setup beyond funding.
    let accounts = env.test_accounts();
    assert!(
        accounts.len() >= 2,
        "this test requires at least 2 pre-funded test accounts"
    );
    let alice_strkey = accounts[0].account_strkey();
    let bob_strkey = accounts[1].account_strkey();
    let alice = addr(&env, &alice_strkey);
    let bob = addr(&env, &bob_strkey);

    // Fund Alice with 100 USDC so the transfer doesn't fail on the
    // balance check. `deal_token` is a separate top-level invoke; the
    // recording auth manager will overwrite its payload set when the
    // transfer call below runs, so we don't need to clear anything.
    const HUNDRED_USDC: i128 = 100 * 10_000_000;
    env.deal_token(&usdc, &alice, HUNDRED_USDC);

    // The `transfer` call. Demands `require_auth` from `from` (Alice).
    let amount: i128 = 25 * 10_000_000;
    let mut args = soroban_sdk::Vec::new(&env);
    let alice_val: Val = alice.into_val(&*env);
    let bob_val: Val = bob.into_val(&*env);
    let amount_val: Val = amount.into_val(&*env);
    args.push_back(alice_val);
    args.push_back(bob_val);
    args.push_back(amount_val);
    env.invoke_contract::<()>(&usdc, &Symbol::new(&env, "transfer"), args);

    // Inspect the recorded auth tree.
    let tree = env.auth_tree();

    assert!(
        !tree.is_empty(),
        "transfer should have produced at least one auth payload"
    );

    // The transfer demands exactly one `require_auth` (from Alice). Per
    // payload that's one root invocation with no sub-invocations.
    assert_eq!(
        tree.payload_count(),
        1,
        "expected exactly 1 payload for a single transfer; got tree:\n{tree}"
    );
    assert_eq!(
        tree.invocation_count(),
        1,
        "expected exactly 1 invocation node; got tree:\n{tree}"
    );

    // Render and assert the human-readable output names the transfer
    // and includes Alice's strkey (or its abbreviated form).
    let rendered = tree.to_string();
    assert!(
        rendered.contains("transfer("),
        "rendered tree should name the transfer call:\n{rendered}"
    );
    let alice_prefix = &alice_strkey[..4];
    let alice_suffix = &alice_strkey[alice_strkey.len() - 4..];
    assert!(
        rendered.contains(alice_prefix) && rendered.contains(alice_suffix),
        "rendered tree should mention Alice's address ({alice_strkey}):\n{rendered}"
    );

    // Also exercise `print_auth_tree` so its stderr path is covered. The
    // assertion is implicit — the call must not panic. Useful for eyeball
    // verification when the test runs with `--nocapture`.
    eprintln!("--- captured auth tree ---");
    env.print_auth_tree();
    eprintln!("--- end ---");
}
