//! "If I market-sell 1,000,000 XLM into Phoenix's XLM/USDC pool right
//! now, what's my actual fill price after slippage?"
//!
//! Without soroban-fork: pick reserve numbers that look "reasonable" —
//! tests pass, production loses 4% to slippage you didn't model.
//! With soroban-fork: read the actual reserves the pool holds at this
//! ledger, and compute the constant-product curve against them.
//!
//! Output is pre-fee AMM math (curve-only, no swap fees) — the goal is
//! to interpret the slippage curve in isolation. Phoenix's `swap()`
//! function applies its own fee on top.
//!
//! ```sh
//! cargo run --release --example phoenix_slippage
//! ```

use soroban_fork::ForkConfig;
use soroban_sdk::{Address, Env, IntoVal, String as SorobanString, Symbol, Val};

const XLM_SAC: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const PHOENIX_XLM_USDC: &str = "CBHCRSVX3ZZ7EGTSYMKPEFGZNWRVCSESQR3UABET4MIW52N4EVU6BIZX";
const UNIT: i128 = 10_000_000;

fn addr(env: &Env, id: &str) -> Address {
    Address::from_string(&SorobanString::from_str(env, id))
}

fn balance(env: &Env, token: &Address, who: &Address) -> i128 {
    let v: Val = who.into_val(env);
    env.invoke_contract(
        token,
        &Symbol::new(env, "balance"),
        soroban_sdk::vec![env, v],
    )
}

fn fmt(raw: i128) -> String {
    let whole = raw / UNIT;
    let frac = (raw % UNIT).unsigned_abs();
    format!("{whole}.{frac:07}")
}

/// Constant-product output: out = R_out * dx / (R_in + dx).
fn cp_out(reserve_in: i128, reserve_out: i128, dx: i128) -> i128 {
    reserve_out
        .checked_mul(dx)
        .expect("reserve_out * dx overflow")
        / (reserve_in + dx)
}

fn main() {
    let rpc = std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".into());

    eprintln!("Forking Stellar mainnet from {rpc} ...");
    let env = ForkConfig::new(&rpc).build().expect("fork build");

    let e: &Env = env.env();
    let xlm = addr(e, XLM_SAC);
    let usdc = addr(e, USDC_SAC);
    let pool = addr(e, PHOENIX_XLM_USDC);

    let xlm_reserve = balance(e, &xlm, &pool);
    let usdc_reserve = balance(e, &usdc, &pool);
    if xlm_reserve == 0 || usdc_reserve == 0 {
        eprintln!("Pool reserves are empty — pool may have been migrated. Aborting.");
        return;
    }
    let spot = usdc_reserve as f64 / xlm_reserve as f64;

    eprintln!();
    eprintln!("=== Phoenix XLM/USDC — live mainnet reserves ===");
    eprintln!("Forked at ledger:  {}", env.ledger_sequence());
    eprintln!("Pool XLM:          {}", fmt(xlm_reserve));
    eprintln!("Pool USDC:         {}", fmt(usdc_reserve));
    eprintln!("Spot price:        ${spot:.6} per XLM");
    eprintln!();
    eprintln!("=== Slippage on XLM -> USDC market sells ===");
    eprintln!(
        "{:<14}  {:>17}  {:>11}  {:>10}",
        "Sell size", "USDC out", "Avg price", "vs spot"
    );

    for size in [1_000_i128, 10_000, 100_000, 500_000, 1_000_000] {
        let dx = size * UNIT;
        let dy = cp_out(xlm_reserve, usdc_reserve, dx);
        let avg = dy as f64 / dx as f64;
        let vs_spot = (avg - spot) / spot * 100.0;
        eprintln!(
            "{:<14}  {:>17}  ${:>9.6}  {:>+9.2}%",
            format!("{} XLM", size),
            fmt(dy),
            avg,
            vs_spot
        );
    }
    eprintln!();
    eprintln!("RPC fetches:       {}", env.fetch_count());
    eprintln!("Numbers reflect on-chain reserves at fork time.");
}
