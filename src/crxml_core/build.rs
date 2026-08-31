use std::process::Command;

fn main() {
    // Re-run build.rs whenever git HEAD or any branch ref changes.
    // Without .git/refs/heads, cargo caches build.rs output across commits
    // that only touch .rs files — the anti-staleness SHA goes stale.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Get HEAD SHA
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Detect uncommitted edits — a SHA matching HEAD says nothing about
    // working-tree changes that aren't compiled into the binary.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let build_sha = if dirty { format!("{sha}-dirty") } else { sha };

    println!("cargo:rustc-env=CRXML_BUILD_SHA={build_sha}");
}
