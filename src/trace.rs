//! Call-tree tracing for forked environments.
//!
//! Soroban's host emits **diagnostic events** — `fn_call` / `fn_return`
//! pairs that describe every cross-contract invocation while running in
//! [`DiagnosticLevel::Debug`](soroban_env_host::DiagnosticLevel). This
//! module reads that event stream and reconstructs a hierarchical
//! [`Trace`] suitable for both programmatic assertions and Foundry-style
//! `-vvvv` text output.
//!
//! # Enabling
//!
//! Tracing must be enabled **before** the first contract call:
//!
//! ```rust,no_run
//! use soroban_fork::ForkConfig;
//!
//! let env = ForkConfig::new("https://soroban-testnet.stellar.org:443")
//!     .tracing(true)
//!     .build()
//!     .expect("fork");
//!
//! // ... env.invoke_contract::<i128>(&vault, &symbol, args) ...
//!
//! eprintln!("{}", env.trace());
//! ```
//!
//! # Wire format the parser relies on
//!
//! From `soroban-env-host` 25.x ([`fn_call_diagnostics`][src1] and
//! [`fn_return_diagnostics`][src2]):
//!
//! - **`fn_call`** — `topics = [Symbol("fn_call"), Bytes(callee_id),
//!   Symbol(fn_name)]`, `data` is:
//!   - `ScVal::Void` if the function takes 0 args
//!   - the single arg directly if it takes 1 arg
//!   - `ScVal::Vec([args...])` if it takes 2+ args
//!   - `event.contract_id` is the **calling** contract (`None` if the
//!     call originated from the test harness).
//!
//! - **`fn_return`** — `topics = [Symbol("fn_return"), Symbol(fn_name)]`,
//!   `data` is the return value as a single `ScVal`. `event.contract_id`
//!   is the **returning** contract.
//!
//! [src1]: https://github.com/stellar/rs-soroban-env/blob/v25.0.1/soroban-env-host/src/events/diagnostic.rs
//! [src2]: https://github.com/stellar/rs-soroban-env/blob/v25.0.1/soroban-env-host/src/events/diagnostic.rs
//!
//! # Caveats
//!
//! 1. **Single-`Vec`-arg ambiguity.** A call with one argument of type
//!    `Vec<T>` is wire-indistinguishable from a call with `n` args that
//!    the host packed into `ScVal::Vec`. We always treat `ScVal::Vec`
//!    payloads as multi-arg in [`TraceFrame::args`]. This matches the
//!    EVM/Foundry analogue where calldata is a flat byte vector and
//!    Foundry's `-vvvv` cannot recover the true arity either.
//!
//! 2. **Per-invocation scoping.** The host's `InvocationMeter` calls
//!    `events.clear()` at the start of every top-level `invoke_contract`
//!    (see [`push_invocation`][meter] in `soroban-env-host`). Each
//!    [`Trace`] therefore reflects only the **most recent** top-level
//!    invocation; events from prior calls are gone. This is perfect for
//!    per-test scoping but means you cannot accumulate trace history
//!    across calls — capture the trace yourself if you need history.
//!
//!    [meter]: https://github.com/stellar/rs-soroban-env/blob/v25.0.1/soroban-env-host/src/host/invocation_metering.rs
//!
//! 3. **Trapped frames.** A WASM trap, host panic, or budget exhaustion
//!    produces a `fn_call` with no matching `fn_return`. Those frames
//!    render as [`TraceResult::Trapped`].
//!
//! 4. **Rolled-back frames.** When a call fails, the host retroactively
//!    marks all of its events (including any children's `fn_return`s) as
//!    `failed_call=true`. Such frames still report the actual return
//!    value via [`TraceResult::Returned`] but have
//!    [`TraceFrame::rolled_back`] set so callers know the state changes
//!    were undone.

use std::fmt;

use log::warn;
use soroban_env_host::events::Events;
use soroban_env_host::xdr::{ContractEventBody, ContractEventType, ScMapEntry, ScVal};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A complete call tree captured from a `ForkedEnv`'s diagnostic events.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// Top-level invocations, in chronological order. Each root is a
    /// call originating from the test harness (no calling contract).
    pub roots: Vec<TraceFrame>,
}

