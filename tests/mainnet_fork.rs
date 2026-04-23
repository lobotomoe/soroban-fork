//! # Mainnet Fork Examples
//!
//! Real integration tests against live Stellar mainnet state.
//! Each test forks mainnet via RPC and reads actual DeFi contract state.
//!
//! Run with:
//! ```sh
//! cargo test --test mainnet_fork -- --nocapture
//! ```
//!
//! Set `MAINNET_RPC_URL` to override the default public RPC endpoint:
//! ```sh
//! MAINNET_RPC_URL=https://your-rpc.example.com cargo test --test mainnet_fork -- --nocapture
//! ```

use soroban_fork::ForkConfig;
use soroban_sdk::{Address, IntoVal, String as SorobanString, Symbol, Val};

// ---------------------------------------------------------------------------
// Mainnet contract addresses
// ---------------------------------------------------------------------------

/// Native XLM Stellar Asset Contract.
const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

/// USDC (Circle) Stellar Asset Contract.
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

/// BLND governance token SAC.
const BLND_SAC: &str = "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY";

/// Blend V2 backstop contract (holds BLND:USDC LP and governs pools).
const BLEND_BACKSTOP: &str = "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7";

/// Blend V1 Fixed XLM-USDC lending pool (one of the most active).
const BLEND_V1_FIXED_POOL: &str = "CDVQVKOY2YSXS2IC7KN6MNASSHPAO7UN2UR2ON4OI2SKMFJNVAMDX6DP";

/// Blend V1 YieldBlox pool.
const BLEND_V1_YIELDBLOX: &str = "CBP7NO6F7FRDHSOFQBT2L2UWYIZ2PU76JKVRYAQTG3KZSQLYAOKIF2WB";

/// Phoenix DEX: XLM/USDC liquidity pool (most active Phoenix pair).
const PHOENIX_XLM_USDC: &str = "CBHCRSVX3ZZ7EGTSYMKPEFGZNWRVCSESQR3UABET4MIW52N4EVU6BIZX";

/// Phoenix DEX: PHO/USDC liquidity pool.
const PHOENIX_PHO_USDC: &str = "CD5XNKK3B6BEF2N7ULNHHGAMOKZ7P6456BFNIHRF4WNTEDKBRWAE7IAA";

/// Soroswap AMM router.
const SOROSWAP_ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";

/// Aquarius AMM router (highest TVL on Soroban).
const AQUARIUS_ROUTER: &str = "CBQDHNBFBZYE4MKPWBSJOPIYLW4SFSXAXUTSXJN76GNKYVYPCKWC6QUK";

/// Blend V2 BLND:USDC Comet LP pool.
const BLEND_COMET_LP: &str = "CAS3FL6TLZKDGGSISDBWGGPXT3NRR4DYTZD7YOD3HMYO6LTJUVGRVEAM";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mainnet_rpc() -> String {
    std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".to_string())
}

