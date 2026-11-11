//! Reproducible history-search pressure fixture; never touches an existing repo.
use anyhow::{Result, ensure};
use gitbuddy::git::{CancellationToken, Repository};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let destination = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/history-bench-50000".into()),
    );
    let count = args
        .next()
        .map(|n| n.to_string_lossy().parse::<usize>())
        .transpose()?
        .unwrap_or(50000);
    let existing = args.next().is_some_and(|arg| arg == "--existing");
    ensure!(
        existing || !destination.exists(),
        "Benchmark destination already exists"
    );
    ensure!(
        (1000..=500000).contains(&count),
        "Choose 1,000–500,000 commits"
    );
    let repo = if existing {
        Repository::open(&destination)?
    } else {
        Repository::init(&destination)?
    };
    let generation = if existing {
        0.0
    } else {
        let raw = git2::Repository::open(&repo.root)?;
        let tree = raw.find_tree(raw.treebuilder(None)?.write()?)?;
        let signature = git2::Signature::now("Benchmark", "benchmark@example.invalid")?;
        let started = Instant::now();
        let mut parent = None;
        for i in 0..count {
            let previous = parent.map(|id| raw.find_commit(id)).transpose()?;
            parent = Some(raw.commit(
                Some("HEAD"),
                &signature,
                &signature,
                &format!("benchmark history {i}"),
                &tree,
                &previous.as_ref().into_iter().collect::<Vec<_>>(),
            )?);
        }
        started.elapsed().as_secs_f64()
    };
    let started = Instant::now();
    let snapshot = repo.snapshot(300)?;
    let snapshot_time = started.elapsed().as_secs_f64();
    ensure!(snapshot.commits.len() == 300, "Unexpected history window");
    let started = Instant::now();
    let first = repo.search_history("benchmark", &CancellationToken::default())?;
    let first_time = started.elapsed().as_secs_f64();
    ensure!(first.commits.len() == 1000, "Expected bounded result page");
    let started = Instant::now();
    let second =
        repo.search_history_next(first.next.as_ref().unwrap(), &CancellationToken::default())?;
    let second_time = started.elapsed().as_secs_f64();
    ensure!(second.commits.len() == 1000, "Expected second result page");
    let started = Instant::now();
    let full = repo.search_history("no-such-benchmark-text", &CancellationToken::default())?;
    let full_time = started.elapsed().as_secs_f64();
    ensure!(full.scanned == count.min(100000), "Full scan bound differs");
    let started = Instant::now();
    let cancel = CancellationToken::default();
    cancel.cancel();
    ensure!(
        repo.search_history("benchmark", &cancel).is_err(),
        "Cancelled search unexpectedly succeeded"
    );
    let cancel_time = started.elapsed().as_secs_f64();
    let token = CancellationToken::default();
    let worker_token = token.clone();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(25));
        worker_token.cancel();
    });
    let started = Instant::now();
    let cancelled = repo
        .search_history("benchmark", &token)
        .is_err_and(|e| gitbuddy::git::is_cancelled(&e));
    let mid_cancel_time = started.elapsed().as_secs_f64();
    worker.join().unwrap();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"commits":count,"generation_seconds":generation,"snapshot_300_seconds":snapshot_time,"first_1000_seconds":first_time,"second_1000_seconds":second_time,"scan_seconds":full_time,"scanned":full.scanned,"cancel_seconds":cancel_time,"midwalk_cancel_seconds":mid_cancel_time,"midwalk_cancelled":cancelled})
        )?
    );
    Ok(())
}
