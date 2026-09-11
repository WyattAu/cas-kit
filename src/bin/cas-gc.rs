// SPDX-License-Identifier: MIT OR Apache-2.0
//! `cas-gc` — mark/sweep garbage collection for cas-kit blob stores.
//!
//! The store's objects are opaque blobs (no internal references), so the
//! roots file you supply *is* the live set: `mark` validates it against
//! what is physically present, `sweep` removes everything else. See the
//! `cas_kit::gc` module documentation for the reference model, pack
//! handling, crash safety, and concurrency contract.
//!
//! ```text
//! cas-gc --root <DIR> mark <ROOTS_FILE>
//! cas-gc --root <DIR> sweep <ROOTS_FILE> --dry-run
//! cas-gc --root <DIR> sweep <ROOTS_FILE> --apply [--delete]
//! ```
//!
//! `ROOTS_FILE` holds one 64-char lowercase hex hash per line; blank
//! lines and `#` comments are ignored, and `-` reads stdin. Sweeps
//! default to trash mode (recoverable under `<root>/trash/`);
//! `--delete` unlinks permanently.
//!
//! Exit codes: 0 success (including a dry run that found garbage),
//! 1 operational failure.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cas_kit::gc::{self, SweepMode, SweepOptions};
use cas_kit::{BlobStore, Hash};
use clap::{ArgGroup, Args, Parser, Subcommand};

/// Parse arguments and run.
#[derive(Parser)]
#[command(
    name = "cas-gc",
    version,
    about = "Mark-sweep garbage collection for cas-kit blob stores",
    long_about = "Mark-sweep garbage collection for cas-kit blob stores.\n\n\
        cas-kit objects are opaque blobs: the roots file is the live set, \
        and a sweep removes everything in the store that it does not contain."
)]
struct Cli {
    /// Store root directory (the one containing objects/).
    #[arg(long)]
    root: PathBuf,

    /// Emit a single JSON object to stdout instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a roots file against the store and report what a sweep
    /// would reclaim. Changes nothing.
    Mark {
        /// File with one 64-char hex hash per line ('-' = stdin).
        roots_file: PathBuf,
    },
    /// Remove objects unreachable from the roots file.
    Sweep {
        /// File with one 64-char hex hash per line ('-' = stdin).
        roots_file: PathBuf,
        #[command(flatten)]
        mode: ModeArgs,
        /// Permanently delete garbage instead of moving it to `<root>/trash`.
        #[arg(long)]
        delete: bool,
    },
}

/// Exactly one of `--dry-run` / `--apply` is required.
#[derive(Args)]
#[command(group(
    ArgGroup::new("sweep-mode")
        .required(true)
        .args(&["dry_run", "apply"]),
))]
struct ModeArgs {
    /// Report what would be removed; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Perform the sweep.
    #[arg(long)]
    apply: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let store = BlobStore::new(&cli.root).map_err(|e| e.to_string())?;

    // mark and dry-run sweeps change nothing; --apply defaults to the
    // recoverable trash mode, --delete upgrades to unlink.
    let (command, roots_file, mode) = match &cli.command {
        Command::Mark { roots_file } => ("mark", roots_file, SweepMode::DryRun),
        Command::Sweep {
            roots_file,
            mode,
            delete,
        } => {
            let sweep_mode = if mode.dry_run {
                SweepMode::DryRun
            } else if *delete {
                SweepMode::Delete
            } else {
                SweepMode::Trash
            };
            ("sweep", roots_file, sweep_mode)
        }
    };

    let roots = parse_roots(roots_file)?;

    let live_set = gc::mark(&store, &roots).map_err(|e| e.to_string())?;
    let live: HashSet<Hash> = live_set.live.iter().copied().collect();
    let options = SweepOptions {
        mode,
        rewrite_partial_packs: true,
    };
    let report = gc::sweep(&store, &live, options).map_err(|e| e.to_string())?;

    if cli.json {
        print_json(command, &report, live_set.missing.len());
    } else {
        print_human(&cli.root, command, &report, &live_set);
    }
    Ok(())
}

