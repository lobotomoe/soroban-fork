//! Authorization-tree introspection for forked environments.
//!
//! Soroban's host runs the recording auth manager whenever a test enables
//! [`mock_all_auths`](soroban_sdk::Env::mock_all_auths). Every
//! `require_auth` demand made by every contract during a top-level
//! [`invoke_contract`](soroban_sdk::Env::invoke_contract) is recorded as
//! a [`RecordedAuthPayload`]: one entry per signer, each carrying the
//! full tree of invocations that signer is being asked to authorize.
//! This module reads that recorded set out and gives it a
//! Foundry-`-vvvv`-style [`Display`] impl suitable for both programmatic
//! assertions and human debugging.
//!
//! # Enabling
//!
//! Recording auth is on whenever you call `env.mock_all_auths()` (or the
//! `_allowing_non_root_auth` variant — see the README's "Common
//! pitfalls" section before reaching for it). No `ForkConfig` flag is
//! required. [`AuthTree`] will be empty until at least one top-level
//! `invoke_contract` has run, since the host populates the recorded
//! payloads as a side effect of completing that invocation.
//!
//! ```rust,no_run
//! use soroban_fork::ForkConfig;
//!
//! let env = ForkConfig::new("https://soroban-mainnet.stellar.org:443")
//!     .build()
//!     .expect("fork");
//! env.mock_all_auths();
//!
//! // ... env.invoke_contract::<i128>(&pool, &deposit, args) ...
//!
//! eprintln!("{}", env.auth_tree());
//! ```
//!
//! # What this module captures
//!
//! For every authorization payload the host recorded:
//!
//! - The signer **address** ([`Some`] for an explicit invoker; [`None`]
//!   when the source account of the transaction is the implicit signer
//!   and no separate signature is required).
//! - The **nonce** ([`Some`] for replay-protected payloads, [`None`] for
//!   source-account auth which doesn't carry a nonce).
//! - The full [`SorobanAuthorizedInvocation`] tree, including recursive
//!   `sub_invocations` made on behalf of this signer.
//!
//! # What this module does NOT capture
//!
//! Two limits inherited from the upstream `soroban-env-host` API. Both
//! could be lifted with cooperation from the host crate; until then,
//! we are honest about the gap rather than ship a half-measure.
//!
//! - **`Error(Auth, InvalidAction)` failure args.** When `require_auth`
//!   fails, soroban-env-host constructs the error locally with only the
//!   address in the diagnostic args; the contract that demanded auth,
//!   the function name, and the expected authorizer are not persisted
//!   to any host accessor we can read out post-failure. After a failed
//!   call, [`AuthTree`] reflects whatever payload set the host left in
//!   its `previous_authorization_manager`; the precise contents on a
//!   panic mid-invocation are an implementation detail of
//!   `soroban-env-host` and not something this crate guarantees. A
//!   structured `last_auth_failure()` accessor with the failed contract
//!   and function name awaits an upstream change.
//!
//! - **The `disable_non_root_auth` mode flag.** Whether
//!   `mock_all_auths_allowing_non_root_auth` was used vs. plain
//!   `mock_all_auths` is not exposed by the host. We can't enforce a
//!   strict-mode invariant from outside the host; the README's
//!   "Common pitfalls" section documents the trap.
//!
//! # Per-invocation scoping
//!
//! Like [`Trace`](crate::trace::Trace), the recorded payloads reflect
//! only the **most recent** top-level `invoke_contract`. Earlier
//! invocations' payloads are gone. Capture each `auth_tree()` before
//! the next call if you need history.

use std::fmt;

use soroban_env_host::auth::RecordedAuthPayload;
use soroban_env_host::xdr::{
    InvokeContractArgs, SorobanAuthorizedFunction, SorobanAuthorizedInvocation,
};

use crate::trace::{render_address, render_scval};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Captured authorization-payload set from the most recent top-level
/// `invoke_contract`.
///
/// Construct via [`ForkedEnv::auth_tree`](crate::ForkedEnv::auth_tree)
/// in the common case, or [`AuthTree::from_payloads`] when wrapping a
/// payload Vec retrieved through some other path.
#[derive(Debug)]
pub struct AuthTree {
    /// One entry per signer that the recording auth manager observed
    /// `require_auth` demands for during the most recent top-level
    /// invocation. Held verbatim from the host (no copying), so the
    /// `RecordedAuthPayload` shape matches the upstream definition
    /// one-to-one.
    pub payloads: Vec<RecordedAuthPayload>,
}

impl AuthTree {
    /// Wrap an already-fetched payload Vec in this Display-friendly shell.
    ///
    /// Useful when you've called
    /// [`ForkedEnv::auth_payloads`](crate::ForkedEnv::auth_payloads)
    /// for programmatic inspection and now want a string rendering as
    /// well — the Vec is moved in once, no extra host round-trip.
    pub fn from_payloads(payloads: Vec<RecordedAuthPayload>) -> Self {
        Self { payloads }
    }

