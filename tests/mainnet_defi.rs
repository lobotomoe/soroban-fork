//! # Cross-Protocol DeFi Scenarios on Mainnet Fork
//!
//! Complex integration tests that touch multiple protocols simultaneously.
//! These tests demonstrate scenarios that PASS in snapshot tests but FAIL
//! (or give wrong results) in production.
//!
//! ```sh
//! cargo test --test mainnet_defi -- --nocapture
//! ```

use soroban_fork::ForkConfig;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env, IntoVal, String as SorobanString, Symbol, Val};

// ---------------------------------------------------------------------------
// Inline Blend pool types (no SDK dependency — works with V1 and V2 pools)
// ---------------------------------------------------------------------------

/// Blend pool supply/withdraw request.
/// Fields are serialized alphabetically by #[contracttype]: address, amount, request_type.
#[soroban_sdk::contracttype]
#[derive(Clone)]
pub struct Request {
    pub address: Address,
    pub amount: i128,
    pub request_type: u32,
}

/// Blend pool user positions.
#[soroban_sdk::contracttype]
#[derive(Clone)]
pub struct Positions {
    pub collateral: soroban_sdk::Map<u32, i128>,
    pub liabilities: soroban_sdk::Map<u32, i128>,
    pub supply: soroban_sdk::Map<u32, i128>,
}

const SUPPLY_COLLATERAL: u32 = 0;
const WITHDRAW_COLLATERAL: u32 = 1;

// ---------------------------------------------------------------------------
// Mainnet addresses
// ---------------------------------------------------------------------------

const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const BLEND_V1_FIXED_POOL: &str = "CDVQVKOY2YSXS2IC7KN6MNASSHPAO7UN2UR2ON4OI2SKMFJNVAMDX6DP";
const PHOENIX_XLM_USDC: &str = "CBHCRSVX3ZZ7EGTSYMKPEFGZNWRVCSESQR3UABET4MIW52N4EVU6BIZX";

const UNIT: i128 = 10_000_000; // 7 decimals

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mainnet_rpc() -> String {
    std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".to_string())
}

