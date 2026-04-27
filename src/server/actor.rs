//! Single-threaded actor that owns the `ForkedEnv` and processes RPC
//! commands serially.
//!
//! # Why an actor
//!
//! `soroban_sdk::Env` is `!Send` — its internal `Host = Rc<HostImpl>`
//! pins it to a single thread. We can't share it via `Arc<RwLock<Env>>`,
//! and rebuilding it per-request would invalidate any state the user
//! has accumulated (deal_token, mock_all_auths, future cheatcodes).
//!
//! The fix is the actor pattern: HTTP handlers (running on a multi-thread
//! tokio runtime) send `Command`s through a `tokio::sync::mpsc` channel
//! to one OS thread that owns the `ForkedEnv`. The worker processes
//! commands serially in a blocking loop; each command carries a
//! `oneshot::Sender` for the reply, so handlers `await` on the receiver
//! and the OS thread is freed while the worker computes.
//!
//! # Trade-offs
//!
//! - **Throughput is bounded by single-threaded execution** of contract
//!   calls. Soroban contract execution is μs–ms range, and we're a test
//!   harness, so this is fine. Foundry's Anvil makes the same trade-off.
//! - **A panicking handler kills the worker thread**, dropping the
//!   `Receiver` and breaking the whole server. v0.5 documents this; a
//!   future version may add panic recovery via `catch_unwind` in the
//!   worker loop.
//! - **Cache misses on `getLedgerEntries` block all other requests** for
//!   the duration of the upstream RPC call. Steady-state cache hits are
//!   instant; first-touch latency is one RPC round-trip per uncached key.

use std::rc::Rc;
use std::thread;

use log::{info, warn};
use soroban_env_host::budget::Budget;
use soroban_env_host::e2e_invoke::{
    invoke_host_function_in_recording_mode, RecordingInvocationAuthMode,
};
use soroban_env_host::storage::SnapshotSource;
use soroban_env_host::xdr::{
    AccountId, ContractEvent, DiagnosticEvent, HostFunction, LedgerEntry, LedgerKey, ScVal,
    SorobanAuthorizationEntry, SorobanResources,
};
use tokio::sync::{mpsc, oneshot};

use crate::{ForkConfig, ForkError};

/// A unit of work the worker can execute against `ForkedEnv`.
///
/// Each variant carries its inputs and a `oneshot::Sender` for the
/// reply. Using a sum type (rather than `Box<dyn FnOnce>`) keeps the
/// command surface explicit and `Debug`-printable for tracing.
#[derive(Debug)]
pub(crate) enum Command {
    /// Snapshot of network metadata captured at fork-build time. No
    /// upstream call.
    GetNetwork {
        reply: oneshot::Sender<NetworkReply>,
    },
    /// The forked Env's current ledger info. No upstream call.
    GetLatestLedger {
        reply: oneshot::Sender<LatestLedgerReply>,
    },
    /// The fork-point ledger as a single-element Stellar `getLedgers`
    /// page. The fork is a frozen snapshot — there's exactly one ledger
    /// of state, regardless of what the caller's `start_ledger` was, so
    /// the request's `start_ledger` is intentionally not threaded
    /// through; we always answer with our own sequence.
    GetLedgersPage {
        reply: oneshot::Sender<LedgersPageReply>,
    },
    /// Resolve a batch of `LedgerKey`s through the snapshot source —
    /// cache hits are instant; misses trigger upstream RPC fetches that
    /// block the worker for the round-trip.
    GetLedgerEntries {
        keys: Vec<LedgerKey>,
        reply: oneshot::Sender<LedgerEntriesReply>,
    },
    /// Run a host function in recording mode and return everything the
    /// host observed: result, auth requirements, footprint, events,
    /// budget consumption. Does **not** mutate the env's state (the
    /// host primitive constructs its own throwaway sandbox per call).
    ///
    /// `transaction_size_bytes` is the on-the-wire length of the
    /// `TransactionEnvelope` the handler decoded. The worker needs it
    /// to compute the bandwidth + historical-data components of
    /// `minResourceFee`; threading it as a `Command` field keeps fee
    /// math centralised in the worker (where the live fee schedule
    /// lives) rather than splitting it across handler and worker.
    SimulateTransaction {
        host_function: HostFunction,
        source_account: AccountId,
        transaction_size_bytes: u32,
        reply: oneshot::Sender<SimulationReply>,
    },
}