    /// Number of distinct authorisations recorded. Each payload covers
    /// one signer's tree of invocations.
    pub fn payload_count(&self) -> usize {
        self.payloads.len()
    }

    /// Total invocations across all payloads, recursively counting every
    /// `sub_invocation`. Useful for asserting that a multi-hop call
    /// demanded the expected number of `require_auth`s.
    pub fn invocation_count(&self) -> usize {
        self.payloads
            .iter()
            .map(|p| count_invocations(&p.invocation))
            .sum()
    }

    /// `true` when no payloads were recorded — typically because no
    /// top-level invocation has run yet, or the invocation made no
    /// `require_auth` demands.
    pub fn is_empty(&self) -> bool {
        self.payloads.is_empty()
    }
}

/// Recursively count [`SorobanAuthorizedInvocation`] nodes.
fn count_invocations(inv: &SorobanAuthorizedInvocation) -> usize {
    1 + inv
        .sub_invocations
        .iter()
        .map(count_invocations)
        .sum::<usize>()
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

const INDENT: &str = "  ";

impl fmt::Display for AuthTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.payloads.is_empty() {
            return writeln!(
                f,
                "[AUTH] (empty — has invoke_contract run yet, and did it demand any auth?)"
            );
        }
        writeln!(f, "[AUTH]")?;
        for (idx, payload) in self.payloads.iter().enumerate() {
            render_payload(f, idx, payload)?;
        }
        Ok(())
    }
}

fn render_payload(
    f: &mut fmt::Formatter<'_>,
    idx: usize,
    payload: &RecordedAuthPayload,
) -> fmt::Result {
    write!(f, "{INDENT}payload #{idx}  signer=")?;
    match &payload.address {
        Some(addr) => render_address(f, addr)?,
        None => write!(f, "<source account>")?,
    }
    if let Some(n) = payload.nonce {
        write!(f, "  nonce={n}")?;
    }
    writeln!(f)?;
    render_invocation(f, &payload.invocation, 2)
}

fn render_invocation(
    f: &mut fmt::Formatter<'_>,
    inv: &SorobanAuthorizedInvocation,
    depth: usize,
) -> fmt::Result {
    let pad = INDENT.repeat(depth);
    match &inv.function {
        SorobanAuthorizedFunction::ContractFn(args) => render_contract_fn(f, &pad, args)?,
        SorobanAuthorizedFunction::CreateContractHostFn(_) => {
            // The CreateContractHostFn / V2 variants are rare in
            // ergonomic test code (deploy flows usually go through
            // `UploadContractWasm` + `CreateContract` envelopes, not
            // require_auth payloads). Emit a placeholder rather than
            // pulling the entire ContractIDPreimage decoder in here —
            // we can flesh out the rendering when a real test asks for it.
            writeln!(f, "{pad}<create_contract>")?;
        }
        SorobanAuthorizedFunction::CreateContractV2HostFn(_) => {
            writeln!(f, "{pad}<create_contract_v2>")?;
        }
    }
    for sub in inv.sub_invocations.iter() {
        render_invocation(f, sub, depth + 1)?;
    }
    Ok(())
}