/// One frame in the call tree.
#[derive(Debug, Clone)]
pub struct TraceFrame {
    /// The contract being called.
    pub contract: stellar_strkey::Contract,
    /// The function name being invoked.
    pub function: String,
    /// Decoded arguments. See module-level docs for the single-`Vec`-arg
    /// ambiguity caveat.
    pub args: Vec<ScVal>,
    /// What happened to this call.
    pub result: TraceResult,
    /// `true` if the host marked this call's events as `failed_call`,
    /// meaning a parent (or this call itself) ultimately failed and the
    /// state changes were rolled back. A frame may both have
    /// `result == Returned(...)` and `rolled_back == true`.
    pub rolled_back: bool,
    /// Nested calls this frame made, in invocation order.
    pub children: Vec<TraceFrame>,
}

/// Outcome of a single frame.
#[derive(Debug, Clone)]
pub enum TraceResult {
    /// The function emitted `fn_return` with this value.
    Returned(ScVal),
    /// The function entered (`fn_call` seen) but never returned. Caused
    /// by a WASM trap, host panic, or budget exhaustion.
    Trapped,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

impl Trace {
    /// Parse the host's diagnostic event stream into a structured trace.
    ///
    /// The event stream is walked once, top to bottom:
    /// - `fn_call`  → push a new frame onto the stack
    /// - `fn_return` → pop the top frame, attach return value, link to parent
    /// - everything else → ignored (e.g. diagnostic `error` events)
    ///
    /// On reaching the end, any frames still on the stack are emitted as
    /// [`TraceResult::Trapped`] — they entered but never returned.
    ///
    /// Malformed streams (orphan `fn_return` without a matching `fn_call`)
    /// are tolerated: the orphan is logged at `warn!` level and skipped.
    pub fn from_events(events: &Events) -> Self {
        let mut stack: Vec<TraceFrame> = Vec::new();
        let mut roots: Vec<TraceFrame> = Vec::new();

        for host_event in &events.0 {
            // We only care about diagnostic events. Contract/system events
            // can also live in this stream depending on the source — skip
            // them rather than mis-parsing.
            if host_event.event.type_ != ContractEventType::Diagnostic {
                continue;
            }

            let ContractEventBody::V0(body) = &host_event.event.body;
            let topics = body.topics.as_slice();
            let kind = topics.first().and_then(scval_as_symbol_str);

            match kind.as_deref() {
                Some("fn_call") => {
                    let Some(frame) = parse_fn_call(topics, &body.data, host_event.failed_call)
                    else {
                        warn!("soroban-fork: malformed fn_call event, skipped");
                        continue;
                    };
                    stack.push(frame);
                }
                Some("fn_return") => {
                    if let Some(mut frame) = stack.pop() {
                        frame.result = TraceResult::Returned(body.data.clone());
                        // The host may mark a fn_return as failed_call when
                        // a parent call later rolls back. Either source —
                        // the open frame's failed_call or this return's —
                        // is enough to flag the rollback.
                        if host_event.failed_call {
                            frame.rolled_back = true;
                        }
                        attach(&mut stack, &mut roots, frame);
                    } else {
                        warn!("soroban-fork: orphan fn_return event with no open frame");
                    }
                }
                _ => continue,
            }
        }

        // Anything left on the stack never received a fn_return. That's a
        // trap. Drain in reverse so children attach before their parents.
        while let Some(mut frame) = stack.pop() {
            frame.result = TraceResult::Trapped;
            // Trapped implies the surrounding state was rolled back.
            frame.rolled_back = true;
            attach(&mut stack, &mut roots, frame);
        }

        Trace { roots }
    }

    /// Total number of frames in the tree (including all nested children).
    pub fn frame_count(&self) -> usize {
        self.roots.iter().map(count_frames).sum()
    }