#[derive(Debug)]
pub(crate) struct NetworkReply {
    /// Network passphrase, or a synthesised label when the user
    /// overrode `network_id` and the original passphrase is unknown.
    pub(crate) passphrase: String,
    pub(crate) protocol_version: u32,
    /// Hex-encoded SHA-256 of the passphrase (a.k.a. network ID).
    pub(crate) network_id_hex: String,
}

#[derive(Debug)]
pub(crate) struct LatestLedgerReply {
    pub(crate) sequence: u32,
    pub(crate) protocol_version: u32,
    /// Synthesised stable identifier for the fork-point ledger. The
    /// real RPC returns a 32-byte ledger hash; we don't have one (we
    /// forked from a snapshot, not a Stellar ledger), so we generate
    /// a deterministic label from the sequence.
    pub(crate) id: String,
}

#[derive(Debug)]
pub(crate) struct LedgersPageReply {
    pub(crate) sequence: u32,
    pub(crate) close_time: u64,
}

#[derive(Debug)]
pub(crate) struct LedgerEntriesReply {
    /// Per-key result, in the same order the keys were given. `None`
    /// means the key is absent from the ledger (and we asked the
    /// upstream RPC to confirm); `Some` carries the entry plus its
    /// optional live-until-ledger TTL hint.
    pub(crate) entries: Vec<Option<(LedgerKey, LedgerEntry, Option<u32>)>>,
    /// Sequence number reported as `latestLedger` on the wire — the
    /// fork's reported ledger.
    pub(crate) latest_ledger: u32,
}

/// Recording-mode simulation outcome. We avoid carrying `HostError`
/// across the channel (it isn't `Send` in all circumstances and
/// stringifying loses no useful client-facing information) so we map
/// the failure case to a human-readable message at the worker boundary.
#[derive(Debug)]
pub(crate) struct SimulationReply {
    /// `Ok(scval)` on simulation success, `Err(message)` when the host
    /// reported an error during invocation. The wire response carries
    /// the message in the top-level `error` field.
    pub(crate) result: std::result::Result<ScVal, String>,
    /// Recorded auth entries that a real `sendTransaction` would need
    /// to be signed with.
    pub(crate) auth: Vec<SorobanAuthorizationEntry>,
    /// Footprint (read+write keys) and resource accounting (instructions,
    /// disk-read/write bytes). Becomes `transactionData.resources`.
    pub(crate) resources: SorobanResources,
    /// Contract-emitted events.
    pub(crate) contract_events: Vec<ContractEvent>,
    /// Diagnostic events captured if tracing was on (fn_call/fn_return
    /// pairs). Empty otherwise.
    pub(crate) diagnostic_events: Vec<DiagnosticEvent>,
    /// Echoed `latestLedger` for the response wire shape.
    pub(crate) latest_ledger: u32,
    /// Resource fee a real `sendTransaction` would owe at the live
    /// network's fee schedule, summed from non-refundable and
    /// refundable components. `None` when the schedule could not be
    /// resolved — the wire response then omits `minResourceFee`
    /// rather than lying with `"0"`.
    pub(crate) min_resource_fee: Option<i64>,
    /// Real memory in bytes the host's budget metered during the
    /// invocation, queried via `Budget::get_mem_bytes_consumed`.
    /// `None` only on the recording-mode failure path before any
    /// metering happened.
    pub(crate) mem_bytes: Option<u64>,
}

/// Handle to the worker thread. Cloning is cheap (Arc-style internally
/// in `mpsc::Sender`) so handlers can clone freely.
#[derive(Clone)]
pub(crate) struct ActorHandle {
    tx: mpsc::Sender<Command>,
}

