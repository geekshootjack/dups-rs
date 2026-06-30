use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod hashfile;
mod rename;
mod logging;

#[derive(Parser)]
#[command(name = "dups")]
#[command(about = "Rename files with xxHash3 suffix for deduplication", long_about = None)]
struct Cli {
    /// Input directory path
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Apply renaming (default is dry-run)
    #[arg(long)]
    apply: bool,

    /// Path to hashfile
    #[arg(long)]
    hashfile: Option<PathBuf>,

    /// Verify hashes before renaming
    #[arg(long)]
    verify: bool,

    /// Include all file types, not just videos
    #[arg(long)]
    all_files: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Undo a previous rename operation
    Undo {
        /// Path to the operation log
        log: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Undo { log }) => {
            logging::undo(&log)?;
        }
        None => {
            let path = cli
                .path
                .ok_or_else(|| anyhow::anyhow!("Path argument required"))?;

            let operation = rename::RenameOperation::new(
                path,
                cli.hashfile,
                cli.verify,
                !cli.apply, // dry_run is true when apply is false
                cli.all_files,
            )?;

            operation.execute()?;
        }
    }

    Ok(())
}
