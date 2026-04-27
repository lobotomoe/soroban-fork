//! "If I deposit 50,000 USDC into Blend's Fixed pool, what does the pool
//! look like afterward, and what's my share of total supply?"
//!
//! Without soroban-fork: deploy a Blend clone on testnet, mock interest
//! curves, fabricate realistic reserves. Your numbers do not match
//! production. With soroban-fork: real Blend WASM, real reserves on real
//! mainnet state — locally, in seconds, deterministically.
//!
//! ```sh
//! cargo run --release --example blend_lending
//! ```
//!
//! Override the upstream RPC by setting `MAINNET_RPC_URL`.

use soroban_fork::ForkConfig;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env, IntoVal, String as SorobanString, Symbol, Val};

const USDC_SAC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
const BLEND_V1_FIXED_POOL: &str = "CDVQVKOY2YSXS2IC7KN6MNASSHPAO7UN2UR2ON4OI2SKMFJNVAMDX6DP";
const UNIT: i128 = 10_000_000;

// `#[contracttype]` serializes fields alphabetically by name on the
// XDR wire — reordering would silently mis-bind to Blend's pool::submit
// signature. Keep address, amount, request_type in this order.
#[soroban_sdk::contracttype]
#[derive(Clone)]
struct Request {
    address: Address,
    amount: i128,
    request_type: u32,
}
const SUPPLY_COLLATERAL: u32 = 0;

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

fn main() {
    let rpc = std::env::var("MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://soroban-rpc.mainnet.stellar.gateway.fm".into());

    eprintln!("Forking Stellar mainnet from {rpc} ...");
    let env = ForkConfig::new(&rpc).build().expect("fork build");
    env.mock_all_auths();

    let e: &Env = env.env();
    let usdc = addr(e, USDC_SAC);
    let pool = addr(e, BLEND_V1_FIXED_POOL);
    let alice = Address::generate(e);

    let pool_usdc_before = balance(e, &usdc, &pool);
    eprintln!();
    eprintln!("=== Blend V1 Fixed pool — live mainnet state ===");
    eprintln!("Forked at ledger:  {}", env.ledger_sequence());
    eprintln!("Pool USDC supply:  {} USDC", fmt(pool_usdc_before));

    let deposit = 50_000 * UNIT;
    env.deal_token(&usdc, &alice, deposit);

    let mut requests = soroban_sdk::Vec::new(e);
    requests.push_back(Request {
        request_type: SUPPLY_COLLATERAL,
        address: usdc.clone(),
        amount: deposit,
    });
    let user_val: Val = alice.into_val(e);
    let req_val: Val = requests.into_val(e);
    env.invoke_contract::<Val>(
        &pool,
        &Symbol::new(e, "submit"),
        soroban_sdk::vec![e, user_val, user_val, user_val, req_val],
    );

    let pool_usdc_after = balance(e, &usdc, &pool);
    let alice_remaining = balance(e, &usdc, &alice);
    let delta = pool_usdc_after - pool_usdc_before;
    let share_pct = if pool_usdc_after > 0 {
        deposit as f64 * 100.0 / pool_usdc_after as f64
    } else {
        0.0
    };

    eprintln!();
    eprintln!("=== After Alice deposits {} USDC ===", fmt(deposit));
    eprintln!(
        "Pool USDC supply:  {} USDC  (Δ +{})",
        fmt(pool_usdc_after),
        fmt(delta)
    );
    eprintln!("Alice wallet:      {} USDC", fmt(alice_remaining));
    eprintln!("Alice share:       {share_pct:.2}% of total supply");
    eprintln!();
    eprintln!("RPC fetches:       {}", env.fetch_count());
    eprintln!("Real mainnet pool unchanged.");
}