impl ActorHandle {
    /// Send a command and `await` the reply.
    ///
    /// Two failure modes, both surfaced as `internal_error` to clients:
    /// - The send queue is full (worker too slow). Tokio's bounded
    ///   channel applies backpressure here; we don't want unbounded
    ///   queueing under load.
    /// - The worker has died (channel closed). The server is in an
    ///   unrecoverable state at that point.
    pub(crate) async fn send<R>(
        &self,
        build: impl FnOnce(oneshot::Sender<R>) -> Command,
    ) -> Result<R, ActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = build(reply_tx);
        self.tx
            .send(cmd)
            .await
            .map_err(|_| ActorError::WorkerGone)?;
        reply_rx.await.map_err(|_| ActorError::WorkerGone)
    }
}

/// Failure modes when communicating with the worker.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ActorError {
    /// Worker thread is no longer running. Either it panicked or the
    /// server is shutting down. Either way, the only correct behaviour
    /// is to fail subsequent requests fast.
    #[error("worker thread is no longer running")]
    WorkerGone,
}

/// Spawn the worker thread and return a handle that the HTTP layer can
/// use to enqueue commands.
///
/// **The Env is built inside the worker thread, never crossing a thread
/// boundary.** That's the load-bearing constraint of this whole module:
/// `Env` contains `Rc<HostImpl>` which is `!Send`, so any attempt to
/// pass an already-built `ForkedEnv` into `thread::spawn` fails to
/// compile. The trade-off is that build errors surface asynchronously
/// through `ready_rx` — callers must `.await` it before serving.
pub(crate) fn spawn(
    config: ForkConfig,
) -> (
    ActorHandle,
    oneshot::Receiver<std::result::Result<(), ForkError>>,
) {
    // 32 = a small bounded queue. If the worker can't keep up, handlers
    // back-pressure (their `.send().await` waits) rather than spinning
    // up unbounded RAM. For a test-fork server, 32 in-flight requests
    // is comfortably more than any realistic test suite generates.
    let (tx, rx) = mpsc::channel(32);
    let (ready_tx, ready_rx) = oneshot::channel();

    // `std::thread::spawn` (not `tokio::task::spawn_blocking`) because
    // we need a long-lived OS thread that *owns* the !Send Env, not a
    // pool worker that might migrate between calls.
    thread::Builder::new()
        .name("soroban-fork-worker".into())
        .spawn(move || {
            let env = match config.build() {
                Ok(env) => {
                    let _ = ready_tx.send(Ok(()));
                    env
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            worker_loop(env, rx);
        })
        .expect("spawn soroban-fork-worker thread");

    (ActorHandle { tx }, ready_rx)
}

/// Main loop. Pulls commands from the channel, dispatches, sends
/// replies. Exits when the channel closes (all senders dropped =
/// server shutting down).
fn worker_loop(env: crate::ForkedEnv, mut rx: mpsc::Receiver<Command>) {
    info!("soroban-fork: worker thread started");
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Command::GetNetwork { reply } => {
                let passphrase = env
                    .passphrase()
                    // Passphrase is missing only when the user passed an
                    // explicit `network_id` override. We synthesise a
                    // label so the wire shape stays valid; a real client
                    // that needs the exact passphrase string should not
                    // override `network_id` in the first place.
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Forked Soroban Network (custom network_id)".to_string());
                let _ = reply.send(NetworkReply {
                    passphrase,
                    protocol_version: env.protocol_version(),
                    network_id_hex: hex_encode(&env.network_id()),
                });
            }
            Command::GetLatestLedger { reply } => {
                let _ = reply.send(LatestLedgerReply {
                    sequence: env.ledger_sequence(),
                    protocol_version: env.protocol_version(),
                    id: format!("forked-ledger-{}", env.ledger_sequence()),
                });
            }
            Command::GetLedgersPage { reply } => {
                // We have one ledger of state; clients may ask for any
                // `start_ledger` but we always answer with the fork
                // point. The wire shape stays valid either way.
                let _ = reply.send(LedgersPageReply {
                    sequence: env.ledger_sequence(),
                    close_time: env.ledger_close_time(),
                });
            }
            Command::GetLedgerEntries { keys, reply } => {
                let entries = resolve_ledger_entries(&env, keys);
                let _ = reply.send(LedgerEntriesReply {
                    entries,
                    latest_ledger: env.ledger_sequence(),
                });
            }
            Command::SimulateTransaction {
                host_function,
                source_account,
                transaction_size_bytes,
                reply,
            } => {
                let _ = reply.send(simulate_transaction(
                    &env,
                    host_function,
                    source_account,
                    transaction_size_bytes,
                ));
            }
        }
    }
    warn!("soroban-fork: worker thread shutting down (channel closed)");
    drop(env);
    info!("soroban-fork: worker thread exited");
}

