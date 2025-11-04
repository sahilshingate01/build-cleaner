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
fn build_index(targets: &[Target]) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for (i, t) in targets.iter().enumerate() {
        m.insert(t.name.clone(), i); // later entries (config/include) win
    }
    m
}

fn parent_has_marker(parent: &Path, marker: &str) -> bool {
    if let Some(ext) = marker.strip_prefix("*.") {
        let suffix = format!(".{}", ext);
        if let Ok(rd) = fs::read_dir(parent) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    if n.ends_with(&suffix) {
                        return true;
                    }
                }
            }
        }
        false
    } else {
        parent.join(marker).exists()
    }
}

fn match_target<'a>(
    name: &str,
    dir: &Path,
    index: &HashMap<String, usize>,
    targets: &'a [Target],
) -> Option<&'a Target> {
    let t = &targets[*index.get(name)?];
    if t.markers.is_empty() {
        return Some(t);
    }
    let parent = dir.parent()?;
    if t.markers.iter().any(|m| parent_has_marker(parent, m)) {
        Some(t)
    } else {
        None
    }
}

struct Ctx<'a> {
    targets: &'a [Target],
    index: &'a HashMap<String, usize>,
    excludes: &'a HashSet<String>,
    follow_symlinks: bool,
}

#[derive(Clone)]
struct Sized {
    path: PathBuf,
    category: String,
    bytes: u64,
    files: u64,
}

struct Progress {
    scanned: AtomicU64,
    matches: Mutex<Vec<Sized>>,
    visited: Mutex<HashSet<PathBuf>>,
}

fn scan(dir: &Path, ctx: &Ctx, prog: &Progress, root: &Path) {
    prog.scanned.fetch_add(1, Ordering::Relaxed);
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return, // permission denied etc. — skip quietly
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if ctx.excludes.contains(name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if ft.is_symlink() {
            if !ctx.follow_symlinks {
                continue;
            }
            let resolved = match fs::canonicalize(&path) {
                Ok(path) if path.starts_with(root) => path,
                _ => continue,
            };
            if !resolved.is_dir() || !prog.visited.lock().unwrap().insert(resolved.clone()) {
                continue;
            }
            // A link is never itself a deletion candidate. Traverse its resolved,
            // in-root directory instead, so only real target directories match.
            subdirs.push(resolved);
            continue;
        }
        if !ft.is_dir() || !prog.visited.lock().unwrap().insert(path.clone()) {
            continue;
        }
        if let Some(t) = match_target(name.as_ref(), &path, ctx.index, ctx.targets) {
            let (bytes, files) = dir_size(&path, ctx.follow_symlinks);
            prog.matches.lock().unwrap().push(Sized {
                path,
                category: t.category.clone(),
                bytes,
                files,
            });
        } else {
            subdirs.push(path);
        }
    }
    subdirs.par_iter().for_each(|d| scan(d, ctx, prog, root));
}

fn dir_size(dir: &Path, follow_symlinks: bool) -> (u64, u64) {
    let mut visited = HashSet::new();
    dir_size_inner(dir, follow_symlinks, &mut visited)
}

fn dir_size_inner(dir: &Path, follow_symlinks: bool, visited: &mut HashSet<PathBuf>) -> (u64, u64) {
    let canonical = match fs::canonicalize(dir) {
        Ok(path) => path,
        Err(_) => return (0, 0),
    };
    if !visited.insert(canonical) {
        return (0, 0);
    }
    let mut bytes = 0u64;
    let mut files = 0u64;
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            if follow_symlinks {
                if let Ok(m) = fs::metadata(entry.path()) {
                    if m.is_file() {
                        bytes += m.len();
                        files += 1;
                    } else if m.is_dir() {
                        let (b, f) = dir_size_inner(&entry.path(), follow_symlinks, visited);
                        bytes += b;
                        files += f;
                    }
                }
            }
            continue;
        }
        if ft.is_dir() {
            let (b, f) = dir_size_inner(&entry.path(), follow_symlinks, visited);
            bytes += b;
            files += f;
        } else if let Ok(m) = entry.metadata() {
            bytes += m.len();
            files += 1;
        }
    }
    (bytes, files)
}

