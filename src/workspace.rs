//! Workspace contract WASM builder — closes the `include_bytes!` rebuild trap.
//!
//! When a Soroban test loads its contract bytes via something like
//!
//! ```rust,ignore
//! const WASM: &[u8] = include_bytes!(
//!     "../target/wasm32v1-none/release/my_contract.wasm"
//! );
//! ```
//!
//! Cargo will *not* rebuild the contract when the contract crate's
//! `.rs` files change. `cargo test` rebuilds only the test binary; the
//! contract is a separate compilation graph that Cargo doesn't know to
//! re-run from the test crate's point of view. Result: the test passes
//! against stale WASM bytes the next time someone edits the contract
//! and forgets to run `stellar contract build` first.
//!
//! [`workspace_wasm`] closes this trap by invoking Cargo at test
//! runtime to (re)build the named crate against the Soroban WASM
//! target before reading the resulting bytes. Cargo's incremental
//! compilation keeps the rebuild cheap when the source hasn't actually
//! changed (sub-second on small crates).
//!
//! ```rust,no_run
//! let wasm = soroban_fork::workspace_wasm("my_contract")
//!     .expect("build my_contract for wasm32v1-none");
//! // …pass `wasm` to whatever needs the bytes — Env::register, an
//! //   UploadContractWasm envelope, fork_setCode over JSON-RPC, etc.
//! ```
//!
//! # Layout assumptions
//!
//! - `crate_name` is a member of the same Cargo workspace as the
//!   calling test. The workspace root and target directory are
//!   located via `cargo metadata`, so an explicit `CARGO_TARGET_DIR`
//!   environment override is honored automatically.
//! - The crate produces a single `cdylib` whose artifact name (with
//!   `-` replaced by `_`) becomes `<crate_name>.wasm` — Cargo's
//!   standard naming for cdylib outputs.
//! - The default build target is `wasm32v1-none` (current Soroban
//!   target as of soroban-sdk 25.x). Use [`workspace_wasm_with`] to
//!   override this for older soroban-sdk versions or custom profiles.
//!
//! # When this isn't enough
//!
//! For test suites that load wasm in many places, the cheaper
//! alternative is a `build.rs` in the test crate that runs
//! `cargo build -p <contract>` once per `cargo test` invocation, with
//! `cargo:rerun-if-changed=` directives pointing at the contract's
//! source files. That keeps the compile cost out of every individual
//! test. The README's "Common pitfalls" section documents the
//! tradeoff.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ForkError, Result};

/// Default Soroban WASM compilation target. Matches soroban-sdk 25.x.
const DEFAULT_TARGET: &str = "wasm32v1-none";

/// Default Cargo build profile. Soroban contracts ship `--release`
/// because debug builds typically exceed the network's resource
/// limits — and a fork tool that produces bytes the network would
/// reject would be misleading.
const DEFAULT_PROFILE: &str = "release";

/// Build the named workspace member as a Soroban contract WASM and
/// return the resulting bytes.
///
/// See module-level docs for layout assumptions and the rationale.
/// Calls `cargo build -p <crate_name> --target wasm32v1-none --release`
/// and reads the resulting `.wasm` from the workspace's target
/// directory.
///
/// # Errors
///
/// Returns [`ForkError::Workspace`] when:
///
/// - `cargo metadata` fails (cargo not on `PATH`, not inside a
///   workspace, or workspace metadata malformed).
/// - The named crate is not a workspace member.
/// - `cargo build` fails — the underlying compilation error is
///   surfaced verbatim so the failing test reports it straight.
/// - The expected `.wasm` artifact is missing or unreadable after a
///   successful build.
pub fn workspace_wasm(crate_name: &str) -> Result<Vec<u8>> {
    workspace_wasm_in(None, crate_name, DEFAULT_TARGET, DEFAULT_PROFILE)
}

/// Like [`workspace_wasm`], but with an explicit build target triple
/// and profile. Useful when downstream pins to an older Soroban
/// target (`wasm32-unknown-unknown` for soroban-sdk <25, for
/// example) or a non-`release` profile for size optimisation.
pub fn workspace_wasm_with(crate_name: &str, target: &str, profile: &str) -> Result<Vec<u8>> {
    workspace_wasm_in(None, crate_name, target, profile)
}

