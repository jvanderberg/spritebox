//! Build script for the spritebox CLI.
//!
//! Locates the cross-compiled `spritebox-fsd` Linux binary so it can be
//! embedded into the CLI via `include_bytes!`. The CLI then `write_file`s
//! the embedded bytes to the sprite at share-launch time — no separate
//! daemon download or path lookup at runtime.
//!
//! Search order:
//! 1. `SPRITEBOX_FSD_BIN` env var (used by CI to point at a prebuilt
//!    artifact downloaded from a previous job).
//! 2. `target/x86_64-unknown-linux-gnu/release/spritebox-fsd` — the
//!    canonical local-dev path.
//!
//! If no daemon binary is found, we emit a helpful message and *do not*
//! fail the build. Instead a `daemon_embedded` cfg flag is left unset,
//! and the runtime surfaces a clear error if a `--share` is attempted.
//! This way `cargo build`, `cargo test`, etc. work out of the box on a
//! fresh checkout — the daemon is only required for actually running
//! a share.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SPRITEBOX_FSD_BIN");

    let candidates: Vec<PathBuf> = [
        std::env::var("SPRITEBOX_FSD_BIN").ok().map(PathBuf::from),
        Some(PathBuf::from(
            "target/x86_64-unknown-linux-gnu/release/spritebox-fsd",
        )),
    ]
    .into_iter()
    .flatten()
    .collect();

    let found = candidates.iter().find(|p| p.exists());

    match found {
        Some(rel_path) => {
            // Canonicalize so include_bytes! gets an absolute path
            // regardless of where the build is invoked from.
            let abs_path = match std::fs::canonicalize(rel_path) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "spritebox build.rs: failed to canonicalize daemon path \
                         {}: {e}. Daemon will not be embedded.",
                        rel_path.display()
                    );
                    return;
                }
            };
            // Re-run the build whenever the daemon binary changes.
            println!("cargo:rerun-if-changed={}", abs_path.display());
            // include_bytes! needs a literal path; we expose it via env!().
            println!(
                "cargo:rustc-env=SPRITEBOX_FSD_PATH={}",
                abs_path.display()
            );
            println!("cargo:rustc-cfg=daemon_embedded");
            println!(
                "cargo:warning=embedding spritebox-fsd from {} ({} bytes)",
                abs_path.display(),
                std::fs::metadata(&abs_path).map(|m| m.len()).unwrap_or(0)
            );
        }
        None => {
            // Soft-fail: build still succeeds, but --share will error
            // with a clear message at runtime.
            eprintln!();
            eprintln!(
                "spritebox build.rs: spritebox-fsd Linux binary not found"
            );
            eprintln!(
                "  search paths tried: {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            eprintln!("  the spritebox CLI will build, but --share will fail");
            eprintln!("  at runtime. To enable --share, build the daemon:");
            eprintln!();
            eprintln!("    ./scripts/build-daemon.sh");
            eprintln!();
            // Mark the daemon path as something that, when it later
            // appears, will trigger a rebuild.
            for candidate in &candidates {
                let _ = candidate;
                println!(
                    "cargo:rerun-if-changed={}",
                    Path::new(&candidate).display()
                );
            }
        }
    }
}