/// Pull any matches discovered since `consumed` out of the shared vec, releasing
/// the lock before the caller does I/O with them.
fn take_new(prog: &Progress, consumed: &mut usize) -> Vec<Sized> {
    let v = prog.matches.lock().unwrap();
    if v.len() > *consumed {
        let slice = v[*consumed..].to_vec();
        *consumed = v.len();
        slice
    } else {
        Vec::new()
    }
}

/// Human live view: stream each match to stdout as it arrives and keep a spinner
/// status line on stderr. Single-threaded, so parallel scan workers never garble.
fn human_reporter(
    prog: &Progress,
    done: &AtomicBool,
    start: Instant,
    min_size: u64,
    path_width: usize,
) {
    const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut frame = 0usize;
    let mut consumed = 0usize;
    let mut shown = 0u64;
    let mut running = 0u64;
    let mut header = false;

    loop {
        let newly = take_new(prog, &mut consumed);
        if newly.iter().any(|s| s.bytes >= min_size) {
            {
                let mut e = io::stderr().lock();
                let _ = write!(e, "\r\x1b[2K");
                let _ = e.flush();
            }
            let mut out = io::stdout().lock();
            for s in &newly {
                if s.bytes < min_size {
                    continue;
                }
                if !header {
                    let _ = writeln!(out, "{:>10}  {:<11}  PATH", "SIZE", "CATEGORY");
                    header = true;
                }
                let _ = writeln!(
                    out,
                    "{:>10}  {:<11}  {}",
                    human(s.bytes),
                    s.category,
                    elide(&s.path.to_string_lossy(), path_width)
                );
                shown += 1;
                running += s.bytes;
            }
            let _ = out.flush();
        }

        if done.load(Ordering::Relaxed) {
            let mut e = io::stderr().lock();
            let _ = write!(e, "\r\x1b[2K");
            let _ = e.flush();
            break;
        }

        let scanned = prog.scanned.load(Ordering::Relaxed);
        {
            let mut e = io::stderr().lock();
            let _ = write!(
                e,
                "\r\x1b[2K{}  {} dirs · {} matches · {} reclaimable · {}s",
                SPIN[frame % SPIN.len()],
                commafy(scanned),
                shown,
                human(running),
                start.elapsed().as_secs()
            );
            let _ = e.flush();
        }
        frame += 1;
        thread::sleep(Duration::from_millis(80));
    }
}