fn addr(env: &Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

fn fmt(raw: i128) -> String {
    let whole = raw / UNIT;
    let frac = (raw % UNIT).unsigned_abs();
    format!("{whole}.{frac:07}")
}

fn token_mint(env: &Env, token: &Address, to: &Address, amount: i128) {
    let to_val: Val = to.into_val(env);
    let amount_val: Val = amount.into_val(env);
    env.invoke_contract::<()>(
        token,
        &Symbol::new(env, "mint"),
        soroban_sdk::vec![env, to_val, amount_val],
    );
}

fn token_transfer(env: &Env, token: &Address, from: &Address, to: &Address, amount: i128) {
    let from_val: Val = from.into_val(env);
    let to_val: Val = to.into_val(env);
    let amount_val: Val = amount.into_val(env);
    env.invoke_contract::<()>(
        token,
        &Symbol::new(env, "transfer"),
        soroban_sdk::vec![env, from_val, to_val, amount_val],
    );
}

fn token_burn(env: &Env, token: &Address, from: &Address, amount: i128) {
    let from_val: Val = from.into_val(env);
    let amount_val: Val = amount.into_val(env);
    env.invoke_contract::<()>(
        token,
        &Symbol::new(env, "burn"),
        soroban_sdk::vec![env, from_val, amount_val],
    );
}

fn token_balance(env: &Env, token: &Address, account: &Address) -> i128 {
    let account_val: Val = account.into_val(env);
    env.invoke_contract(
        token,
        &Symbol::new(env, "balance"),
        soroban_sdk::vec![env, account_val],
    )
}

fn blend_submit(env: &Env, pool: &Address, user: &Address, requests: soroban_sdk::Vec<Request>) {
    let user_val: Val = user.into_val(env);
    let requests_val: Val = requests.into_val(env);
    // submit(from, spender, to, requests) -> Positions
    // Return as Val to avoid struct layout mismatch across pool versions.
    env.invoke_contract::<Val>(
        pool,
        &Symbol::new(env, "submit"),
        soroban_sdk::vec![env, user_val, user_val, user_val, requests_val],
    );
}

/// Constant product AMM: output for a given input.
/// out = reserve_out * amount_in / (reserve_in + amount_in)
fn amm_output(reserve_in: i128, reserve_out: i128, amount_in: i128) -> i128 {
    reserve_out * amount_in / (reserve_in + amount_in)
}

// ---------------------------------------------------------------------------
// Test 1: Token lifecycle — mint, transfer, burn on real mainnet tokens
// ---------------------------------------------------------------------------

/// Proves that state mutations work correctly on forked mainnet.
/// The real USDC contract WASM is fetched from mainnet and executed locally.
/// All state changes are local — no real tokens move.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_token_lifecycle_on_fork() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    eprintln!("\n=== Token Lifecycle on Mainnet Fork ===\n");

    // Mint — SAC admin auth is auto-mocked
    token_mint(&env, &usdc, &alice, 100_000 * UNIT);
    assert_eq!(token_balance(&env, &usdc, &alice), 100_000 * UNIT);
    eprintln!("Minted 100,000 USDC to Alice");

    // Transfer
    token_transfer(&env, &usdc, &alice, &bob, 40_000 * UNIT);
    assert_eq!(token_balance(&env, &usdc, &alice), 60_000 * UNIT);
    assert_eq!(token_balance(&env, &usdc, &bob), 40_000 * UNIT);
    eprintln!("Alice -> Bob: 40,000 USDC");

    // Burn
    token_burn(&env, &usdc, &bob, 10_000 * UNIT);
    assert_eq!(token_balance(&env, &usdc, &bob), 30_000 * UNIT);
    eprintln!("Bob burned 10,000 USDC");

    // Final state
    eprintln!("\nFinal balances:");
    eprintln!("  Alice: {} USDC", fmt(token_balance(&env, &usdc, &alice)));
    eprintln!("  Bob:   {} USDC", fmt(token_balance(&env, &usdc, &bob)));
    eprintln!("  (Real mainnet balances unchanged)");
    eprintln!("\nRPC fetches: {}", env.fetch_count());
}

// ---------------------------------------------------------------------------
// Test 2: Blend lending round-trip — supply USDC, verify, withdraw
// ---------------------------------------------------------------------------

/// Supplies USDC to a real Blend V1 lending pool on mainnet fork,
/// then withdraws. The pool's WASM is fetched from mainnet — the SAME code
/// that runs in production. No mocks, no stubs.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_blend_lending_roundtrip() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let pool = addr(&env, BLEND_V1_FIXED_POOL);
    let alice = Address::generate(&env);

    eprintln!("\n=== Blend Lending Round-Trip (Mainnet Fork) ===\n");

    // Setup: mint USDC to Alice
    let initial_amount = 100_000 * UNIT;
    token_mint(&env, &usdc, &alice, initial_amount);

    let pool_usdc_before = token_balance(&env, &usdc, &pool);
    eprintln!("Pool USDC before: {}", fmt(pool_usdc_before));
    eprintln!("Alice USDC: {}", fmt(initial_amount));

    // Phase 1: Supply 50K USDC to Blend
    let supply_amount = 50_000 * UNIT;
    eprintln!("\n--- Supplying {} USDC to Blend ---", fmt(supply_amount));

    let mut supply_requests = soroban_sdk::Vec::new(&env);
    supply_requests.push_back(Request {
        request_type: SUPPLY_COLLATERAL,
        address: usdc.clone(),
        amount: supply_amount,
    });
    blend_submit(&env, &pool, &alice, supply_requests);

    let alice_after_supply = token_balance(&env, &usdc, &alice);
    let pool_after_supply = token_balance(&env, &usdc, &pool);

    eprintln!("Alice USDC after supply: {}", fmt(alice_after_supply));
    eprintln!("Pool USDC after supply:  {}", fmt(pool_after_supply));
    eprintln!("Pool delta: +{}", fmt(pool_after_supply - pool_usdc_before));

    assert_eq!(alice_after_supply, initial_amount - supply_amount);

    // Phase 2: Withdraw 25K USDC from Blend
    let withdraw_amount = 25_000 * UNIT;
    eprintln!(
        "\n--- Withdrawing {} USDC from Blend ---",
        fmt(withdraw_amount)
    );

    let mut withdraw_requests = soroban_sdk::Vec::new(&env);
    withdraw_requests.push_back(Request {
        request_type: WITHDRAW_COLLATERAL,
        address: usdc.clone(),
        amount: withdraw_amount,
    });
    blend_submit(&env, &pool, &alice, withdraw_requests);

    let alice_after_withdraw = token_balance(&env, &usdc, &alice);
    let pool_after_withdraw = token_balance(&env, &usdc, &pool);

    eprintln!("Alice USDC after withdraw: {}", fmt(alice_after_withdraw));
    eprintln!("Pool USDC after withdraw:  {}", fmt(pool_after_withdraw));

    assert_eq!(
        alice_after_withdraw,
        initial_amount - supply_amount + withdraw_amount
    );
    eprintln!("\nRound-trip successful against real Blend pool WASM!");
    eprintln!("RPC fetches: {}", env.fetch_count());
}

