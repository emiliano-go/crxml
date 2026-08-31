//! Predicate-first benchmark: sweeps predicate column position × selectivity.
//!
//! Usage:
//!   cargo run --release --example bench_predicate_first
//!   cargo run --release --features profile --example bench_predicate_first

use _crxml_core::xml::scanner::scan_chunk;
use rypipe_core::{ExecutionPlan, FieldType, FilterPredicate, TableBuilder};

// Profile counters from crxml-core (behind feature gate)
#[cfg(feature = "profile")]
use _crxml_core::xml::scanner::{reset_profile_counters, REJECTED_ROWS, SKIPPED_FIELDS};
use std::fs;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bench3(f: impl Fn()) -> f64 {
    let mut ts: Vec<f64> = (0..3)
        .map(|_| {
            let t0 = Instant::now();
            f();
            t0.elapsed().as_secs_f64()
        })
        .collect();
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts[1]
}

#[cfg(feature = "profile")]
fn reset_counters() {
    reset_profile_counters();
}

#[cfg(not(feature = "profile"))]
fn reset_counters() {}

#[cfg(feature = "profile")]
fn load_counters() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        REJECTED_ROWS.load(Ordering::Relaxed),
        SKIPPED_FIELDS.load(Ordering::Relaxed),
    )
}

#[cfg(not(feature = "profile"))]
fn load_counters() -> (u64, u64) {
    (0, 0)
}

fn make_plan(predicate: FilterPredicate) -> ExecutionPlan {
    let mut plan = ExecutionPlan::new();
    plan.filter = Some(predicate);
    // Declare all 11 fields as Str for consistency
    for k in 1..=11 {
        plan.field_types
            .insert(format!("Field{k}"), FieldType::String);
    }
    plan
}

fn eq_pred(field: &str, value: &str) -> FilterPredicate {
    FilterPredicate::Equal {
        field: field.to_string(),
        value: value.to_string(),
    }
}

fn ne_pred(field: &str, value: &str) -> FilterPredicate {
    FilterPredicate::NotEqual {
        field: field.to_string(),
        value: value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Load data
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test_533mb.xml".to_string());
    println!("Loading {path}...");
    let xml = fs::read(&path).expect("failed to read file");
    let mb = xml.len() as f64 / 1024.0 / 1024.0;

    // Count rows (unfiltered parse)
    let n_rows = {
        let mut tb = TableBuilder::with_capacity(500_000);
        scan_chunk(&xml, b"Details", &mut tb).unwrap();
        tb.finish().unwrap().num_rows()
    };
    println!("  {mb:.1} MB, {n_rows} rows, 11 fields/row");

    // ── (A) Unfiltered baseline ──────────────────────────────────────
    reset_counters();
    let t_unfiltered = bench3(|| {
        let mut tb = TableBuilder::with_capacity(n_rows);
        scan_chunk(&xml, b"Details", &mut tb).unwrap();
        let _ = tb.finish();
    });
    println!("\n{:=<78}", "");
    println!(
        "UNFILTERED  {t_unfiltered:.4}s  {:.0} MB/s",
        mb / t_unfiltered
    );
    println!("{:=<78}", "");

    // ── (B) Sweep: predicate column position × selectivity ──────────
    //
    //  Positions: Field1 (first), Field6 (middle), Field11 (last)
    //  Selectivities:
    //    0%   – reject everything  (FieldX == "nonexistent")
    //    ~6%  – accept ~6%        (FieldX == "01-00123")
    //    100% – accept everything  (FieldX != "nonexistent")

    let positions = [
        ("Field1", "first"),
        ("Field6", "middle"),
        ("Field11", "last"),
    ];

    let selectivities: &[(&str, Box<dyn Fn(&str) -> FilterPredicate>)] = &[
        ("0% (reject all)", Box::new(|f| eq_pred(f, "nonexistent"))),
        ("~6% (accept ~6%)", Box::new(|f| eq_pred(f, "01-00123"))),
        ("100% (accept all)", Box::new(|f| ne_pred(f, "nonexistent"))),
    ];

    println!("\n{:=<78}", "");
    println!(
        "{:<12} {:<22} {:>8} {:>10} {:>6} {:>8}",
        "Position", "Selectivity", "Time(s)", "MB/s", "vs Base", "Rows"
    );
    println!("{:-<78}", "");

    for (field_name, pos_label) in &positions {
        for (sel_label, make_pred) in selectivities {
            let plan = make_plan(make_pred(field_name));
            let expected_rows = if sel_label.contains("0%") {
                0
            } else if sel_label.contains("100%") {
                n_rows
            } else {
                n_rows / 16 // ~6%: 1 in 16 rows matches
            };

            reset_counters();
            let t = bench3(|| {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                let _ = tb.finish();
            });

            // Verification parse for row count
            let actual_rows = {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                tb.finish().unwrap().num_rows()
            };

            // Counters (from bench3 iterations, read before verification parse)
            let (rejected, skipped) = load_counters();
            let ratio = if rejected > 0 {
                skipped as f64 / rejected as f64
            } else {
                0.0
            };

            println!(
                "{:<12} {:<22} {:>8.4} {:>10.0} {:>5.2}x {:>8}",
                pos_label,
                sel_label,
                t,
                mb / t,
                t_unfiltered / t,
                actual_rows,
            );

            // Counter details (only meaningful with --features profile)
            if rejected > 0 {
                println!(
                    "             counters: REJECTED={rejected} SKIPPED={skipped} ratio={ratio:.1}"
                );
            }
        }
        println!();
    }

    println!("Done.");
}
