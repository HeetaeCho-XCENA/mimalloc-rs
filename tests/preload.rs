// SPDX-License-Identifier: MIT
//
// Convenience wrapper around `scripts/preload-check.sh` (the real end-to-end
// LD_PRELOAD proof that an unmodified C program uses our allocator).
//
// This test is `#[ignore]` by default. The script invokes `cargo rustc` to
// build the override cdylib; running that *inside* `cargo test` would be a
// nested-cargo invocation. Even though the script uses a separate
// `--target-dir target/preload` (so the build lock can't deadlock against the
// outer test build), we keep it ignored to (a) avoid surprising/expensive
// rebuilds during a normal `cargo test`, and (b) because the dedicated
// `preload` CI job is the authoritative gate. Run it explicitly with:
//
//     cargo test --features override --test preload -- --ignored
//
#![cfg(all(feature = "override", target_os = "linux"))]

use std::process::Command;

#[test]
#[ignore = "runs scripts/preload-check.sh (nested cargo build); gated by the `preload` CI job"]
fn preload_override_serves_unmodified_c_program() {
    // Locate the script relative to the crate root.
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/preload-check.sh");

    // Need bash to run the script at all; skip gracefully if it's missing.
    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("SKIP: `bash` not available");
        return;
    }

    let output = match Command::new("bash").arg(script).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP: could not spawn bash: {e}");
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The script prints "SKIP:" and exits 0 when there's no C compiler.
    if stdout.contains("SKIP:") {
        eprintln!("SKIP (from script): {}", stdout.trim());
        return;
    }

    assert!(
        output.status.success(),
        "preload-check.sh failed (exit {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status.code(),
    );
    assert!(
        stdout.contains("PRELOAD CHECK: PASS"),
        "preload-check.sh did not report PASS\n--- stdout ---\n{stdout}",
    );
}
