use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

use dups::{check, generate, logging, rename};

#[derive(Parser)]
#[command(name = "dups", version)]
#[command(about = "Rename files with xxHash3 suffix for deduplication", long_about = None)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    /// Input directory path (alias for `dups check <PATH>`)
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Read-only scan for duplicate filenames (never writes any file)
    Check {
        /// Directory to scan
        path: PathBuf,

        /// Include all file types, not just videos
        #[arg(long)]
        all_files: bool,
    },
    /// Plan/execute renames from .xxh3 manifests (or computed hashes with --only-dupes)
    Rename {
        /// Input directory path
        path: PathBuf,

        /// Only rename files that are members of duplicate-filename groups
        #[arg(long)]
        only_dupes: bool,

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
    },
    /// Scan directory and generate .xxh3 hashfile
    Generate {
        /// Directory to scan
        path: PathBuf,

        /// Output hashfile path (default: dups-manifest-TIMESTAMP.xxh3 in scanned directory)
        #[arg(long, short)]
        output: Option<PathBuf>,

        /// Include all file types, not just videos
        #[arg(long)]
        all_files: bool,
    },
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
        Some(Commands::Generate {
            path,
            output,
            all_files,
        }) => {
            generate::generate(&path, output.as_deref(), all_files)?;
        }
        Some(Commands::Check { path, all_files }) => {
            let groups = check::check(&path, all_files)?;
            std::process::exit(if groups > 0 { 1 } else { 0 });
        }
        Some(Commands::Rename {
            path,
            only_dupes,
            apply,
            hashfile,
            verify,
            all_files,
        }) => {
            let operation = rename::RenameOperation::new(
                path,
                hashfile,
                verify,
                !apply, // dry_run is true when apply is false
                all_files,
                only_dupes,
            )?;

            operation.execute()?;
        }
        None => match cli.path {
            // Bare `dups <PATH>` is an alias for `dups check <PATH>`.
            Some(path) => {
                let groups = check::check(&path, false)?;
                std::process::exit(if groups > 0 { 1 } else { 0 });
            }
            None => {
                Cli::command().print_help()?;
                println!();
            }
        },
    }

    Ok(())
}