/// Machine live view: emit newline-delimited JSON events on stdout — a `match`
/// per reclaimable dir as it's sized, and a `progress` heartbeat each tick.
fn ndjson_reporter(prog: &Progress, done: &AtomicBool, start: Instant, min_size: u64) {
fn human(bytes: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < U.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", b, U[i])
}

/// Shorten an over-long path by keeping the head and tail with a `…` in the
/// middle (leaf name matters most, root gives context, deep middle is least
/// useful). `max == 0` disables truncation (piped / non-tty / JSON).
fn elide(s: &str, max: usize) -> String {
    if max == 0 {
        return s.to_string();
    }
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head; // bias toward the end
    let head_str: String = s.chars().take(head).collect();
    let tail_str: String = s.chars().skip(n - tail).collect();
    format!("{}…{}", head_str, tail_str)
}

fn commafy(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let len = b.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let num: f64 = num.parse().ok()?;
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TIB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((num * mult) as u64)
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn categories_sorted(sized: &[Sized]) -> Vec<(String, u64, usize)> {
    let mut agg: HashMap<&str, (u64, usize)> = HashMap::new();
    for s in sized {
        let e = agg.entry(s.category.as_str()).or_default();
        e.0 += s.bytes;
        e.1 += 1;
    }
    let mut cats: Vec<(String, u64, usize)> = agg
        .into_iter()
        .map(|(c, (b, n))| (c.to_string(), b, n))
        .collect();
    cats.sort_by_key(|c| Reverse(c.1));
    cats
}

fn print_targets(targets: &[Target]) {
    println!("# buildcleaner targets (reverse .gitignore)");
    println!("# format: name[:category[:marker,...]]  — markers gate generic names (checked in the parent dir)");
    for t in targets {
        if t.markers.is_empty() {
            println!("{}:{}", t.name, t.category);
        } else {
            println!("{}:{}:{}", t.name, t.category, t.markers.join(","));
        }
    }
}

fn print_json(sized: &[Sized], scanned: u64, secs: f64) {
    let total: u64 = sized.iter().map(|s| s.bytes).sum();
    let mut items = String::new();
    for (i, s) in sized.iter().enumerate() {
        if i > 0 {
            items.push(',');
        }
        items.push_str(&format!(
            "{{\"path\":\"{}\",\"category\":\"{}\",\"bytes\":{},\"files\":{}}}",
            json_escape(&s.path.to_string_lossy()),
            json_escape(&s.category),
            s.bytes,
            s.files
        ));
    }
    println!(
        "{{\"scanned_dirs\":{},\"elapsed_secs\":{:.3},\"total_bytes\":{},\"count\":{},\"items\":[{}]}}",
        scanned, secs, total, sized.len(), items
    );
}

/// Terminating `summary` event for the NDJSON stream.
fn stream_summary(sized: &[Sized], scanned: u64, secs: f64) {
fn print_breakdown(sized: &[Sized], delete: bool) {
    println!("\nBy category:");
    for (c, b, n) in categories_sorted(sized) {
        println!(
            "  {:<11} {:>10}  ({} dir{})",
            c,
            human(b),
            n,
            if n == 1 { "" } else { "s" }
        );
    }
    let total: u64 = sized.iter().map(|s| s.bytes).sum();
    println!(
        "\nTotal reclaimable: {} across {} directories.",
        human(total),
        sized.len()
    );
    if !delete {
        println!("Dry run — nothing deleted. Re-run with --delete to remove.");
    }
}

fn scanned_line(scanned: u64, roots: &[PathBuf], elapsed: Duration) {
    // Name the single root, but never imply a multi-root scan covered only the first.
    let scope = match roots {
        [only] => only.display().to_string(),
        [first, rest @ ..] => format!("{} and {} more", first.display(), rest.len()),
        [] => ".".to_string(),
    };
    println!(
        "Scanned {} directories under {} in {:.2}s.",
        commafy(scanned),
        scope,
        elapsed.as_secs_f64()
    );
}

/// Batch mode (non-tty / --progress never): full sorted table, then breakdown.
fn print_report(
    sized: &[Sized],
    scanned: u64,
    elapsed: Duration,
    roots: &[PathBuf],
    min_size: u64,
    delete: bool,
    path_width: usize,
) {
    scanned_line(scanned, roots, elapsed);
    if sized.is_empty() {
        println!(
            "No reclaimable directories found{}.",
            if min_size > 0 {
                " above the size threshold"
            } else {
                ""
            }
        );
        return;
    }
    println!();
    println!("{:>10}  {:<11}  PATH", "SIZE", "CATEGORY");
    for s in sized {
        println!(
            "{:>10}  {:<11}  {}",
            human(s.bytes),
            s.category,
            elide(&s.path.to_string_lossy(), path_width)
        );
    }
    print_breakdown(sized, delete);
}

/// Live mode: per-dir rows already streamed by the reporter, so just the tail.
fn print_live_summary(
    sized: &[Sized],
    scanned: u64,
    elapsed: Duration,
    roots: &[PathBuf],
    min_size: u64,
    delete: bool,
) {
    println!();
    scanned_line(scanned, roots, elapsed);
    if sized.is_empty() {
        println!(
            "No reclaimable directories found{}.",
            if min_size > 0 {
                " above the size threshold"
            } else {
                ""
            }
        );
        return;
    }
    print_breakdown(sized, delete);
}

/// Write one NDJSON line to stdout and flush immediately, so a UI driving us as a
/// subprocess sees each event in realtime rather than in a buffered burst.
fn emit(line: String) {

fn main() {
    let cli = Cli::parse();
    let targets = default_targets();
    let index = build_index(&targets);
    let ctx = Ctx {
        targets,
        index,
        min_size: 0,
        follow_symlinks: cli.follow_symlinks,
        exclude: HashSet::new(),
    };
    let prog = Progress::default();
    let root = cli.paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let start = Instant::now();
    scan(&root, &ctx, &prog, &root);
    let elapsed = start.elapsed();
    let matches = prog.matches.lock().unwrap();
    if cli.json {
        print_json(&matches, prog.scanned.load(Ordering::Relaxed), elapsed.as_secs_f64());
    } else {
        print_report(&matches, prog.scanned.load(Ordering::Relaxed), &[root], elapsed, false);
    }
}