/// Run `invoke_host_function_in_recording_mode` against the forked
/// snapshot source and translate the result into a `SimulationReply`.
///
/// **No state mutation.** The host primitive builds its own throwaway
/// sandbox internally — calling this method twice from the same env
/// is idempotent and side-effect-free. That's exactly what
/// `simulateTransaction` should do.
fn simulate_transaction(
    env: &crate::ForkedEnv,
    host_function: HostFunction,
    source_account: AccountId,
    transaction_size_bytes: u32,
) -> SimulationReply {
    use soroban_sdk::testutils::Ledger as _;

    let snapshot_source: Rc<dyn SnapshotSource> = env.snapshot_source().clone();
    let ledger_info = env.env().ledger().get();
    let budget = Budget::default();

    // `Recording(false)` = "track auth as the contract calls
    // require_auth(...) and report the entries needed". `false` allows
    // non-root authorizations (per the host's terminology). Callers
    // who want to enforce specific signed entries can use
    // `sendTransaction` in v0.6 with explicit auth.
    let auth_mode = RecordingInvocationAuthMode::Recording(false);

    let mut diagnostic_events: Vec<DiagnosticEvent> = Vec::new();
    let result = invoke_host_function_in_recording_mode(
        &budget,
        true, // enable_diagnostics — captures fn_call/fn_return events
        &host_function,
        &source_account,
        auth_mode,
        ledger_info,
        snapshot_source,
        [0u8; 32], // base_prng_seed — deterministic for reproducible simulations
        &mut diagnostic_events,
    );

    let latest_ledger = env.ledger_sequence();

    // Memory accounting reads from the same Budget that was just charged
    // by the host. On the failure path the budget may not have been
    // exercised; treat that as `None` rather than a misleading 0.
    let mem_bytes = budget.get_mem_bytes_consumed().ok();

    match result {
        Ok(rec) => {
            let invoke_result = rec.invoke_result.map_err(|e| format!("host error: {e}"));
            let min_resource_fee = compute_min_resource_fee(
                env,
                &rec.resources,
                &rec.contract_events,
                transaction_size_bytes,
            );
            SimulationReply {
                result: invoke_result,
                auth: rec.auth,
                resources: rec.resources,
                contract_events: rec.contract_events,
                diagnostic_events,
                latest_ledger,
                min_resource_fee,
                mem_bytes,
            }
        }
        Err(e) => {
            // Recording-mode-level error (budget exhaustion). The wire
            // response sets `error` and elides everything else.
            SimulationReply {
                result: Err(format!("recording-mode error: {e}")),
                auth: Vec::new(),
                resources: empty_soroban_resources(),
                contract_events: Vec::new(),
                diagnostic_events,
                latest_ledger,
                min_resource_fee: None,
                mem_bytes,
            }
        }
    }
}