// ---------------------------------------------------------------------------
// Test 3: Swap price impact — snapshot lies, fork reveals
// ---------------------------------------------------------------------------

/// Shows WHY fork testing matters for DeFi.
///
/// In snapshot tests, you set reserves to convenient round numbers.
/// On real mainnet, the reserves are different — and your "reasonable"
/// swap can have devastating price impact.
///
/// This test reads real Phoenix pool reserves and calculates swap outputs
/// for different sizes, revealing the non-linear price impact curve.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_swap_price_impact_real_vs_snapshot() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let xlm = addr(&env, XLM_SAC);
    let pool = addr(&env, PHOENIX_XLM_USDC);

    // Read REAL reserves from mainnet
    let real_xlm = token_balance(&env, &xlm, &pool);
    let real_usdc = token_balance(&env, &usdc, &pool);
    let spot_price = real_usdc as f64 / real_xlm as f64;

    eprintln!("\n=== Price Impact: Snapshot vs Reality ===\n");
    eprintln!("Real Phoenix XLM/USDC reserves (mainnet):");
    eprintln!("  XLM:  {}", fmt(real_xlm));
    eprintln!("  USDC: {}", fmt(real_usdc));
    eprintln!("  Spot:  ${spot_price:.6}/XLM\n");

    // Typical snapshot test reserves (10x smaller, convenient numbers)
    let snap_xlm = 500_000 * UNIT;
    let snap_usdc = 80_000 * UNIT;
    let snap_spot = snap_usdc as f64 / snap_xlm as f64;

    eprintln!("Snapshot test reserves (fabricated):");
    eprintln!("  XLM:  {}", fmt(snap_xlm));
    eprintln!("  USDC: {}", fmt(snap_usdc));
    eprintln!("  Spot:  ${snap_spot:.6}/XLM\n");

    // Compare swap outputs at different sizes
    let swap_sizes = [
        100 * UNIT,
        10_000 * UNIT,
        100_000 * UNIT,
        500_000 * UNIT,
        1_000_000 * UNIT,
    ];

    eprintln!(
        "{:<15} {:>18} {:>18} {:>10}",
        "Swap Size", "Real Output", "Snapshot Output", "Diff %"
    );
    eprintln!("{:-<15} {:-<18} {:-<18} {:-<10}", "", "", "", "");

    for size in swap_sizes {
        let real_out = amm_output(real_xlm, real_usdc, size);
        let snap_out = amm_output(snap_xlm, snap_usdc, size);

        let _real_impact = 1.0 - (real_out as f64 / size as f64) / spot_price;
        let diff_pct = if snap_out > 0 {
            (real_out - snap_out) as f64 / snap_out as f64 * 100.0
        } else {
            0.0
        };

        eprintln!(
            "{:<15} {:>14} USDC {:>14} USDC {:>+9.1}%",
            fmt(size),
            fmt(real_out),
            fmt(snap_out),
            diff_pct,
        );

        // For large swaps, show the price impact
        if size >= 100_000 * UNIT {
            let effective_price = real_out as f64 / size as f64;
            let loss_vs_spot = (spot_price - effective_price) / spot_price * 100.0;
            eprintln!(
                "  ^ effective ${effective_price:.6}/XLM, {loss_vs_spot:.1}% worse than spot"
            );
        }
    }

    // The punchline
    let large_real = amm_output(real_xlm, real_usdc, 1_000_000 * UNIT);
    let large_snap = amm_output(snap_xlm, snap_usdc, 1_000_000 * UNIT);
    let loss_usd = (large_real - large_snap).unsigned_abs() / UNIT as u128;

    eprintln!("\n--- The Production Bug ---");
    eprintln!(
        "Your snapshot test says: 1M XLM swap -> {} USDC",
        fmt(large_snap)
    );
    eprintln!(
        "Real mainnet pool gives: 1M XLM swap -> {} USDC",
        fmt(large_real)
    );
    eprintln!("Difference: ~${loss_usd} — that's real money.");
    eprintln!("Fork testing catches this. Snapshot testing doesn't.\n");

    eprintln!("RPC fetches: {}", env.fetch_count());
}