    /// Whether any frame in the tree was trapped or rolled back.
    pub fn had_failures(&self) -> bool {
        self.roots.iter().any(any_failure)
    }
}

fn count_frames(frame: &TraceFrame) -> usize {
    1 + frame.children.iter().map(count_frames).sum::<usize>()
}

fn any_failure(frame: &TraceFrame) -> bool {
    frame.rolled_back
        || matches!(frame.result, TraceResult::Trapped)
        || frame.children.iter().any(any_failure)
}

fn attach(stack: &mut [TraceFrame], roots: &mut Vec<TraceFrame>, frame: TraceFrame) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(frame);
    } else {
        roots.push(frame);
    }
}

fn parse_fn_call(topics: &[ScVal], data: &ScVal, failed: bool) -> Option<TraceFrame> {
    if topics.len() < 3 {
        return None;
    }
    let contract = scval_as_contract(&topics[1])?;
    let function = scval_as_symbol_str(&topics[2])?;
    let args = unwrap_args_payload(data);
    Some(TraceFrame {
        contract,
        function,
        args,
        result: TraceResult::Trapped, // pessimistic until fn_return arrives
        rolled_back: failed,
        children: Vec::new(),
    })
}

/// Convert the host-encoded data payload back into a flat `Vec<ScVal>`.
///
/// The host packs args according to arity: 0 → `Void`, 1 → arg-as-is,
/// `n>=2` → `ScVal::Vec(args)`. We invert that here. See the
/// single-Vec-arg ambiguity caveat in the module docs.
fn unwrap_args_payload(data: &ScVal) -> Vec<ScVal> {
    match data {
        ScVal::Void => Vec::new(),
        ScVal::Vec(Some(v)) => v.0.iter().cloned().collect(),
        ScVal::Vec(None) => Vec::new(),
        single => vec![single.clone()],
    }
}

fn scval_as_symbol_str(v: &ScVal) -> Option<String> {
    match v {
        ScVal::Symbol(s) => std::str::from_utf8(s.0.as_slice()).ok().map(str::to_string),
        _ => None,
    }
}

fn scval_as_contract(v: &ScVal) -> Option<stellar_strkey::Contract> {
    match v {
        ScVal::Bytes(b) => {
            let arr: [u8; 32] = b.0.as_slice().try_into().ok()?;
            Some(stellar_strkey::Contract(arr))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

const INDENT: &str = "  ";

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.roots.is_empty() {
            return writeln!(
                f,
                "[TRACE] (empty — was tracing(true) set on ForkConfig before invoke?)"
            );
        }
        writeln!(f, "[TRACE]")?;
        for root in &self.roots {
            render_frame(f, root, 1)?;
        }
        Ok(())
    }
}

fn render_frame(f: &mut fmt::Formatter<'_>, frame: &TraceFrame, depth: usize) -> fmt::Result {
    let pad = INDENT.repeat(depth);
    let contract_short = abbreviate_strkey(&frame.contract.to_string());
    write!(f, "{pad}[{contract_short}] {}(", frame.function)?;
    for (i, arg) in frame.args.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        render_scval(f, arg)?;
    }
    write!(f, ")")?;
    if frame.rolled_back {
        write!(f, " [rolled back]")?;
    }
    writeln!(f)?;

    for child in &frame.children {
        render_frame(f, child, depth + 1)?;
    }

    let return_pad = INDENT.repeat(depth + 1);
    match &frame.result {
        TraceResult::Returned(v) => {
            write!(f, "{return_pad}\u{2190} ")?;
            render_scval(f, v)?;
            writeln!(f)
        }
        TraceResult::Trapped => writeln!(f, "{return_pad}\u{2190} TRAPPED (no fn_return)"),
    }
}

