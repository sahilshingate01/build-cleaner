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

/// How results leave the engine. `text` is for humans; `json` and `stream-json`
/// are the machine contract a UI drives.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

/// How the engine receives *what to work on* when not scanning a path.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum InputFormat {
    Text,
    StreamJson,
}

/// `--output-format` wins; the legacy `--json` / `--stream` flags are kept as aliases.
fn effective_output(cli: &Cli) -> OutputFormat {
    if let Some(f) = cli.output_format {
        f
    } else if cli.stream {
        OutputFormat::StreamJson
    } else if cli.json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    }
}

#[derive(Clone)]
struct Target {
    name: String,
    category: String,
    /// If non-empty, the dir only matches when the PARENT contains one of these
    /// marker files. Entries beginning with `*.` match by extension.
    markers: Vec<String>,
}

impl Target {
    fn new(name: &str, category: &str, markers: &[&str]) -> Self {
        Target {
            name: name.to_string(),
            category: category.to_string(),
            markers: markers.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// The curated list: directories that are safe to delete because a build system
/// or package manager can regenerate them. Generic names are gated behind a
/// marker file so we never touch a hand-authored `dist/`, `build/`, `target/`...
fn default_targets() -> Vec<Target> {
    let t = |n: &str, c: &str, m: &[&str]| Target::new(n, c, m);
    vec![
        // --- JS / TS ecosystems ---
        t("node_modules", "node", &[]),
        t(".next", "next", &[]),
        t(".nuxt", "nuxt", &[]),
        t(".output", "nitro", &[]),
        t(".turbo", "turbo", &[]),
        t(".svelte-kit", "svelte", &[]),
        t(".angular", "angular", &[]),
        t(".astro", "astro", &[]),
        t(".parcel-cache", "parcel", &[]),
        t(".vite", "vite", &[]),
        t(".expo", "expo", &[]),
        t(".serverless", "serverless", &[]),
        t("bower_components", "bower", &[]),
        t("jspm_packages", "jspm", &[]),
        t("dist", "dist", &["package.json"]),
        t("build", "build", &["package.json"]),
        t("coverage", "coverage", &["package.json"]),
        t(".nyc_output", "coverage", &[]),
        // --- generic caches (rebuildable) ---
        t(".cache", "cache", &[]),
        // --- Rust / Zig / native ---
        t("target", "target", &["Cargo.toml", "pom.xml"]),
        t("zig-cache", "zig", &[]),
        t(".zig-cache", "zig", &[]),
        t("zig-out", "zig", &[]),
        t("CMakeFiles", "cmake", &[]),
        t("cmake-build-debug", "cmake", &[]),
        t("cmake-build-release", "cmake", &[]),
        t("obj", "dotnet", &["*.csproj", "*.fsproj", "*.vbproj"]),
        // --- JVM ---
        t(".gradle", "gradle", &[]),
        // --- Python ---
        t("__pycache__", "python", &[]),
        t(".pytest_cache", "python", &[]),
        t(".mypy_cache", "python", &[]),
        t(".ruff_cache", "python", &[]),
        t(".tox", "python", &[]),
        // --- Dart / Flutter ---
        t(".dart_tool", "dart", &[]),
        // --- PHP / Go ---
        t("vendor", "vendor", &["composer.json", "go.mod"]),
        // --- Apple ---
        t("Pods", "cocoapods", &["Podfile"]),
        t("DerivedData", "xcode", &[]),
        // --- Elixir / Haskell / Elm ---
        t("_build", "elixir", &["mix.exs"]),
        t("deps", "elixir", &["mix.exs"]),
        t("dist-newstyle", "haskell", &[]),
        t(".stack-work", "haskell", &[]),
        t("elm-stuff", "elm", &[]),
        // --- Infra ---
        t(".terraform", "terraform", &[]),
    ]
}

fn parse_config(path: &Path) -> io::Result<Vec<Target>> {

fn main() {
    let cli = Cli::parse();
    let targets = default_targets();
    if cli.list_targets {
        for t in &targets {
            println!("{}:{}", t.name, t.category);
        }
        return;
    }
    println!("buildcleaner: loaded {} default targets", targets.len());
}