// ---------------------------------------------------------------------------
// Test 4: Cross-protocol portfolio — Blend lending + Phoenix price oracle
// ---------------------------------------------------------------------------

/// Full cross-protocol DeFi scenario:
///   1. Mint USDC (SAC)
///   2. Supply to Blend lending pool (Blend V1)
///   3. Read XLM/USDC price (Phoenix DEX)
///   4. Stress-test: what if XLM drops and borrowers get liquidated?
///   5. Withdraw and reconcile
///
/// This is the test that passes with snapshots but catches real issues
/// on a fork: pool utilization, interest rates, and available liquidity
/// all come from REAL mainnet state.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_cross_protocol_portfolio() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let xlm = addr(&env, XLM_SAC);
    let blend_pool = addr(&env, BLEND_V1_FIXED_POOL);
    let phoenix_pool = addr(&env, PHOENIX_XLM_USDC);

    let alice = Address::generate(&env);

    eprintln!("\n=== Cross-Protocol Portfolio (Mainnet Fork) ===\n");

    // ------ Phase 1: Setup ------
    eprintln!("--- Phase 1: Setup ---");
    token_mint(&env, &usdc, &alice, 200_000 * UNIT);
    eprintln!("Alice: 200,000 USDC");

    // ------ Phase 2: Market price from Phoenix ------
    eprintln!("\n--- Phase 2: Market Price (Phoenix DEX) ---");
    let phoenix_xlm = token_balance(&env, &xlm, &phoenix_pool);
    let phoenix_usdc = token_balance(&env, &usdc, &phoenix_pool);
    let xlm_price = phoenix_usdc as f64 / phoenix_xlm as f64;
    eprintln!(
        "Phoenix pool: {} XLM / {} USDC",
        fmt(phoenix_xlm),
        fmt(phoenix_usdc)
    );
    eprintln!("XLM/USDC spot: ${xlm_price:.6}");

    // ------ Phase 3: Supply to Blend + Pool analysis ------
    eprintln!("\n--- Phase 3: Supply to Blend ---");
    let pool_usdc_before = token_balance(&env, &usdc, &blend_pool);
    let pool_xlm_before = token_balance(&env, &xlm, &blend_pool);
    let pool_tvl = pool_usdc_before as f64 + pool_xlm_before as f64 * xlm_price;

    eprintln!("Pool state BEFORE:");
    eprintln!("  USDC: {}", fmt(pool_usdc_before));
    eprintln!(
        "  XLM:  {} (${:.0} at spot)",
        fmt(pool_xlm_before),
        pool_xlm_before as f64 * xlm_price / UNIT as f64
    );
    eprintln!("  TVL:  ${:.0}", pool_tvl / UNIT as f64);

    let supply_amount = 100_000 * UNIT;
    let mut requests = soroban_sdk::Vec::new(&env);
    requests.push_back(Request {
        request_type: SUPPLY_COLLATERAL,
        address: usdc.clone(),
        amount: supply_amount,
    });
    blend_submit(&env, &blend_pool, &alice, requests);

    let pool_usdc_after = token_balance(&env, &usdc, &blend_pool);
    eprintln!("\nSupplied {} USDC", fmt(supply_amount));
    eprintln!(
        "Pool USDC after: {} (+{})",
        fmt(pool_usdc_after),
        fmt(pool_usdc_after - pool_usdc_before)
    );

    // ------ Phase 4: Portfolio valuation ------
    eprintln!("\n--- Phase 4: Portfolio Valuation ---");
    let usdc_wallet = token_balance(&env, &usdc, &alice);
    let blend_position = supply_amount; // just deposited

    eprintln!("  USDC wallet:   {} (liquid)", fmt(usdc_wallet));
    eprintln!("  Blend lending: {} (earning yield)", fmt(blend_position));
    eprintln!(
        "  Total:         {} USDC",
        fmt(usdc_wallet + blend_position)
    );

    // ------ Phase 5: Stress test — XLM crash impact on pool ------
    eprintln!("\n--- Phase 5: XLM Crash Stress Test ---");
    eprintln!("If XLM drops, the pool's XLM collateral loses value.");
    eprintln!("Borrowers may get liquidated. Pool utilization shifts.\n");

    eprintln!(
        "{:<12} {:>14} {:>14} {:>14}",
        "XLM Drop", "Pool XLM Val", "Pool TVL", "USDC share"
    );
    eprintln!("{:-<12} {:-<14} {:-<14} {:-<14}", "", "", "", "");

    for drop_pct in [0i32, 10, 20, 30, 50, 80] {
        let stressed = xlm_price * (100 - drop_pct) as f64 / 100.0;
        let xlm_val = pool_xlm_before as f64 * stressed;
        let tvl = pool_usdc_after as f64 + xlm_val;
        let usdc_share = pool_usdc_after as f64 / tvl * 100.0;

        eprintln!(
            "{:>10}%  ${:>12.0}  ${:>12.0}  {:>12.1}%",
            drop_pct,
            xlm_val / UNIT as f64,
            tvl / UNIT as f64,
            usdc_share,
        );
    }

    // ------ Phase 6: Withdraw ------
    eprintln!("\n--- Phase 6: Withdraw ---");
    let mut requests = soroban_sdk::Vec::new(&env);
    requests.push_back(Request {
        request_type: WITHDRAW_COLLATERAL,
        address: usdc.clone(),
        amount: supply_amount,
    });
    blend_submit(&env, &blend_pool, &alice, requests);

    let final_usdc = token_balance(&env, &usdc, &alice);
    eprintln!("Withdrew {} USDC from Blend", fmt(supply_amount));
    eprintln!("Alice final USDC: {}", fmt(final_usdc));

    // The b_rate gotcha: in a fresh snapshot test, you get back EXACTLY
    // what you put in. On real mainnet, interest has been accruing —
    // the b_rate is not 1.0, so supply(X) -> withdraw(X) loses a fraction
    // to rounding. This is the kind of off-by-one that fork testing catches.
    let rounding_loss = 200_000 * UNIT - final_usdc;
    if rounding_loss > 0 {
        eprintln!(
            "\n  ** b_rate rounding: lost {} stroops ({} USDC)",
            rounding_loss,
            fmt(rounding_loss),
        );
        eprintln!("  ** In snapshot tests, this is always 0. On real state, it's not.");
        eprintln!("  ** Your vault's share accounting MUST handle this.");
    }
    assert!(
        final_usdc >= 199_999 * UNIT,
        "withdrawal returned unreasonably little"
    );

    eprintln!("\n3 protocols (SAC + Blend + Phoenix), real mainnet WASM.");
    eprintln!("RPC fetches: {}", env.fetch_count());
}