/// Read newline-separated hex hashes; blank lines and `#` comments are
/// ignored.
fn parse_roots(path: &Path) -> Result<HashSet<Hash>, String> {
    let stdin_label = Path::new("-");
    let reader: Box<dyn BufRead> = if path == stdin_label {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        let file = fs::File::open(path)
            .map_err(|e| format!("cannot open roots file {}: {e}", path.display()))?;
        Box::new(BufReader::new(file))
    };

    let mut roots = HashSet::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("cannot read roots file: {e}"))?;
        let text = line.split('#').next().unwrap_or_default().trim();
        if text.is_empty() {
            continue;
        }
        let hash =
            Hash::from_hex(text).map_err(|e| format!("{}:{}: {e}", path.display(), index + 1))?;
        roots.insert(hash);
    }
    Ok(roots)
}

/// Human-readable report.
fn print_human(root: &Path, command: &str, report: &gc::SweepReport, live_set: &gc::LiveSet) {
    let plan = &report.plan;
    println!("cas-gc {command} — root: {}", root.display());
    println!(
        "scanned:  {:>9} objects (loose {}, packed {})",
        plan.scanned, plan.loose_present, plan.packed_present
    );
    println!(
        "live:     {:>9} (roots {}, missing {})",
        plan.live,
        live_set.roots,
        live_set.missing.len()
    );
    println!(
        "garbage:  {:>9} (loose {}, packed in {} pack{})",
        plan.garbage,
        plan.loose_garbage.len(),
        plan.packs_with_garbage,
        if plan.packs_with_garbage == 1 {
            ""
        } else {
            "s"
        }
    );
    println!(
        "reclaimable: {} (estimate; pack rewrites excluded)",
        human_bytes(plan.bytes_reclaimable)
    );
    if plan.unreadable_packs.is_empty() {
        println!("unreadable packs: 0");
    } else {
        println!(
            "unreadable packs: {} — left untouched:",
            plan.unreadable_packs.len()
        );
        for pack in &plan.unreadable_packs {
            println!("  {}", pack.display());
        }
    }

    if command == "mark" || !report.executed {
        println!("mode:     dry run (nothing changed)");
    } else {
        println!(
            "removed:  {} loose object{}, {} pack{} removed, {} pack{} rewritten",
            report.loose_removed,
            plural(report.loose_removed),
            report.packs_removed,
            plural(report.packs_removed),
            report.packs_rewritten,
            plural(report.packs_rewritten)
        );
        println!("reclaimed: {}", human_bytes(report.bytes_reclaimed));
        match &report.trash {
            Some(trash) => println!(
                "trash:    {} (restore by moving files back)",
                trash.display()
            ),
            None => println!("trash:    none (permanently deleted)"),
        }
    }
}

/// Compact machine-readable report with a fixed key order.
fn print_json(command: &str, report: &gc::SweepReport, missing_roots: usize) {
    let plan = &report.plan;
    let trash = report
        .trash
        .as_ref()
        .map(|p| format!("\"{}\"", json_escape(&p.to_string_lossy())))
        .unwrap_or_else(|| "null".to_string());
    println!(
        concat!(
            "{{\"command\":\"{}\",\"dry_run\":{},\"scanned\":{},\"live\":{},",
            "\"garbage\":{},\"missing_roots\":{},\"loose_garbage\":{},",
            "\"packed_garbage\":{},\"packs_with_garbage\":{},",
            "\"packs_removed\":{},\"packs_rewritten\":{},\"orphan_files\":{},",
            "\"unreadable_packs\":{},\"bytes_reclaimable\":{},",
            "\"bytes_reclaimed\":{},\"executed\":{},\"trash\":{}}}"
        ),
        command,
        report.mode == SweepMode::DryRun,
        plan.scanned,
        plan.live,
        plan.garbage,
        missing_roots,
        plan.loose_garbage.len(),
        plan.packed_garbage_objects,
        plan.packs_with_garbage,
        report.packs_removed,
        report.packs_rewritten,
        plan.orphan_files.len(),
        plan.unreadable_packs.len(),
        plan.bytes_reclaimable,
        report.bytes_reclaimed,
        report.executed,
        trash,
    );
}

/// JSON-escape a string (quotes, backslashes, control characters).
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// "" for exactly one, "s" otherwise.
fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Human-readable byte count (binary units).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
