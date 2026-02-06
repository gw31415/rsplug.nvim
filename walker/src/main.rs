use std::env;
use walker::compiled_glob::CompiledGlob;
use walker::walker::{EntryKind, Walker};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let patterns: Vec<String> = env::args().skip(1).collect();
    if patterns.is_empty() {
        eprintln!("Usage: cargo run -- '<glob pattern>' ['<glob pattern>' ...]");
        eprintln!("Example: cargo run -- '**/*.rs' 'src/**'");
        return Ok(());
    }

    let mut compiled = Vec::new();
    for pattern in &patterns {
        match CompiledGlob::new(pattern) {
            Ok(c) => compiled.push(c),
            Err(err) => {
                eprintln!("invalid pattern `{pattern}`: {err}");
                continue;
            }
        };
    }

    let merged = match CompiledGlob::merge_many(compiled) {
        Ok(merged) => merged,
        Err(err) => {
            eprintln!("no valid patterns to run: {err}");
            return Ok(());
        }
    };

    println!("# merged patterns:");
    for pattern in &patterns {
        println!("#   {pattern}");
    }

    let mut rx = Walker::spawn(merged);
    while let Some(msg) = rx.recv().await {
        match msg {
            Ok(event) => {
                let kind = match event.kind {
                    EntryKind::File => "file",
                    EntryKind::Dir => "dir",
                    EntryKind::Symlink => "symlink",
                    EntryKind::Other => "other",
                };
                println!("{kind}\t{}", event.path.display());
            }
            Err(err) => {
                eprintln!("walk error: {err}");
            }
        }
    }

    Ok(())
}