/// Most general form of the helper: explicit `manifest_dir` selects which
/// workspace's `cargo metadata` / `cargo build` we run, instead of
/// inheriting the calling process's current directory.
///
/// Pass `None` to use the calling process's cwd — that's what
/// [`workspace_wasm`] and [`workspace_wasm_with`] do, and it's right
/// for the typical "I'm in my test crate inside the workspace" case.
///
/// Pass `Some(dir)` to root the cargo invocations at an explicit
/// directory. This exists so integration tests can spin up a fixture
/// workspace in a tempdir without polluting the test harness's own
/// cwd, and it's the testing seam that lets the
/// [tests/workspace_wasm.rs][int] integration test exercise the full
/// build pipeline without modifying soroban-fork itself into a
/// workspace.
///
/// [int]: https://github.com/lobotomoe/soroban-fork/blob/main/tests/workspace_wasm.rs
pub fn workspace_wasm_in(
    manifest_dir: Option<&Path>,
    crate_name: &str,
    target: &str,
    profile: &str,
) -> Result<Vec<u8>> {
    let metadata = read_metadata(manifest_dir)?;
    let target_dir = extract_target_dir(&metadata)?;
    require_workspace_member(&metadata, crate_name)?;

    invoke_cargo_build(manifest_dir, crate_name, target, profile)?;

    let wasm_path = target_dir
        .join(target)
        .join(profile)
        .join(format!("{}.wasm", crate_name.replace('-', "_")));

    std::fs::read(&wasm_path).map_err(|e| {
        ForkError::Workspace(format!(
            "expected wasm output at {} after a successful build, but read failed: {}",
            wasm_path.display(),
            e
        ))
    })
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Pick the cargo binary respecting the `CARGO` environment variable
/// rustup sets when invoking subprocesses from a cargo-driven build.
/// Falls back to the bare `"cargo"` name if `CARGO` isn't set, which
/// then resolves through `PATH`.
fn cargo_bin() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn read_metadata(manifest_dir: Option<&Path>) -> Result<serde_json::Value> {
    let mut cmd = Command::new(cargo_bin());
    cmd.args(["metadata", "--no-deps", "--format-version=1"]);
    if let Some(dir) = manifest_dir {
        cmd.current_dir(dir);
    }
    let output = cmd
        .output()
        .map_err(|e| ForkError::Workspace(format!("failed to spawn `cargo metadata`: {e}")))?;
    if !output.status.success() {
        return Err(ForkError::Workspace(format!(
            "`cargo metadata` failed (exit {}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| ForkError::Workspace(format!("parsing cargo metadata JSON: {e}")))
}

fn extract_target_dir(metadata: &serde_json::Value) -> Result<PathBuf> {
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| {
            ForkError::Workspace("cargo metadata response missing `target_directory`".into())
        })
}

/// Verify the named crate appears in the workspace.
///
/// Cargo's `workspace_members` array is a list of opaque package IDs
/// whose textual format has changed between Cargo versions (≤ 1.76 was
/// `<name> <version> (path+file:///…)`; ≥ 1.77 is
/// `path+file:///…#<name>@<version>`, with the `<name>@` prefix
/// dropped when the name equals the directory basename). Rather than
/// parse the IDs ourselves, we cross-reference them against the
/// `packages` array — every `packages[]` entry has a stable `name`
/// field that comes straight from `Cargo.toml`.
fn require_workspace_member(metadata: &serde_json::Value, crate_name: &str) -> Result<()> {
    let workspace_member_ids: std::collections::HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .ok_or_else(|| {
            ForkError::Workspace("cargo metadata response missing `workspace_members`".into())
        })?
        .iter()
        .filter_map(|m| m.as_str())
        .collect();

    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| ForkError::Workspace("cargo metadata response missing `packages`".into()))?;

    let workspace_names: Vec<&str> = packages
        .iter()
        .filter(|pkg| {
            pkg["id"]
                .as_str()
                .map(|id| workspace_member_ids.contains(id))
                .unwrap_or(false)
        })
        .filter_map(|pkg| pkg["name"].as_str())
        .collect();

    if workspace_names.contains(&crate_name) {
        return Ok(());
    }

    Err(ForkError::Workspace(format!(
        "{crate_name} is not a workspace member. cargo metadata listed: [{}]",
        workspace_names.join(", ")
    )))
}

fn invoke_cargo_build(
    manifest_dir: Option<&Path>,
    crate_name: &str,
    target: &str,
    profile: &str,
) -> Result<()> {
    let mut cmd = Command::new(cargo_bin());
    cmd.args(["build", "-p", crate_name, "--target", target]);
    // Cargo accepts `--release` as sugar for `--profile=release` and
    // accepts the bare default `dev` profile silently. For any other
    // profile name (custom user-defined profiles), pass `--profile`
    // explicitly.
    match profile {
        "release" => {
            cmd.arg("--release");
        }
        "dev" => {}
        other => {
            cmd.args(["--profile", other]);
        }
    }
    if let Some(dir) = manifest_dir {
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .map_err(|e| ForkError::Workspace(format!("failed to spawn `cargo build`: {e}")))?;
    if !status.success() {
        return Err(ForkError::Workspace(format!(
            "`cargo build -p {crate_name} --target {target}` failed (exit {status}). \
             cargo's stderr was already written to this process's stderr."
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal cargo-metadata-shaped JSON value for tests.
    /// `members` is a list of `(id, name)` pairs that get split across
    /// `workspace_members` (just the ids) and `packages` (one entry
    /// per pair, with id + name). We don't bother with version,
    /// manifest_path, or any other fields the real production code
    /// doesn't read.
    fn metadata_with(members: &[(&str, &str)]) -> serde_json::Value {
        let workspace_members: Vec<serde_json::Value> = members
            .iter()
            .map(|(id, _)| serde_json::json!(id))
            .collect();
        let packages: Vec<serde_json::Value> = members
            .iter()
            .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
            .collect();
        serde_json::json!({
            "workspace_members": workspace_members,
            "packages": packages,
        })
    }

    #[test]
    fn member_check_accepts_old_cargo_id_format() {
        // Cargo ≤ 1.76: "name version (path+file:///…)"
        let metadata = metadata_with(&[
            ("foo 0.1.0 (path+file:///tmp/foo)", "foo"),
            ("bar-baz 0.2.0 (path+file:///tmp/bar-baz)", "bar-baz"),
        ]);
        assert!(require_workspace_member(&metadata, "foo").is_ok());
        assert!(require_workspace_member(&metadata, "bar-baz").is_ok());
    }

    #[test]
    fn member_check_accepts_new_cargo_id_format_with_name() {
        // Cargo ≥ 1.77, name-differs case: "path+file:///…#name@version"
        let metadata =
            metadata_with(&[("path+file:///tmp/aliased-dir#real-name@0.1.0", "real-name")]);
        assert!(require_workspace_member(&metadata, "real-name").is_ok());
    }

    #[test]
    fn member_check_accepts_new_cargo_id_format_name_dropped() {
        // Cargo ≥ 1.77, name-equals-basename case: the `name@` prefix
        // is dropped and the id is just `path+file:///…#version`.
        // This was the bug the v0.9.1 smoke-test surfaced.
        let metadata = metadata_with(&[(
            "path+file:///tmp/sfork-smoke/demo_contract#0.0.1",
            "demo_contract",
        )]);
        assert!(require_workspace_member(&metadata, "demo_contract").is_ok());
    }

    #[test]
    fn member_check_rejects_unknown_member() {
        let metadata = metadata_with(&[("path+file:///tmp/foo#foo@0.1.0", "foo")]);
        let err = require_workspace_member(&metadata, "bar").unwrap_err();
        assert!(err.to_string().contains("bar is not a workspace member"));
        // The error message should also list the actually-present
        // members so the test author sees what cargo did find.
        assert!(err.to_string().contains("foo"));
    }

    #[test]
    fn member_check_does_not_match_substring_of_member_name() {
        // 'foo' must not match a member named 'foobar'. We're now
        // matching against `packages[].name` which is an exact
        // string compare, so this is structurally guaranteed —
        // but kept as a regression test in case anyone refactors
        // back to substring matching.
        let metadata = metadata_with(&[("path+file:///tmp/foobar#foobar@0.1.0", "foobar")]);
        assert!(require_workspace_member(&metadata, "foo").is_err());
    }

    #[test]
    fn member_check_errors_when_workspace_members_missing() {
        let metadata = serde_json::json!({});
        let err = require_workspace_member(&metadata, "foo").unwrap_err();
        assert!(err.to_string().contains("missing `workspace_members`"));
    }

    #[test]
    fn member_check_errors_when_packages_missing() {
        // workspace_members present but packages not — malformed
        // cargo output, surface explicitly rather than falsely
        // claiming the crate isn't a member.
        let metadata = serde_json::json!({
            "workspace_members": ["path+file:///tmp/foo#foo@0.1.0"],
        });
        let err = require_workspace_member(&metadata, "foo").unwrap_err();
        assert!(err.to_string().contains("missing `packages`"));
    }

    #[test]
    fn member_check_ignores_non_workspace_packages() {
        // The `packages` array contains every transitive dependency
        // in the build graph, not just workspace members. We only
        // accept matches whose `id` is in `workspace_members`.
        let metadata = serde_json::json!({
            "workspace_members": ["path+file:///tmp/local#local@0.1.0"],
            "packages": [
                { "id": "path+file:///tmp/local#local@0.1.0", "name": "local" },
                { "id": "registry+https://crates.io#serde@1.0.0", "name": "serde" },
            ]
        });
        assert!(require_workspace_member(&metadata, "local").is_ok());
        // serde is a transitive dep, not a workspace member — must reject.
        assert!(require_workspace_member(&metadata, "serde").is_err());
    }

    #[test]
    fn target_dir_extracted_from_metadata() {
        let metadata = serde_json::json!({
            "target_directory": "/tmp/target"
        });
        assert_eq!(
            extract_target_dir(&metadata).unwrap(),
            PathBuf::from("/tmp/target")
        );
    }

    #[test]
    fn target_dir_errors_on_missing() {
        let metadata = serde_json::json!({});
        let err = extract_target_dir(&metadata).unwrap_err();
        assert!(err.to_string().contains("missing `target_directory`"));
    }
}
