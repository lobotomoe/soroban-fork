//! Error types for the `soroban-fork` crate.
//!
//! Every fallible operation in the public API returns [`Result<T, ForkError>`]
//! (aliased as [`Result<T>`]). Errors are typed so callers can discriminate
//! transport failures from cache-file problems from XDR-decode problems,
//! rather than grepping stringly-typed errors.

use std::path::PathBuf;

/// The crate's unified error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForkError {
    /// A JSON-RPC HTTP request failed (network error, timeout, or non-success
    /// status after all retries).
    #[error("RPC transport error: {0}")]
    Transport(String),

    /// The RPC endpoint returned a JSON-RPC level error object.
    #[error("RPC returned error: {0}")]
    RpcError(String),

    /// The RPC response was well-formed JSON but didn't contain a `result`
    /// field (protocol violation on the server side).
    #[error("RPC response had no result field")]
    RpcNoResult,

    /// An XDR payload failed to encode or decode. Usually indicates an
    /// incompatibility between the pinned `stellar-xdr` version and the
    /// protocol version the RPC server is serving.
    #[error("XDR codec error: {0}")]
    Xdr(String),

    /// A base64 payload from the RPC failed to decode.
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),

    /// JSON (de)serialization error.
    #[error("JSON codec error: {0}")]
    Json(#[from] serde_json::Error),

    /// Reading or writing the on-disk cache snapshot failed.
    #[error("cache I/O error at {path}: {message}")]
    Cache {
        /// The path we were trying to read or write.
        path: PathBuf,
        /// The underlying I/O-or-serialization error, stringified because
        /// [`soroban_ledger_snapshot`] returns a non-`std::error::Error`
        /// type for its file operations. Not named `source` so thiserror
        /// doesn't confuse it with the error-chain source attribute.
        message: String,
    },
}

impl From<reqwest::Error> for ForkError {
    fn from(value: reqwest::Error) -> Self {
        Self::Transport(value.to_string())
    }
}

/// Convenience alias — every public API returns this.
pub type Result<T> = std::result::Result<T, ForkError>;