fn addr(env: &soroban_sdk::Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

fn token_name(env: &soroban_sdk::Env, token: &Address) -> soroban_sdk::String {
    env.invoke_contract(token, &Symbol::new(env, "name"), soroban_sdk::vec![env])
}

fn token_symbol(env: &soroban_sdk::Env, token: &Address) -> soroban_sdk::String {
    env.invoke_contract(token, &Symbol::new(env, "symbol"), soroban_sdk::vec![env])
}

fn token_decimals(env: &soroban_sdk::Env, token: &Address) -> u32 {
    env.invoke_contract(token, &Symbol::new(env, "decimals"), soroban_sdk::vec![env])
}

fn token_balance(env: &soroban_sdk::Env, token: &Address, account: &Address) -> i128 {
    let account_val: Val = account.into_val(env);
    env.invoke_contract(
        token,
        &Symbol::new(env, "balance"),
        soroban_sdk::vec![env, account_val],
    )
}

const USDC_DECIMALS: u32 = 7;
const XLM_DECIMALS: u32 = 7;

fn format_amount(raw: i128, decimals: u32) -> String {
    let divisor = 10i128.pow(decimals);
    let whole = raw / divisor;
    let frac = (raw % divisor).unsigned_abs();
    format!("{whole}.{frac:0>width$}", width = decimals as usize)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Simple: read token metadata from the three major Stellar tokens.
/// Demonstrates basic lazy-fetch: instance + WASM + storage per token.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_token_metadata() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let xlm = addr(&env, XLM_SAC);
    let usdc = addr(&env, USDC_SAC);
    let blnd = addr(&env, BLND_SAC);

    eprintln!("\n=== Stellar Mainnet Token Metadata ===\n");

    for (label, token) in [("XLM", &xlm), ("USDC", &usdc), ("BLND", &blnd)] {
        let name = token_name(&env, token);
        let symbol = token_symbol(&env, token);
        let decimals = token_decimals(&env, token);
        eprintln!("{label}:");
        eprintln!("  name     = {name:?}");
        eprintln!("  symbol   = {symbol:?}");
        eprintln!("  decimals = {decimals}");
    }

    // SAC tokens always have 7 decimals
    assert_eq!(token_decimals(&env, &xlm), 7);
    assert_eq!(token_decimals(&env, &usdc), 7);
    assert_eq!(token_decimals(&env, &blnd), 7);

    eprintln!("\nRPC fetches: {}", env.fetch_count());
}

/// Medium: measure how much capital is locked in Blend Protocol pools.
/// Cross-contract queries: call `balance()` on token contracts with pool addresses.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_blend_pool_tvl() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let xlm = addr(&env, XLM_SAC);
    let blnd = addr(&env, BLND_SAC);

    let v1_fixed = addr(&env, BLEND_V1_FIXED_POOL);
    let v1_yieldblox = addr(&env, BLEND_V1_YIELDBLOX);
    let backstop = addr(&env, BLEND_BACKSTOP);
    let comet = addr(&env, BLEND_COMET_LP);

    eprintln!("\n=== Blend Protocol TVL (Mainnet Fork) ===\n");

    // V1 Fixed XLM-USDC Pool
    let fixed_usdc = token_balance(&env, &usdc, &v1_fixed);
    let fixed_xlm = token_balance(&env, &xlm, &v1_fixed);
    eprintln!("Blend V1 Fixed Pool:");
    eprintln!("  USDC: {}", format_amount(fixed_usdc, USDC_DECIMALS));
    eprintln!("  XLM:  {}", format_amount(fixed_xlm, XLM_DECIMALS));

    // V1 YieldBlox Pool
    let yb_usdc = token_balance(&env, &usdc, &v1_yieldblox);
    let yb_xlm = token_balance(&env, &xlm, &v1_yieldblox);
    eprintln!("\nBlend V1 YieldBlox Pool:");
    eprintln!("  USDC: {}", format_amount(yb_usdc, USDC_DECIMALS));
    eprintln!("  XLM:  {}", format_amount(yb_xlm, XLM_DECIMALS));

    // Backstop
    let backstop_blnd = token_balance(&env, &blnd, &backstop);
    let backstop_usdc = token_balance(&env, &usdc, &backstop);
    eprintln!("\nBlend V2 Backstop:");
    eprintln!("  BLND: {}", format_amount(backstop_blnd, 7));
    eprintln!("  USDC: {}", format_amount(backstop_usdc, USDC_DECIMALS));

    // Comet LP (BLND:USDC)
    let comet_blnd = token_balance(&env, &blnd, &comet);
    let comet_usdc = token_balance(&env, &usdc, &comet);
    eprintln!("\nBlend Comet LP (BLND:USDC):");
    eprintln!("  BLND: {}", format_amount(comet_blnd, 7));
    eprintln!("  USDC: {}", format_amount(comet_usdc, USDC_DECIMALS));

    // Totals
    let total_usdc = fixed_usdc + yb_usdc + backstop_usdc + comet_usdc;
    let total_xlm = fixed_xlm + yb_xlm;
    eprintln!("\n--- Totals ---");
    eprintln!(
        "  USDC across Blend: {}",
        format_amount(total_usdc, USDC_DECIMALS)
    );
    eprintln!(
        "  XLM  across Blend: {}",
        format_amount(total_xlm, XLM_DECIMALS)
    );

    assert!(fixed_usdc >= 0);
    assert!(fixed_xlm >= 0);

    eprintln!("\nRPC fetches: {}", env.fetch_count());
}

