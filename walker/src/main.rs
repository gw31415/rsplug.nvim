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

    for pattern in patterns {
        let compiled = match CompiledGlob::new(&pattern) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("invalid pattern `{pattern}`: {err}");
                continue;
            }
        };

        println!("# pattern: {pattern}");
        let mut rx = Walker::spawn(compiled);
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
        println!();
    }

    Ok(())
}
