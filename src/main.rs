use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use terminal_size::{terminal_size, Width};

const MAX_INPUT_PATHS: usize = 10_000;
const MAX_INPUT_LINE_BYTES: usize = 16 * 1024;

/// Find and reclaim space from build caches, dependency dirs, and generated output.
#[derive(Parser)]
#[command(name = "buildcleaner", version, about)]
struct Cli {
    /// Directories (or drive mounts) to scan. Defaults to the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Actually delete the found directories (default is a dry run)
    #[arg(long)]
    delete: bool,

    /// Skip the confirmation prompt when deleting
    #[arg(short = 'y', long)]
    yes: bool,

    /// Only report/act on directories at least this large (e.g. 100M, 1.5G)
    #[arg(long, value_name = "SIZE")]
    min_size: Option<String>,

    /// Follow symlinks (off by default for safety)
    #[arg(long)]
    follow_symlinks: bool,

    /// Live progress: auto (tty only), always, or never
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    progress: ProgressWhen,

    /// Extra directory names to treat as reclaimable (no marker required)
    #[arg(long, value_name = "NAME")]
    include: Vec<String>,

    /// Directory names to never match or descend into
    #[arg(long, value_name = "NAME")]
    exclude: Vec<String>,

    /// Load extra targets from a reverse-gitignore file (one `name[:category[:marker,...]]` per line)
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Print the built-in target list (the reverse .gitignore) and exit
    #[arg(long)]
    list_targets: bool,

    /// Output format: text (table), json (one blob), or stream-json (NDJSON events for live UIs)
    #[arg(long, value_name = "FMT")]
    output_format: Option<OutputFormat>,

    /// Operate on paths read from stdin instead of scanning. text = one path per line;
    /// stream-json = one {"path":"…"} object per line (pipe back the match events you chose)
    #[arg(long, value_name = "FMT")]
    input_format: Option<InputFormat>,

    /// Alias for --output-format json
    #[arg(long, hide = true)]
    json: bool,

    /// Alias for --output-format stream-json
    #[arg(long, hide = true, conflicts_with = "json")]
    stream: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProgressWhen {
    Auto,
    Always,
    Never,
}


fn main() {
    let cli = Cli::parse();
    println!("buildcleaner v{}", env!("CARGO_PKG_VERSION"));
    if cli.paths.is_empty() {
        println!("No scan paths specified. Defaulting to current directory.");
    }
}
