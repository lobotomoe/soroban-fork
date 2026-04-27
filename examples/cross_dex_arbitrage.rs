//! "Phoenix XLM/USDC vs Soroswap XLM/USDC — same trade, two DEXes,
//! how big is the price gap right now?"
//!
//! Without soroban-fork: deploy two AMM clones on testnet, fabricate
//! "reasonable" reserves, hope the disparity looks like prod. Numbers
//! are guesses; arbitrage signals are made-up.
//!
//! With soroban-fork: query Phoenix's `simulate_swap` and Soroswap's
//! `router_get_amounts_out` against live mainnet state. Both calls go
//! through the real on-chain WASM with the real fee schedules — no
//! external curve assumptions, no fabricated reserves.
//!
//! ```sh
//! cargo run --release --example cross_dex_arbitrage
//! ```

use soroban_fork::ForkConfig;
use soroban_sdk::{Address, Env, IntoVal, String as SorobanString, Symbol};

const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const PHOENIX_XLM_USDC: &str = "CBHCRSVX3ZZ7EGTSYMKPEFGZNWRVCSESQR3UABET4MIW52N4EVU6BIZX";
const SOROSWAP_ROUTER: &str = "CAG5LRYQ5JVEUI5TEID72EYOVX44TTUJT5BQR2J6J77FH65PCCFAJDDH";
const UNIT: i128 = 10_000_000;

// Phoenix's pool::simulate_swap return shape, mirrored here so the
// SDK can decode the response. `#[contracttype]` serializes fields
// alphabetically by name on the XDR wire — keep this order.
#[soroban_sdk::contracttype]
#[derive(Clone)]
struct SimulateSwapResponse {
    ask_amount: i128,
    commission_amount: i128,
    spread_amount: i128,
    total_return: i128,
}

fn addr(env: &Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

fn fmt(raw: i128) -> String {
    let whole = raw / UNIT;
    let frac = (raw % UNIT).unsigned_abs();
    format!("{whole}.{frac:07}")
}

/// Phoenix XYK pool's `simulate_swap` — runs the same math `swap()`
/// would, including the configured commission, without mutating state.
/// Returns the post-fee USDC out for an XLM-in offer.
fn phoenix_quote(env: &Env, pool: &Address, offer: &Address, amount: i128) -> SimulateSwapResponse {
    env.invoke_contract(
        pool,
        &Symbol::new(env, "simulate_swap"),
        soroban_sdk::vec![env, offer.into_val(env), amount.into_val(env)],
    )
}

/// Soroswap router's chained `get_amounts_out`. The last element of
/// the returned vector is the output amount after walking the path
/// — for `[xlm, usdc]` that's USDC out, post-fee.
fn soroswap_quote(
    env: &Env,
    router: &Address,
    path: soroban_sdk::Vec<Address>,
    amount: i128,
) -> i128 {
    let amounts: soroban_sdk::Vec<i128> = env.invoke_contract(
        router,
        &Symbol::new(env, "router_get_amounts_out"),
        soroban_sdk::vec![env, amount.into_val(env), path.into_val(env)],
    );
    amounts.last().unwrap_or(0)
}

fn main() {
    let rpc = std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".into());

    eprintln!("Forking Stellar mainnet from {rpc} ...");
    let env = ForkConfig::new(&rpc).build().expect("fork build");

    let e: &Env = env.env();
    let xlm = addr(e, XLM_SAC);
    let usdc = addr(e, USDC_SAC);
    let phoenix = addr(e, PHOENIX_XLM_USDC);
    let soroswap = addr(e, SOROSWAP_ROUTER);
    let path = soroban_sdk::vec![e, xlm.clone(), usdc.clone()];

    eprintln!();
    eprintln!("=== Phoenix vs Soroswap — XLM -> USDC ===");
    eprintln!("Forked at ledger:  {}", env.ledger_sequence());
    eprintln!();
    eprintln!(
        "{:<14}  {:>17}  {:>17}  {:>9}",
        "Sell size", "Phoenix USDC out", "Soroswap USDC out", "Gap"
    );

    for size in [1_000_i128, 10_000, 100_000] {
        let dx = size * UNIT;
        let phoenix_resp = phoenix_quote(e, &phoenix, &xlm, dx);
        let soroswap_out = soroswap_quote(e, &soroswap, path.clone(), dx);
        let phoenix_out = phoenix_resp.ask_amount;

        // Gap as a percentage of the cheaper venue's output. Positive
        // sign means Phoenix is paying more; negative means Soroswap
        // is. Either way, the magnitude is what an arb-aware trader
        // cares about.
        let gap_pct = if soroswap_out > 0 && phoenix_out > 0 {
            let cheaper = phoenix_out.min(soroswap_out) as f64;
            (phoenix_out - soroswap_out) as f64 / cheaper * 100.0
        } else {
            0.0
        };

        eprintln!(
            "{:<14}  {:>17}  {:>17}  {:>+8.3}%",
            format!("{} XLM", size),
            fmt(phoenix_out),
            fmt(soroswap_out),
            gap_pct
        );
    }

    eprintln!();
    eprintln!("Positive gap → Phoenix pays more (sell on Phoenix, buy on Soroswap).");
    eprintln!("Negative gap → Soroswap pays more (sell on Soroswap, buy on Phoenix).");
    eprintln!("Sub-percent gaps are normal market-making noise; multi-percent gaps");
    eprintln!("are real cross-DEX arbitrage opportunities at this trade size.");
    eprintln!();
    eprintln!("RPC fetches:       {}", env.fetch_count());
}