/// Medium-Hard: derive the XLM/USDC price from Phoenix DEX pool reserves.
/// AMM reserves = token balances held by the pool contract.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_phoenix_xlm_price() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let xlm = addr(&env, XLM_SAC);
    let pool = addr(&env, PHOENIX_XLM_USDC);

    eprintln!("\n=== Phoenix DEX: XLM/USDC Pool (Mainnet Fork) ===\n");

    let xlm_reserve = token_balance(&env, &xlm, &pool);
    let usdc_reserve = token_balance(&env, &usdc, &pool);

    eprintln!("Pool reserves:");
    eprintln!("  XLM:  {}", format_amount(xlm_reserve, XLM_DECIMALS));
    eprintln!("  USDC: {}", format_amount(usdc_reserve, USDC_DECIMALS));

    if xlm_reserve > 0 {
        // Both have 7 decimals so raw ratio = price
        let price_cents = usdc_reserve * 10_000 / xlm_reserve;
        let price_dollars = price_cents as f64 / 10_000.0;
        eprintln!("\nDerived XLM price: ${price_dollars:.4} USDC");
    }

    // Also check PHO/USDC pool
    let pho_pool = addr(&env, PHOENIX_PHO_USDC);
    let pho_usdc_reserve = token_balance(&env, &usdc, &pho_pool);
    eprintln!("\nPhoenix PHO/USDC pool:");
    eprintln!(
        "  USDC reserve: {}",
        format_amount(pho_usdc_reserve, USDC_DECIMALS)
    );

    assert!(xlm_reserve >= 0);
    assert!(usdc_reserve >= 0);

    eprintln!("\nRPC fetches: {}", env.fetch_count());
}

/// Complex: survey the Stellar DeFi landscape in a single fork.
/// One fork, multiple protocols, cross-contract balance queries.
#[test]
#[ignore = "requires live Stellar mainnet RPC (opt-in via `cargo test -- --ignored`)"]
fn test_defi_landscape() {
    let env = ForkConfig::new(mainnet_rpc()).build().expect("fork setup");
    env.mock_all_auths();

    let usdc = addr(&env, USDC_SAC);
    let xlm = addr(&env, XLM_SAC);

    let protocols: Vec<(&str, &str)> = vec![
        ("Blend V1 Fixed Pool", BLEND_V1_FIXED_POOL),
        ("Blend V1 YieldBlox", BLEND_V1_YIELDBLOX),
        ("Blend V2 Backstop", BLEND_BACKSTOP),
        ("Blend Comet LP", BLEND_COMET_LP),
        ("Phoenix XLM/USDC", PHOENIX_XLM_USDC),
        ("Phoenix PHO/USDC", PHOENIX_PHO_USDC),
        ("Soroswap Router", SOROSWAP_ROUTER),
        ("Aquarius Router", AQUARIUS_ROUTER),
    ];

    eprintln!("\n=== Stellar DeFi Landscape (Mainnet Fork) ===\n");
    eprintln!("{:<25} {:>15} {:>15}", "Protocol", "USDC", "XLM");
    eprintln!("{:-<25} {:-<15} {:-<15}", "", "", "");

    let mut total_usdc: i128 = 0;
    let mut total_xlm: i128 = 0;

    for (name, contract_id) in &protocols {
        let contract = addr(&env, contract_id);
        let usdc_bal = token_balance(&env, &usdc, &contract);
        let xlm_bal = token_balance(&env, &xlm, &contract);

        eprintln!(
            "{:<25} {:>15} {:>15}",
            name,
            format_amount(usdc_bal, USDC_DECIMALS),
            format_amount(xlm_bal, XLM_DECIMALS),
        );

        total_usdc += usdc_bal;
        total_xlm += xlm_bal;
    }

    eprintln!("{:-<25} {:-<15} {:-<15}", "", "", "");
    eprintln!(
        "{:<25} {:>15} {:>15}",
        "TOTAL",
        format_amount(total_usdc, USDC_DECIMALS),
        format_amount(total_xlm, XLM_DECIMALS),
    );

    eprintln!("\nRPC fetches: {}", env.fetch_count());
    eprintln!(
        "Cache entries: {} (reusable across tests with cache_file)",
        env.fetch_count()
    );
}
