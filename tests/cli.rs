// Tests assert invariants directly; unwraps keep failures loud.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Integration tests for the `cas-gc` CLI binary: mark/sweep against a
//! real tempdir store, JSON and human output, exit codes, and the
//! dry-run/apply contract.

use std::collections::HashSet;
use std::fs;
use std::process::Command;

use cas_kit::{BlobStore, Hash};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cas-gc")
}

/// A store with two live blobs and two garbage blobs, plus a roots file
/// naming the live pair. Returns (tempdir, roots-file path).
fn fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;
    let mut live = Vec::new();
    for data in [b"cli live one".as_slice(), b"cli live two".as_slice()] {
        live.push(store.put_blob(data)?);
    }
    for data in [b"cli garbage one".as_slice(), b"cli garbage two".as_slice()] {
        store.put_blob(data)?;
    }
    assert_eq!(store.blob_count()?, 4);

    let roots_file = dir.path().join("roots.txt");
    let mut text = String::from("# generated fixture\n");
    for hash in &live {
        text.push_str(&hash.to_hex());
        text.push('\n');
    }
    fs::write(&roots_file, text)?;
    Ok((dir, roots_file))
}

fn garbage_paths(root: &std::path::Path, hashes: &[Hash]) -> Vec<std::path::PathBuf> {
    hashes
        .iter()
        .map(|h| {
            let hex = h.to_hex();
            root.join("objects").join(&hex[..2]).join(&hex[2..])
        })
        .collect()
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(bin()).args(args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn cli_mark_json_reports_live_and_garbage() -> TestResult {
    let (dir, roots_file) = fixture()?;
    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "mark",
        roots_file.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    // Fixed key order; parsed by substring so the test stays readable.
    for fragment in [
        "\"command\":\"mark\"",
        "\"dry_run\":true",
        "\"scanned\":4",
        "\"live\":2",
        "\"garbage\":2",
        "\"missing_roots\":0",
        "\"bytes_reclaimable\":",
        "\"executed\":false",
    ] {
        assert!(out.contains(fragment), "missing {fragment} in {out}");
    }
    Ok(())
}

#[test]
fn cli_mark_reports_missing_roots() -> TestResult {
    let (dir, roots_file) = fixture()?;
    let roots = fs::read_to_string(&roots_file)?;
    let with_missing = format!("{roots}{}  # absent from the store\n", "e".repeat(64));
    let extended = dir.path().join("roots-more.txt");
    fs::write(&extended, with_missing)?;

    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "mark",
        extended.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"missing_roots\":1"), "out: {out}");
    Ok(())
}

#[test]
fn cli_sweep_dry_run_then_apply_removes_only_garbage() -> TestResult {
    let (dir, roots_file) = fixture()?;

    // Identify the garbage on-disk files before running.
    let store = BlobStore::new_uncompressed(dir.path())?;
    let roots: HashSet<Hash> = fs::read_to_string(&roots_file)?
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(Hash::from_hex)
        .collect::<Result<_, _>>()?;
    let garbage: Vec<Hash> = store
        .list_blobs()?
        .into_iter()
        .filter(|h| !roots.contains(h))
        .collect();
    assert_eq!(garbage.len(), 2);
    let paths = garbage_paths(dir.path(), &garbage);
    drop(store);

    // Dry run: reports, deletes nothing.
    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "sweep",
        roots_file.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"dry_run\":true"), "out: {out}");
    assert!(out.contains("\"garbage\":2"), "out: {out}");
    assert!(paths.iter().all(|p| p.exists()), "dry run must not delete");

    // Apply (default: trash): garbage gone, live intact.
    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "sweep",
        roots_file.to_str().unwrap(),
        "--apply",
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"dry_run\":false"), "out: {out}");
    assert!(out.contains("\"executed\":true"), "out: {out}");
    assert!(
        !out.contains("\"bytes_reclaimed\":0,"),
        "reclaimed bytes must be reported: {out}"
    );
    assert!(
        out.contains("\"trash\":\""),
        "trash path must be reported: {out}"
    );
    assert!(
        paths.iter().all(|p| !p.exists()),
        "apply must delete garbage"
    );
    assert_eq!(BlobStore::new_uncompressed(dir.path())?.blob_count()?, 2);
    Ok(())
}

#[test]
fn cli_sweep_delete_mode_skips_trash() -> TestResult {
    let (dir, roots_file) = fixture()?;
    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "sweep",
        roots_file.to_str().unwrap(),
        "--apply",
        "--delete",
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"trash\":null"), "out: {out}");
    assert!(
        !dir.path().join("trash").exists(),
        "delete mode must not create trash"
    );
    Ok(())
}

#[test]
fn cli_sweep_requires_mode_flag() -> TestResult {
    let (dir, roots_file) = fixture()?;
    let (code, _out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "sweep",
        roots_file.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "clap usage error must exit 2: {err}");
    assert!(
        err.contains("--dry-run") && err.contains("--apply"),
        "err: {err}"
    );
    Ok(())
}

#[test]
fn cli_rejects_unreadable_roots_file() -> TestResult {
    let (dir, _roots_file) = fixture()?;
    let (code, _out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "mark",
        "/nonexistent/cas-gc-roots.txt",
    ]);
    assert_eq!(code, 1, "operational failure must exit 1: {err}");
    assert!(err.contains("cannot open roots file"), "err: {err}");
    Ok(())
}

#[test]
fn cli_rejects_malformed_hash_line() -> TestResult {
    let (dir, _roots_file) = fixture()?;
    let bad = dir.path().join("bad-roots.txt");
    fs::write(&bad, "not-a-hash\n")?;
    let (code, _out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "mark",
        bad.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "err: {err}");
    assert!(err.contains("invalid hash length"), "err: {err}");
    Ok(())
}

#[test]
fn cli_human_output_mentions_key_fields() -> TestResult {
    let (dir, roots_file) = fixture()?;
    let (code, out, err) = run(&[
        "--root",
        dir.path().to_str().unwrap(),
        "sweep",
        roots_file.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(code, 0, "stderr: {err}");
    for field in ["scanned:", "live:", "garbage:", "reclaimable:", "dry run"] {
        assert!(out.contains(field), "missing {field} in:\n{out}");
    }
    Ok(())
}