/// Compute the minimum resource fee a real `sendTransaction` would owe
/// for this simulation, using the on-chain fee schedule.
///
/// Returns `None` when:
/// - Resolving the live fee schedule failed (RPC error, missing config
///   setting). The wire response then omits `minResourceFee` rather
///   than lying with `"0"` or a partial computation.
/// - Encoding a contract event for size measurement failed (host
///   internal error; should not happen with well-formed events).
///
/// **Slight underestimate by signature size.** The bandwidth +
/// historical-data fee components scale with the on-the-wire envelope
/// size, but at simulation time the envelope carries no signatures
/// yet (the caller can't sign what they're trying to size). Real
/// `sendTransaction` will pay ~70 bytes × `fee_per_transaction_size_1kb`
/// extra per signer; clients copying this number into a signed tx
/// should pad accordingly. Same approximation `stellar-rpc` makes.
fn compute_min_resource_fee(
    env: &crate::ForkedEnv,
    resources: &SorobanResources,
    contract_events: &[ContractEvent],
    transaction_size_bytes: u32,
) -> Option<i64> {
    use soroban_env_host::xdr::{DiagnosticEvent, Limits, WriteXdr};

    let fee_config = match env.fee_configuration() {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("soroban-fork: minResourceFee skipped — fee schedule unavailable: {e}");
            return None;
        }
    };

    // Stellar core measures contract-events size as the XDR length of
    // the same `DiagnosticEvent { in_successful_contract_call: true,
    // event }` wrappers that go on the wire.
    let mut events_size: u32 = 0;
    for ce in contract_events {
        let de = DiagnosticEvent {
            in_successful_contract_call: true,
            event: ce.clone(),
        };
        match de.to_xdr(Limits::none()) {
            Ok(bytes) => {
                events_size = events_size.saturating_add(bytes.len() as u32);
            }
            Err(e) => {
                warn!("soroban-fork: minResourceFee skipped — contract event XDR encode failed: {e}");
                return None;
            }
        }
    }

    let footprint = &resources.footprint;
    let read_only_count = footprint.read_only.len() as u32;
    let read_write_count = footprint.read_write.len() as u32;

    let tx_resources = crate::fees::TransactionResources {
        instructions: resources.instructions,
        // Stellar fee math counts every entry the tx touches as "read"
        // (writes are read-then-written), and writes again as "write".
        disk_read_entries: read_only_count.saturating_add(read_write_count),
        write_entries: read_write_count,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes: events_size,
        transaction_size_bytes,
    };
    let (non_refundable, refundable) =
        crate::fees::compute_transaction_resource_fee(&tx_resources, fee_config);
    Some(non_refundable.saturating_add(refundable))
}

/// Stand-in resources struct for the failure path. We populate the same
/// shape the success path returns so the response serialiser doesn't
/// need to special-case `None`.
fn empty_soroban_resources() -> SorobanResources {
    use soroban_env_host::xdr::LedgerFootprint;
    SorobanResources {
        footprint: LedgerFootprint {
            read_only: vec![].try_into().expect("empty vec into VecM"),
            read_write: vec![].try_into().expect("empty vec into VecM"),
        },
        instructions: 0,
        disk_read_bytes: 0,
        write_bytes: 0,
    }
}

/// Resolve a batch of keys through the snapshot source. Cache hits are
/// O(BTreeMap lookup + XDR decode), misses are one RPC round-trip per
/// key (the upstream client batches `getLedgerEntries` internally for
/// pre-warming, but on-demand calls go one at a time).
fn resolve_ledger_entries(
    env: &crate::ForkedEnv,
    keys: Vec<LedgerKey>,
) -> Vec<Option<(LedgerKey, LedgerEntry, Option<u32>)>> {
    let source = env.snapshot_source();
    keys.into_iter()
        .map(|key| {
            let key_rc = Rc::new(key.clone());
            match source.get(&key_rc) {
                Ok(Some((entry_rc, live_until))) => {
                    Some((key, entry_rc.as_ref().clone(), live_until))
                }
                Ok(None) => None,
                // SnapshotSource::get's HostError is theoretical here —
                // our impl never produces one (Strict mode panics, Lenient
                // returns None). If that contract changes, we surface as
                // "missing" rather than crashing the worker; the caller
                // sees a partial response.
                Err(_) => None,
            }
        })
        .collect()
}

/// Lower-case hex encoder for network IDs and similar 32-byte values.
/// Inline to avoid pulling in a hex-crate dep just for a few uses.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