fn render_contract_fn(
    f: &mut fmt::Formatter<'_>,
    pad: &str,
    args: &InvokeContractArgs,
) -> fmt::Result {
    write!(f, "{pad}[")?;
    render_address(f, &args.contract_address)?;
    write!(f, "] ")?;

    match std::str::from_utf8(args.function_name.0.as_slice()) {
        Ok(name) => write!(f, "{name}")?,
        Err(_) => write!(f, "<non-utf8>")?,
    }

    write!(f, "(")?;
    for (i, arg) in args.args.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        render_scval(f, arg)?;
    }
    writeln!(f, ")")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_env_host::xdr::{
        AccountId, ContractId, Hash, Int128Parts, PublicKey, ScAddress, ScSymbol, ScVal, Uint256,
        VecM,
    };

    /// 32-byte ed25519 public key for a deterministic test "signer".
    /// Strkey form: `GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA…` etc.
    /// Using mostly-zero bytes keeps the strkey CRC computation trivial
    /// while still producing a valid 56-character "G..." encoding.
    fn ed25519_pk(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn account_addr(byte: u8) -> ScAddress {
        ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            ed25519_pk(byte),
        ))))
    }

    fn contract_addr(byte: u8) -> ScAddress {
        ScAddress::Contract(ContractId(Hash([byte; 32])))
    }

    fn symbol(s: &str) -> ScSymbol {
        ScSymbol(s.try_into().expect("symbol fits in 32 bytes"))
    }

    fn i128_val(v: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (v >> 64) as i64,
            lo: v as u64,
        })
    }

    fn invoke(
        contract: ScAddress,
        function: &str,
        args: Vec<ScVal>,
    ) -> SorobanAuthorizedInvocation {
        SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: contract,
                function_name: symbol(function),
                args: args.try_into().expect("args fit in VecM"),
            }),
            sub_invocations: VecM::default(),
        }
    }

    fn invoke_with_subs(
        contract: ScAddress,
        function: &str,
        args: Vec<ScVal>,
        subs: Vec<SorobanAuthorizedInvocation>,
    ) -> SorobanAuthorizedInvocation {
        SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: contract,
                function_name: symbol(function),
                args: args.try_into().expect("args fit in VecM"),
            }),
            sub_invocations: subs.try_into().expect("subs fit in VecM"),
        }
    }

    #[test]
    fn empty_tree_renders_explanatory_text() {
        let tree = AuthTree::from_payloads(vec![]);
        let out = tree.to_string();
        assert!(out.starts_with("[AUTH] (empty"));
        assert!(tree.is_empty());
        assert_eq!(tree.payload_count(), 0);
        assert_eq!(tree.invocation_count(), 0);
    }

    #[test]
    fn single_payload_with_explicit_signer_and_nonce() {
        let payload = RecordedAuthPayload {
            address: Some(account_addr(0xAA)),
            nonce: Some(12345),
            invocation: invoke(contract_addr(0xCC), "deposit", vec![i128_val(1_000_000)]),
        };
        let tree = AuthTree::from_payloads(vec![payload]);
        let out = tree.to_string();

        assert!(out.contains("[AUTH]"));
        assert!(out.contains("payload #0"));
        assert!(out.contains("signer="));
        assert!(out.contains("nonce=12345"));
        assert!(out.contains("deposit(1000000)"));
        assert_eq!(tree.payload_count(), 1);
        assert_eq!(tree.invocation_count(), 1);
    }

    #[test]
    fn source_account_signer_renders_placeholder() {
        let payload = RecordedAuthPayload {
            address: None,
            nonce: None,
            invocation: invoke(contract_addr(0xCC), "submit", vec![]),
        };
        let tree = AuthTree::from_payloads(vec![payload]);
        let out = tree.to_string();
        assert!(out.contains("signer=<source account>"));
        // No nonce field at all when absent.
        assert!(!out.contains("nonce="));
    }

    #[test]
    fn nested_sub_invocations_indent_properly() {
        let inner = invoke(contract_addr(0xDD), "transfer_from", vec![i128_val(500)]);
        let outer = invoke_with_subs(
            contract_addr(0xCC),
            "deposit",
            vec![i128_val(500)],
            vec![inner],
        );
        let payload = RecordedAuthPayload {
            address: Some(account_addr(0xAA)),
            nonce: Some(1),
            invocation: outer,
        };
        let tree = AuthTree::from_payloads(vec![payload]);
        let out = tree.to_string();

        // The inner frame should be indented one level deeper than the outer.
        // We don't assert exact byte counts because abbreviation rules may
        // shift; we only assert that both frames appear and the inner has
        // more leading whitespace than the outer.
        let outer_line = out
            .lines()
            .find(|l| l.contains("deposit("))
            .expect("outer line present");
        let inner_line = out
            .lines()
            .find(|l| l.contains("transfer_from("))
            .expect("inner line present");

        let outer_pad = outer_line.len() - outer_line.trim_start().len();
        let inner_pad = inner_line.len() - inner_line.trim_start().len();
        assert!(
            inner_pad > outer_pad,
            "inner frame ({inner_pad}) must be indented deeper than outer ({outer_pad})\n{out}"
        );
        assert_eq!(tree.invocation_count(), 2);
    }

    #[test]
    fn multi_payload_numbering() {
        let p0 = RecordedAuthPayload {
            address: Some(account_addr(0xAA)),
            nonce: Some(1),
            invocation: invoke(contract_addr(0xCC), "alpha", vec![]),
        };
        let p1 = RecordedAuthPayload {
            address: Some(account_addr(0xBB)),
            nonce: Some(2),
            invocation: invoke(contract_addr(0xDD), "beta", vec![]),
        };
        let tree = AuthTree::from_payloads(vec![p0, p1]);
        let out = tree.to_string();
        assert!(out.contains("payload #0"));
        assert!(out.contains("payload #1"));
        assert!(out.contains("alpha("));
        assert!(out.contains("beta("));
        assert_eq!(tree.payload_count(), 2);
    }

    #[test]
    fn invocation_count_is_recursive() {
        let leaf_a = invoke(contract_addr(0xEE), "burn", vec![]);
        let leaf_b = invoke(contract_addr(0xFF), "mint", vec![]);
        let mid = invoke_with_subs(
            contract_addr(0xDD),
            "transfer",
            vec![],
            vec![leaf_a, leaf_b],
        );
        let root = invoke_with_subs(contract_addr(0xCC), "deposit", vec![], vec![mid]);
        let payload = RecordedAuthPayload {
            address: Some(account_addr(0xAA)),
            nonce: Some(1),
            invocation: root,
        };
        let tree = AuthTree::from_payloads(vec![payload]);
        // root + mid + leaf_a + leaf_b == 4
        assert_eq!(tree.invocation_count(), 4);
    }
}
