//! Derives build-time constants: the `--version` string and the run-seal secret.
//!
//! Version precedence:
//! 1. `BASTION_VERSION` env var, when set and non-empty (release pipelines).
//! 2. `git describe --always --tags --dirty=-dirty` (tag, else short SHA, with a
//!    `-dirty` suffix when the working tree has uncommitted changes).
//! 3. The crate's `Cargo.toml` version, when git is unavailable (e.g. a source
//!    tarball with no `.git`).
//!
//! Sealing-secret precedence (see `src/seal.rs` and
//! `docs/developer-guide/attestation.md`):
//! 1. `BASTION_SEAL_SECRET` env var, when set and non-empty (a release pipeline
//!    mints one and shares it across the platform matrix for that release).
//! 2. A random 32-byte value, hex-encoded, generated once and cached under
//!    `OUT_DIR` so a dev binary's incremental rebuilds keep sealing with the same
//!    secret rather than invalidating every previously sealed local run.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BASTION_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let version = std::env::var("BASTION_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!(
        "cargo:rustc-env=BASTION_VERSION={}",
        sanitize_version(&version)
    );

    println!("cargo:rerun-if-env-changed=BASTION_SEAL_SECRET");
    println!("cargo:rustc-env=BASTION_SEAL_SECRET={}", seal_secret());
}

/// Resolve the sealing secret embedded into this binary.
///
/// An explicit `BASTION_SEAL_SECRET` wins (a release build). Otherwise a random
/// secret is generated and cached in `OUT_DIR/seal-secret` so it survives
/// incremental rebuilds: without caching, every `cargo build` of a dev binary
/// would mint a fresh secret and invalidate every run it had already sealed.
fn seal_secret() -> String {
    if let Ok(secret) = std::env::var("BASTION_SEAL_SECRET")
        && !secret.trim().is_empty()
    {
        return secret;
    }

    let cache_path = std::path::PathBuf::from(
        std::env::var("OUT_DIR")
            .expect("OUT_DIR is set by cargo for every build script invocation"),
    )
    .join("seal-secret");

    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        let cached = cached.trim();
        if !cached.is_empty() {
            return cached.to_string();
        }
    }

    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the platform CSPRNG is available at build time");
    let secret = hex_encode(&bytes);
    // Best effort: if the cache write fails, the build still succeeds with a
    // secret that just won't survive to the next invocation.
    let _ = std::fs::write(&cache_path, &secret);
    secret
}

/// A minimal hex encoder so build.rs needs no extra crate beyond `getrandom`.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--always", "--tags", "--dirty=-dirty"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?;
    let version = version.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Keeps the reported version to a predictable, printable character set so a
/// stray ref name can never inject control characters into `--version` output.
fn sanitize_version(raw: &str) -> String {
    let mut version = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if version.is_empty() {
        version = env!("CARGO_PKG_VERSION").to_string();
    }
    version.truncate(128);
    version
}