/// Best-effort compact rendering for an `ScVal`.
///
/// Intentionally not exhaustive on every variant — exotic ones fall back
/// to `Debug`. The goal is "readable in a forge-style log", not a
/// round-trip-faithful encoder.
///
/// Visible to `crate::auth_tree` so the auth-payload renderer produces
/// arguments in the same shape as the trace renderer.
pub(crate) fn render_scval(f: &mut fmt::Formatter<'_>, v: &ScVal) -> fmt::Result {
    match v {
        ScVal::Void => write!(f, "()"),
        ScVal::Bool(b) => write!(f, "{b}"),
        ScVal::U32(n) => write!(f, "{n}"),
        ScVal::I32(n) => write!(f, "{n}"),
        ScVal::U64(n) => write!(f, "{n}"),
        ScVal::I64(n) => write!(f, "{n}"),
        ScVal::U128(p) => write!(f, "{}", u128_from_pieces(p.hi, p.lo)),
        ScVal::I128(p) => write!(f, "{}", i128_from_pieces(p.hi, p.lo)),
        ScVal::Symbol(s) => match std::str::from_utf8(s.0.as_slice()) {
            Ok(text) => write!(f, "{text}"),
            Err(_) => write!(f, "Symbol(<non-utf8>)"),
        },
        ScVal::String(s) => match std::str::from_utf8(s.0.as_slice()) {
            Ok(text) => write!(f, "\"{text}\""),
            Err(_) => write!(f, "String(<non-utf8>)"),
        },
        ScVal::Bytes(b) => render_bytes(f, b.0.as_slice()),
        ScVal::Address(addr) => render_address(f, addr),
        ScVal::Vec(None) => write!(f, "[]"),
        ScVal::Vec(Some(vec)) => {
            write!(f, "[")?;
            for (i, e) in vec.0.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                render_scval(f, e)?;
            }
            write!(f, "]")
        }
        ScVal::Map(None) => write!(f, "{{}}"),
        ScVal::Map(Some(m)) => render_map(f, m.0.as_slice()),
        // ContractInstance, LedgerKeyContractInstance, LedgerKeyNonce,
        // Timepoint, Duration, U256, I256, Error — rare in user-visible
        // function arguments. Falling back to Debug keeps the trace
        // honest without dragging in every XDR variant.
        other => write!(f, "{other:?}"),
    }
}

fn render_bytes(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    const PREVIEW: usize = 8;
    if bytes.len() <= PREVIEW * 2 {
        write!(f, "0x")?;
        for b in bytes {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    } else {
        write!(f, "0x")?;
        for b in &bytes[..PREVIEW] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "\u{2026}({} bytes)", bytes.len())
    }
}

fn render_map(f: &mut fmt::Formatter<'_>, entries: &[ScMapEntry]) -> fmt::Result {
    write!(f, "{{")?;
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        render_scval(f, &e.key)?;
        write!(f, ": ")?;
        render_scval(f, &e.val)?;
    }
    write!(f, "}}")
}

/// Compact rendering for an `ScAddress`. Visible to `crate::auth_tree` for
/// the same reason as [`render_scval`].
pub(crate) fn render_address(
    f: &mut fmt::Formatter<'_>,
    addr: &soroban_env_host::xdr::ScAddress,
) -> fmt::Result {
    use soroban_env_host::xdr::{PublicKey, ScAddress};
    match addr {
        ScAddress::Account(a) => {
            let PublicKey::PublicKeyTypeEd25519(k) = &a.0;
            let full = stellar_strkey::ed25519::PublicKey(k.0).to_string();
            write!(f, "{}", abbreviate_strkey(&full))
        }
        ScAddress::Contract(h) => {
            let full = stellar_strkey::Contract(h.0 .0).to_string();
            write!(f, "{}", abbreviate_strkey(&full))
        }
        // Muxed/ClaimableBalance/LiquidityPool: rare in test traces,
        // fall back to Debug to stay informative without re-implementing
        // every strkey encoder.
        other => write!(f, "{other:?}"),
    }
}

