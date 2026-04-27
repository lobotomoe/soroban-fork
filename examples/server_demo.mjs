#!/usr/bin/env node
// soroban-fork — zero-dependency Node demo against the JSON-RPC server.
//
// Demonstrates that a forked mainnet served by `soroban-fork serve`
// speaks the standard Stellar Soroban RPC dialect, so any tooling that
// already speaks that protocol (`@stellar/stellar-sdk`, Stellar Lab,
// Freighter, custom clients) can point at it transparently.
//
// Two-shell workflow:
//
//   shell A) cargo run --release --features server -- serve \
//              --rpc https://soroban-rpc.mainnet.stellar.gateway.fm
//
//   shell B) node examples/server_demo.mjs
//
// Override the local server URL via SOROBAN_FORK_URL. Requires Node 18+
// (uses the global `fetch`).
//
// This demo intentionally only calls methods whose parameters do not
// require XDR encoding. Once you need `simulateTransaction` or
// `getLedgerEntries`, use `@stellar/stellar-sdk` so its Server class
// builds the envelopes for you — the same code that targets mainnet
// will work against this fork unchanged.

const SERVER_URL = process.env.SOROBAN_FORK_URL ?? "http://127.0.0.1:8000";

let nextId = 1;

async function rpc(method, params = null) {
  const envelope = { jsonrpc: "2.0", id: nextId++, method };
  if (params !== null) envelope.params = params;

  const resp = await fetch(SERVER_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(envelope),
  });

  if (!resp.ok) {
    throw new Error(`HTTP ${resp.status} ${resp.statusText}`);
  }
  const body = await resp.json();
  if (body.error) {
    throw new Error(`RPC ${body.error.code}: ${body.error.message}`);
  }
  return body.result;
}

function section(title) {
  console.log();
  console.log(`=== ${title} ===`);
}

async function main() {
  console.log(`soroban-fork JSON-RPC demo — pointing at ${SERVER_URL}`);

  section("getHealth");
  const health = await rpc("getHealth");
  console.log(`  status:        ${health.status}`);
  console.log(`  latestLedger:  ${health.latestLedger}`);

  section("getVersionInfo");
  const version = await rpc("getVersionInfo");
  console.log(`  version:           ${version.version}`);
  console.log(`  protocolVersion:   ${version.protocolVersion}`);

  section("getNetwork");
  const network = await rpc("getNetwork");
  console.log(`  passphrase:        ${network.passphrase}`);
  console.log(`  networkId:         ${network.networkId}`);
  console.log(`  protocolVersion:   ${network.protocolVersion}`);
  if (network.passphrase === "Public Global Stellar Network ; September 2015") {
    console.log("  ^ this is the live mainnet passphrase, served from a local fork.");
  }

  section("getLatestLedger");
  const latest = await rpc("getLatestLedger");
  console.log(`  sequence:          ${latest.sequence}`);
  console.log(`  protocolVersion:   ${latest.protocolVersion}`);

  section("Next steps");
  console.log("  For getLedgerEntries / simulateTransaction, plug @stellar/stellar-sdk's");
  console.log("  `new SorobanRpc.Server('http://127.0.0.1:8000')` and call its methods —");
  console.log("  they speak the same dialect this server implements.");
}

main().catch((err) => {
  console.error(`\nDemo failed: ${err.message}`);
  console.error(`Is the server running? Try:`);
  console.error(`  cargo run --release --features server -- serve --rpc <mainnet-rpc-url>`);
  process.exit(1);
});
