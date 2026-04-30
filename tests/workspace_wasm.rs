//! # `workspace_wasm` integration tests
//!
//! Closes the testability gap that [`workspace_wasm`] had through v0.9.1:
//! the public function shells out to `cargo metadata` + `cargo build`,
//! which can't be tested by mocking — only by running cargo against a
//! real workspace. v0.9.2 added [`workspace_wasm_in`] taking an
//! explicit `manifest_dir`, which lets these tests spin up a fixture
//! workspace in a tempdir and exercise the full pipeline end-to-end.
//!
//! Two scenarios:
//!
//! 1. **Unknown-crate error path** — runs unconditionally. Builds no
//!    WASM, only exercises `cargo metadata`, so it doesn't need the
//!    `wasm32v1-none` target installed and it runs fast in CI.
//!
//! 2. **Real WASM build** — `#[ignore]`d because it requires the
//!    `wasm32v1-none` target (`rustup target add wasm32v1-none`) and
//!    runs `cargo build` against a fresh dependency graph (≈ 30 s the
//!    first time). Run locally with
//!    `cargo test --test workspace_wasm -- --ignored --nocapture`.

use soroban_fork::{workspace_wasm_in, ForkError};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Allocate a fresh tempdir for one test run. Combines the system temp
/// directory with the test process pid and a nanosecond timestamp so
/// concurrent test runs (cargo's `--test-threads=N`) never collide.
///
/// Cleanup is best-effort on success — we delete the dir at the end of
/// each test. If a test panics partway through, the dir is left behind
/// for inspection; macOS / Linux temp directories get reaped by the OS
/// eventually, and CI environments are ephemeral.
fn fresh_fixture_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is past UNIX epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "soroban-fork-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Write a minimal 2-file workspace fixture under `root`:
/// - `Cargo.toml` declaring `demo_contract` as the single member
/// - `demo_contract/Cargo.toml` with cdylib + soroban-sdk
/// - `demo_contract/src/lib.rs` with one trivial function
///
/// Returns `()`; tests inspect `root` after the call.
fn write_demo_workspace(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["demo_contract"]
"#,
    )
    .expect("write workspace Cargo.toml");

    let crate_dir = root.join("demo_contract");
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir).expect("create demo_contract/src");

    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"[package]
name = "demo_contract"
version = "0.0.1"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
soroban-sdk = "25.3.0"

[profile.release]
opt-level = "z"
overflow-checks = true
debug = 0
strip = "symbols"
debug-assertions = false
panic = "abort"
codegen-units = 1
lto = true
"#,
    )
    .expect("write demo_contract Cargo.toml");

    fs::write(
        src_dir.join("lib.rs"),
        r#"#![no_std]
use soroban_sdk::{contract, contractimpl};

#[contract]
pub struct Demo;

#[contractimpl]
impl Demo {
    pub fn double(x: i32) -> i32 {
        x.saturating_mul(2)
    }
}
"#,
    )
    .expect("write demo_contract lib.rs");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `workspace_wasm_in` should reject a crate name that isn't in the
/// workspace, without ever trying to compile anything. Exercises the
/// `cargo metadata` path + the `require_workspace_member` cross-reference
/// against `packages[].name`. Doesn't need the wasm target installed,
/// doesn't compile any wasm, fast enough to run in default CI.
#[test]
fn errors_when_crate_is_not_a_workspace_member() {
    let dir = fresh_fixture_dir("unknown-crate");
    write_demo_workspace(&dir);

    let result = workspace_wasm_in(Some(&dir), "no_such_crate", "wasm32v1-none", "release");

    let err = match result {
        Err(e) => e,
        Ok(bytes) => {
            let _ = fs::remove_dir_all(&dir);
            panic!(
                "expected error for unknown crate, got Ok({} bytes)",
                bytes.len()
            );
        }
    };

    let message = match &err {
        ForkError::Workspace(m) => m.clone(),
        other => {
            let _ = fs::remove_dir_all(&dir);
            panic!("expected ForkError::Workspace, got {other:?}");
        }
    };

    assert!(
        message.contains("no_such_crate is not a workspace member"),
        "error should name the missing crate; got: {message}"
    );
    // The message also lists the workspace members it *did* find, so
    // the user sees what cargo metadata reported.
    assert!(
        message.contains("demo_contract"),
        "error should list the actual workspace members; got: {message}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// End-to-end: build a real Soroban contract WASM from a fixture
/// workspace and verify the resulting bytes start with the WASM magic
/// `\0asm`.
///
/// `#[ignore]` because it requires the `wasm32v1-none` rustup target
/// and the first run downloads the `soroban-sdk` dependency graph
/// (≈ 30 s on a cold cache, sub-second on a warm one). Run with:
/// ```sh
/// cargo test --test workspace_wasm -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires wasm32v1-none target (rustup target add wasm32v1-none); slow first build (~30s on cold cache)"]
fn builds_real_contract_in_isolated_workspace() {
    let dir = fresh_fixture_dir("e2e");
    write_demo_workspace(&dir);

    let bytes = workspace_wasm_in(Some(&dir), "demo_contract", "wasm32v1-none", "release")
        .expect("workspace_wasm_in should succeed for a well-formed fixture");

    // Sanity: a real Soroban contract WASM is at least a few hundred
    // bytes. The trivial `Demo::double` contract above measured 619
    // bytes during the original v0.9.1 smoke test on macOS / soroban-sdk
    // 25.3.0; we accept anything ≥ 100 to stay tolerant of toolchain
    // drift.
    assert!(
        bytes.len() >= 100,
        "wasm should be ≥ 100 bytes, got {} bytes (truncated build output?)",
        bytes.len()
    );
    assert_eq!(
        &bytes[..4],
        b"\0asm",
        "wasm magic missing — first 4 bytes were {:?}",
        &bytes[..4.min(bytes.len())]
    );

    eprintln!(
        "workspace_wasm_in built demo_contract: {} bytes, magic OK",
        bytes.len()
    );

    let _ = fs::remove_dir_all(&dir);
}