fn abbreviate_strkey(s: &str) -> String {
    if s.len() > 12 {
        format!("{}\u{2026}{}", &s[..4], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

// `Parts`-typed integer reconstruction. Vendored to avoid pulling in
// soroban-env-host's private `num` module.
fn u128_from_pieces(hi: u64, lo: u64) -> u128 {
    ((hi as u128) << 64) | (lo as u128)
}

fn i128_from_pieces(hi: i64, lo: u64) -> i128 {
    // Reconstruct as 128-bit two's complement: shift the signed high half
    // into the upper 64 bits and OR in the unsigned low half.
    ((hi as i128) << 64) | (lo as i128)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host::events::HostEvent;
    use soroban_env_host::xdr::{
        ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, ContractId,
        ExtensionPoint, Hash, Int128Parts, ScBytes, ScSymbol, ScVal, ScVec, VecM,
    };

    fn sym(s: &str) -> ScVal {
        ScVal::Symbol(ScSymbol(s.try_into().expect("symbol fits in 32 bytes")))
    }

    fn i128_val(v: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (v >> 64) as i64,
            lo: v as u64,
        })
    }

    fn bytes32(b: [u8; 32]) -> ScVal {
        ScVal::Bytes(ScBytes(
            b.to_vec().try_into().expect("32 bytes fits BytesM"),
        ))
    }

    fn make_event(
        contract_id: Option<[u8; 32]>,
        topics: Vec<ScVal>,
        data: ScVal,
        failed_call: bool,
    ) -> HostEvent {
        let topics_v: VecM<ScVal> = topics.try_into().expect("topics fit in VecM");
        HostEvent {
            event: ContractEvent {
                ext: ExtensionPoint::V0,
                contract_id: contract_id.map(|id| ContractId(Hash(id))),
                type_: ContractEventType::Diagnostic,
                body: ContractEventBody::V0(ContractEventV0 {
                    topics: topics_v,
                    data,
                }),
            },
            failed_call,
        }
    }

    fn fn_call(callee: [u8; 32], func: &str, data: ScVal, failed: bool) -> HostEvent {
        make_event(
            None, // calling contract; for tests we mostly use root calls
            vec![sym("fn_call"), bytes32(callee), sym(func)],
            data,
            failed,
        )
    }

    fn fn_return(callee: [u8; 32], func: &str, value: ScVal, failed: bool) -> HostEvent {
        make_event(
            Some(callee),
            vec![sym("fn_return"), sym(func)],
            value,
            failed,
        )
    }

    fn evs(events: Vec<HostEvent>) -> Events {
        Events(events)
    }

    const C1: [u8; 32] = [1u8; 32];
    const C2: [u8; 32] = [2u8; 32];
    const C3: [u8; 32] = [3u8; 32];

    // --- Parser tests ---------------------------------------------------

    #[test]
    fn empty_event_stream_yields_empty_trace() {
        let trace = Trace::from_events(&evs(vec![]));
        assert!(trace.roots.is_empty());
        assert_eq!(trace.frame_count(), 0);
        assert!(!trace.had_failures());
    }

    #[test]
    fn single_call_returns_one_root() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "balance", ScVal::Void, false),
            fn_return(C1, "balance", i128_val(42), false),
        ]));
        assert_eq!(trace.roots.len(), 1);
        let root = &trace.roots[0];
        assert_eq!(root.function, "balance");
        assert!(matches!(
            &root.result,
            TraceResult::Returned(ScVal::I128(_))
        ));
        assert!(!root.rolled_back);
        assert!(root.children.is_empty());
        assert_eq!(trace.frame_count(), 1);
        assert!(!trace.had_failures());
    }

    #[test]
    fn nested_call_becomes_child() {
        // root → child structure: deposit calls transfer
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "deposit", ScVal::U32(100), false),
            fn_call(C2, "transfer", ScVal::U32(100), false),
            fn_return(C2, "transfer", ScVal::Void, false),
            fn_return(C1, "deposit", ScVal::U32(100), false),
        ]));
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].function, "deposit");
        assert_eq!(trace.roots[0].children.len(), 1);
        assert_eq!(trace.roots[0].children[0].function, "transfer");
        assert_eq!(trace.frame_count(), 2);
    }

    #[test]
    fn three_level_nesting_preserves_order() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "a", ScVal::Void, false),
            fn_call(C2, "b", ScVal::Void, false),
            fn_call(C3, "c", ScVal::Void, false),
            fn_return(C3, "c", ScVal::Void, false),
            fn_return(C2, "b", ScVal::Void, false),
            fn_return(C1, "a", ScVal::Void, false),
        ]));
        let a = &trace.roots[0];
        assert_eq!(a.function, "a");
        let b = &a.children[0];
        assert_eq!(b.function, "b");
        let c = &b.children[0];
        assert_eq!(c.function, "c");
        assert!(c.children.is_empty());
    }

    #[test]
    fn sibling_calls_at_same_depth() {
        // root makes two sibling calls
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "root", ScVal::Void, false),
            fn_call(C2, "first", ScVal::Void, false),
            fn_return(C2, "first", ScVal::Void, false),
            fn_call(C3, "second", ScVal::Void, false),
            fn_return(C3, "second", ScVal::Void, false),
            fn_return(C1, "root", ScVal::Void, false),
        ]));
        assert_eq!(trace.roots[0].children.len(), 2);
        assert_eq!(trace.roots[0].children[0].function, "first");
        assert_eq!(trace.roots[0].children[1].function, "second");
    }

    #[test]
    fn trapped_frame_is_emitted_on_missing_return() {
        // fn_call without matching fn_return → Trapped
        let trace = Trace::from_events(&evs(vec![fn_call(C1, "panicking", ScVal::Void, false)]));
        assert_eq!(trace.roots.len(), 1);
        assert!(matches!(trace.roots[0].result, TraceResult::Trapped));
        assert!(trace.roots[0].rolled_back);
        assert!(trace.had_failures());
    }

    #[test]
    fn rolled_back_frame_keeps_return_value() {
        // fn_return with failed_call=true → Returned but rolled_back=true
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "child", ScVal::Void, false),
            fn_return(C1, "child", ScVal::U32(7), true),
        ]));
        assert_eq!(trace.roots.len(), 1);
        assert!(matches!(
            &trace.roots[0].result,
            TraceResult::Returned(ScVal::U32(7))
        ));
        assert!(trace.roots[0].rolled_back);
        assert!(trace.had_failures());
    }

    #[test]
    fn orphan_return_is_skipped_not_panic() {
        // fn_return with no matching fn_call: should not crash, should not
        // produce a root.
        let trace = Trace::from_events(&evs(vec![fn_return(C1, "lonely", ScVal::Void, false)]));
        assert!(trace.roots.is_empty());
    }

    #[test]
    fn malformed_fn_call_is_skipped() {
        // Topic count < 3: skip without panicking.
        let trace = Trace::from_events(&evs(vec![make_event(
            None,
            vec![sym("fn_call"), bytes32(C1)], // missing function symbol
            ScVal::Void,
            false,
        )]));
        assert!(trace.roots.is_empty());
    }

    #[test]
    fn non_diagnostic_events_are_ignored() {
        // A Contract event mixed in: should be skipped.
        let mut he = make_event(
            None,
            vec![sym("fn_call"), bytes32(C1), sym("evil")],
            ScVal::Void,
            false,
        );
        he.event.type_ = ContractEventType::Contract;
        let trace = Trace::from_events(&evs(vec![he]));
        assert!(trace.roots.is_empty());
    }

    #[test]
    fn multi_arg_data_is_unwrapped() {
        let args_vec = ScVal::Vec(Some(ScVec(
            vec![ScVal::U32(1), ScVal::U32(2), ScVal::U32(3)]
                .try_into()
                .unwrap(),
        )));
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "f", args_vec, false),
            fn_return(C1, "f", ScVal::Void, false),
        ]));
        assert_eq!(trace.roots[0].args.len(), 3);
    }

    #[test]
    fn single_arg_data_is_one_arg() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "f", ScVal::U32(99), false),
            fn_return(C1, "f", ScVal::Void, false),
        ]));
        assert_eq!(trace.roots[0].args, vec![ScVal::U32(99)]);
    }

    #[test]
    fn zero_arg_data_is_no_args() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "f", ScVal::Void, false),
            fn_return(C1, "f", ScVal::Void, false),
        ]));
        assert!(trace.roots[0].args.is_empty());
    }

    // --- Display tests --------------------------------------------------

    #[test]
    fn display_empty_trace_is_explicit() {
        let trace = Trace::default();
        let s = format!("{trace}");
        assert!(s.contains("empty"), "empty trace should explain why: {s:?}");
    }

    #[test]
    fn display_simple_trace_renders_function_and_return() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "balance", ScVal::Void, false),
            fn_return(C1, "balance", i128_val(42), false),
        ]));
        let s = format!("{trace}");
        assert!(s.contains("[TRACE]"));
        assert!(s.contains("balance"));
        assert!(s.contains("42"));
    }

    #[test]
    fn display_marks_rolled_back_frames() {
        let trace = Trace::from_events(&evs(vec![
            fn_call(C1, "child", ScVal::Void, false),
            fn_return(C1, "child", ScVal::Void, true),
        ]));
        let s = format!("{trace}");
        assert!(
            s.contains("rolled back"),
            "rolled-back frame should be tagged: {s:?}"
        );
    }

    #[test]
    fn display_marks_trapped_frames() {
        let trace = Trace::from_events(&evs(vec![fn_call(C1, "boom", ScVal::Void, false)]));
        let s = format!("{trace}");
        assert!(
            s.contains("TRAPPED"),
            "trapped frame should be tagged: {s:?}"
        );
    }

    // --- ScVal renderer focused tests -----------------------------------

    fn render(v: &ScVal) -> String {
        struct W<'a>(&'a ScVal);
        impl fmt::Display for W<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                render_scval(f, self.0)
            }
        }
        W(v).to_string()
    }

    #[test]
    fn renders_primitives_compactly() {
        assert_eq!(render(&ScVal::Void), "()");
        assert_eq!(render(&ScVal::Bool(true)), "true");
        assert_eq!(render(&ScVal::U32(42)), "42");
        assert_eq!(render(&ScVal::I32(-7)), "-7");
        assert_eq!(render(&ScVal::U64(123_456)), "123456");
    }

    #[test]
    fn renders_i128_correctly() {
        let v = i128_val(-1_000_000_000_000);
        assert_eq!(render(&v), "-1000000000000");
    }

    #[test]
    fn renders_short_bytes_as_full_hex() {
        let b = ScVal::Bytes(ScBytes(vec![0xde, 0xad, 0xbe, 0xef].try_into().unwrap()));
        assert_eq!(render(&b), "0xdeadbeef");
    }

    #[test]
    fn renders_long_bytes_with_preview() {
        let b = ScVal::Bytes(ScBytes(vec![0xab; 64].try_into().unwrap()));
        let s = render(&b);
        assert!(s.starts_with("0xabababababababab"));
        assert!(s.contains("64 bytes"));
    }

    #[test]
    fn renders_symbol_as_text() {
        assert_eq!(render(&sym("transfer")), "transfer");
    }

    #[test]
    fn renders_vec_with_commas() {
        let v = ScVal::Vec(Some(ScVec(
            vec![ScVal::U32(1), ScVal::U32(2)].try_into().unwrap(),
        )));
        assert_eq!(render(&v), "[1, 2]");
    }

    #[test]
    fn abbreviate_strkey_short_unchanged() {
        assert_eq!(abbreviate_strkey("abc"), "abc");
    }

    #[test]
    fn abbreviate_strkey_long_truncates_with_ellipsis() {
        let full = "GABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let s = abbreviate_strkey(full);
        assert!(s.starts_with("GABC"));
        assert!(s.ends_with("XYZ"));
        assert!(s.contains('\u{2026}'));
    }

    #[test]
    fn u128_from_pieces_roundtrip() {
        assert_eq!(u128_from_pieces(0, 0), 0);
        assert_eq!(u128_from_pieces(0, 42), 42);
        assert_eq!(u128_from_pieces(1, 0), 1u128 << 64);
        assert_eq!(u128_from_pieces(u64::MAX, u64::MAX), u128::MAX);
    }

    #[test]
    fn i128_from_pieces_handles_negatives() {
        assert_eq!(i128_from_pieces(0, 0), 0);
        assert_eq!(i128_from_pieces(0, 42), 42);
        // -1 is all-ones in two's complement
        assert_eq!(i128_from_pieces(-1, u64::MAX), -1);
        assert_eq!(i128_from_pieces(-1, 0), i128::from(-1i64) << 64);
    }
}
