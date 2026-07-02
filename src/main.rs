mod engine;
mod hashfile;
mod verify;

use clap::Parser;
use engine::{copy_all, FileEntry};
use hashfile::write_hashfile;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use verify::{verify_all, VerifyMode};

#[derive(Parser)]
#[command(name = "cvv", about = "File copy with integrity verification")]
struct Cli {
    /// Source file or directory
    source: PathBuf,

    /// One or more destination paths
    #[arg(required = true)]
    destinations: Vec<PathBuf>,

    /// Verification mode: source, target, or transfer
    #[arg(short, long)]
    mode: Option<String>,
}

fn parse_verify_mode(s: &str) -> Result<VerifyMode, String> {
    match s {
        "source" => Ok(VerifyMode::Source),
        "target" => Ok(VerifyMode::Target),
        "transfer" => Ok(VerifyMode::Transfer),
        _ => Err(format!("unknown mode: {s} (expected: source, target, transfer)")),
    }
}

fn prompt_verify_mode() -> Option<VerifyMode> {
    print!("Verify? [Y/n] ");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let input = input.trim().to_lowercase();
    if input == "n" || input == "no" {
        return None;
    }

    print!("Mode? [source/target] (default: target): ");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    let input = input.trim().to_lowercase();
    match input.as_str() {
        "source" => Some(VerifyMode::Source),
        "" | "target" => Some(VerifyMode::Target),
        other => {
            eprintln!("Unknown mode: {other}, using target");
            Some(VerifyMode::Target)
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let aborted = Arc::new(AtomicBool::new(false));

    {
        let aborted = aborted.clone();
        ctrlc::set_handler(move || {
            if aborted.load(Ordering::SeqCst) {
                process::exit(1);
            }
            eprintln!("\nInterrupted. Cleaning up...");
            aborted.store(true, Ordering::SeqCst);
        })
        .expect("failed to set Ctrl+C handler");
    }

    let source = &cli.source;
    if !source.exists() {
        eprintln!("Error: source not found: {}", source.display());
        process::exit(1);
    }

    for dest in &cli.destinations {
        if !dest.exists() {
            eprintln!("Error: destination not found: {}", dest.display());
            process::exit(1);
        }
    }

    let (files, dirs) = match FileEntry::discover(source) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error discovering files: {e}");
            process::exit(1);
        }
    };

    // Recreate every discovered directory at each destination, including empty ones,
    // which copy_all would otherwise never touch.
    let mut dir_create_failed = false;
    for dest in &cli.destinations {
        for rel_dir in &dirs {
            let target_dir = dest.join(rel_dir);
            if let Err(e) = fs::create_dir_all(&target_dir) {
                eprintln!("Error creating directory {}: {e}", target_dir.display());
                dir_create_failed = true;
            }
        }
    }
    if dir_create_failed {
        process::exit(1);
    }

    if files.is_empty() {
        if dirs.is_empty() {
            println!("No files to copy.");
        } else {
            println!("No files to copy ({} empty dir(s) recreated).", dirs.len());
        }
        return;
    }

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len();
    println!(
        "Found {} file(s), {:.1} GB total",
        total_files,
        total_bytes as f64 / 1_073_741_824.0
    );

    let mut results = copy_all(source, &files, &cli.destinations, &aborted);

    if aborted.load(Ordering::SeqCst) {
        eprintln!("Copy interrupted.");
        process::exit(1);
    }

    let copy_ok = results.iter().all(|r| r.success);
    if !copy_ok {
        eprintln!("Some files failed to copy.");
        process::exit(1);
    }

    // Determine verify mode
    let verify_mode = if let Some(ref mode_str) = cli.mode {
        match parse_verify_mode(mode_str) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("Error: {e}");
                process::exit(1);
            }
        }
    } else {
        prompt_verify_mode()
    };

    if verify_mode.is_none() {
        println!("Verification skipped.");
    }

    if let Some(mode) = verify_mode {
        println!(
            "\nStarting {} verification...",
            match mode {
                VerifyMode::Source => "source",
                VerifyMode::Target => "target",
                VerifyMode::Transfer => "transfer",
            }
        );
        let verify_ok = verify_all(&mut results, &cli.destinations, mode, &aborted);

        if aborted.load(Ordering::SeqCst) {
            eprintln!("Verification interrupted.");
            process::exit(1);
        }

        if !verify_ok {
            eprintln!("Verification FAILED.");
            process::exit(1);
        }
        println!("Verification passed.");
    }

    // Generate hash files; keep trying remaining destinations even if one fails.
    let mut hashfile_failed = false;
    for dest in &cli.destinations {
        match write_hashfile(source, &results, dest) {
            Ok(hashfile_path) => println!("Hash file: {}", hashfile_path.display()),
            Err(e) => {
                eprintln!("Error writing hash file for {}: {e}", dest.display());
                hashfile_failed = true;
            }
        }
    }
    if hashfile_failed {
        process::exit(1);
    }

    println!(
        "\nAll {} file(s) completed.",
        results.iter().filter(|r| r.success).count()
    );
}
