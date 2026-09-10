//! A content-addressed file store built on `cas_kit::BlobStore`.
//!
//! Subcommands:
//!
//! ```text
//! demo                        self-contained tour: dedup + corruption detection
//! add  <ROOT> <FILE>...       store files, print their content addresses
//! get  <ROOT> <HASH> <DEST>   fetch + verify a blob, write it to DEST
//! list <ROOT>                 list loose and packed addresses
//! ```
//!
//! Every address printed is the BLAKE3 hash of the content: the same file
//! added twice (or from two paths) maps to the same address and is stored
//! once, and `get` re-hashes what it reads before handing it back.
//!
//! Run: `cargo run --example file_store -- demo`

use std::collections::HashSet;
use std::path::Path;
use std::process::ExitCode;

use cas_kit::{BlobStore, CasError, Hash};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("demo") | None => demo(),
        Some("add") => match (args.get(1), args.get(2)) {
            (Some(root), Some(_)) => add(root, &args[2..]),
            _ => usage("add <ROOT> <FILE>..."),
        },
        Some("get") => match (args.get(1), args.get(2), args.get(3)) {
            (Some(root), Some(hash), Some(dest)) => get(root, hash, dest),
            _ => usage("get <ROOT> <HASH> <DEST>"),
        },
        Some("list") => match args.get(1) {
            Some(root) => list(root),
            None => usage("list <ROOT>"),
        },
        Some(other) => usage_other(other),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage(cmd: &str) -> Result<(), CasError> {
    eprintln!("missing arguments — usage: file_store {cmd}");
    Ok(())
}

fn usage_other(cmd: &str) -> Result<(), CasError> {
    eprintln!("unknown subcommand {cmd:?} — expected demo, add, get, or list");
    Ok(())
}

/// The full tour: dedup on write, verified read, and a corrupted blob
/// failing loudly instead of returning wrong bytes.
fn demo() -> Result<(), CasError> {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new(dir.path())?;

    let content = b"The quick brown fox jumps over the lazy dog.";
    let first = store.put_blob(content)?;
    let second = store.put_blob(content)?;
    println!("first  put -> {first}");
    println!("second put -> {second}");
    println!(
        "same address = same content; blobs on disk: {}",
        store.blob_count()?
    );

    let read_back = store.get_blob(&first)?;
    println!("verified read: {:?}", String::from_utf8_lossy(&read_back));

    // Flip one stored byte behind the store's back. Read through a freshly
    // reopened store: the first read populated the in-memory blob cache,
    // which would otherwise serve the pristine copy and mask the corruption.
    corrupt_loose_blob(&store, &first)?;
    let reopened = BlobStore::new(dir.path())?;
    match reopened.get_blob(&first) {
        Err(CasError::HashMismatch { expected, actual }) => {
            println!("corruption detected: address {expected} held bytes hashing to {actual}");
            println!("verify-on-read failed closed — exactly the guarantee you want");
        }
        Err(other) => println!("corruption detected: {other}"),
        Ok(_) => println!("WARNING: corrupted blob was served without complaint"),
    }
    Ok(())
}

fn add(root: &str, files: &[String]) -> Result<(), CasError> {
    let store = BlobStore::new(root)?;
    for path in files {
        let data = std::fs::read(path)?;
        let hash = cas_kit::hash_bytes(&data);
        let duplicate = store.has_blob(&hash);
        let stored = store.put_blob(&data)?;
        let size_note = if duplicate {
            "stored once (deduplicated)"
        } else {
            "new"
        };
        println!("{stored}  <- {path}  ({} bytes, {size_note})", data.len());
    }
    Ok(())
}

fn get(root: &str, hex: &str, dest: &str) -> Result<(), CasError> {
    let store = BlobStore::new(root)?;
    let hash = Hash::from_hex(hex).map_err(|err| CasError::InvalidPath(err.to_string()))?;
    let data = store.get_blob(&hash)?;
    std::fs::write(dest, &data)?;
    println!(
        "{hash} -> {dest} ({} bytes, hash verified on read)",
        data.len()
    );
    Ok(())
}

fn list(root: &str) -> Result<(), CasError> {
    let store = BlobStore::new(root)?;
    let mut all: HashSet<Hash> = store.list_blobs()?.into_iter().collect();
    all.extend(store.list_blobs_packed()?);
    for hash in all {
        println!("{hash}");
    }
    Ok(())
}

/// Locate and mangle a loose blob's on-disk bytes (demo helper). The path
/// layout is `objects/<first 2 hex>/<remaining 62 hex>`.
fn corrupt_loose_blob(store: &BlobStore, hash: &Hash) -> std::io::Result<()> {
    let hex = hash.to_hex();
    let path = store.objects_dir().join(&hex[..2]).join(&hex[2..]);
    overwrite_first_byte(&path)
}

fn overwrite_first_byte(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut bytes = std::fs::read(path)?;
    if let Some(first) = bytes.first_mut() {
        *first = first.wrapping_add(1);
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(&bytes)
}
