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
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ':');
        let name = parts.next().unwrap().trim();
        if name.is_empty() {
            continue;
        }
        let category = parts
            .next()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(name);
        let markers: Vec<String> = parts
            .next()
            .map(|s| {
                s.split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        out.push(Target {
            name: name.to_string(),
            category: category.to_string(),
            markers,
        });
    }
    Ok(out)
}

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
    let mut consumed = 0usize;
    let mut shown = 0u64;
    let mut running = 0u64;

    loop {
        let newly = take_new(prog, &mut consumed);
        if !newly.is_empty() {
            let mut out = io::stdout().lock();
            for s in &newly {
                if s.bytes < min_size {
                    continue;
                }
                let _ = writeln!(
                    out,
                    "{{\"event\":\"match\",\"path\":\"{}\",\"category\":\"{}\",\"bytes\":{},\"files\":{}}}",
                    json_escape(&s.path.to_string_lossy()),
                    json_escape(&s.category),
                    s.bytes,
                    s.files
                );
                shown += 1;
                running += s.bytes;
            }
            let _ = out.flush();
        }

        if done.load(Ordering::Relaxed) {
            break;
        }

        let scanned = prog.scanned.load(Ordering::Relaxed);
        {
            let mut out = io::stdout().lock();
            let _ = writeln!(
                out,
                "{{\"event\":\"progress\",\"scanned\":{},\"matches\":{},\"bytes\":{},\"elapsed_ms\":{}}}",
                scanned,
                shown,
                running,
                start.elapsed().as_millis()
            );
            let _ = out.flush();
        }
        thread::sleep(Duration::from_millis(80));
    }
}

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
    let total: u64 = sized.iter().map(|s| s.bytes).sum();
    let mut cats = String::new();
    for (i, (c, b, n)) in categories_sorted(sized).into_iter().enumerate() {
        if i > 0 {
            cats.push(',');
        }
        cats.push_str(&format!(
            "{{\"category\":\"{}\",\"bytes\":{},\"dirs\":{}}}",
            json_escape(&c),
            b,
            n
        ));
    }
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "{{\"event\":\"summary\",\"scanned\":{},\"elapsed_secs\":{:.3},\"total_bytes\":{},\"count\":{},\"categories\":[{}]}}",
        scanned, secs, total, sized.len(), cats
    );
    let _ = out.flush();
}

/// By-category rollup, grand total, and dry-run note. Shared by both report modes.
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
    let mut o = io::stdout().lock();
    let _ = writeln!(o, "{}", line);
    let _ = o.flush();
}

/// Extract the string value of the top-level `"path"` key from one JSON line.
/// Lines without a path (e.g. progress/summary events piped back verbatim) yield
/// None and are skipped by the caller.
fn json_extract_path(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("path")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Read the paths to operate on from stdin, one per line (or one JSON object per line).
fn read_input_paths_from<R: BufRead>(reader: R, fmt: InputFormat) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => return Err(format!("failed reading stdin: {e}")),
        };
        if line.len() > MAX_INPUT_LINE_BYTES {
            return Err(format!(
                "stdin line exceeds the {} byte limit",
                MAX_INPUT_LINE_BYTES
            ));
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if paths.len() == MAX_INPUT_PATHS {
            return Err(format!("stdin exceeds the {} path limit", MAX_INPUT_PATHS));
        }
        match fmt {
            InputFormat::Text => paths.push(PathBuf::from(t)),
            InputFormat::StreamJson => {
                if let Some(p) = json_extract_path(t) {
                    paths.push(PathBuf::from(p));
                }
            }
        }
    }
    Ok(paths)
}

fn read_input_paths(fmt: InputFormat) -> Result<Vec<PathBuf>, String> {
    read_input_paths_from(io::stdin().lock(), fmt)
}

/// Re-check that a caller-supplied path really is a reclaimable target before we
/// size or delete it — the exact rule the scanner uses (known name + marker gate),
/// plus "exists, is a real directory, not a symlink, not excluded". This is the
/// safety net that stops a buggy UI handing us an arbitrary directory to delete.
fn revalidate(path: &Path, ctx: &Ctx) -> Result<String, &'static str> {
    let meta = fs::symlink_metadata(path).map_err(|_| "path not found")?;
    if meta.file_type().is_symlink() {
        return Err("refusing to operate on a symlink");
    }
    if !meta.is_dir() {
        return Err("not a directory");
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("unreadable directory name")?;
    if ctx.excludes.contains(name) {
        return Err("excluded by --exclude");
    }
    match match_target(name, path, ctx.index, ctx.targets) {
        Some(t) => Ok(t.category.clone()),
        None => Err("not a recognized reclaimable target"),
    }
}

/// Collapse a canonicalized root list to the minimal set that covers the same
/// trees: drop exact duplicates and any root nested inside another. Keeping a
/// nested root would LOSE matches — the ancestor's walk skips already-visited
/// directories before match-testing them, so the nested dir itself would never
/// be reported. Comparison is component-wise, so `/proj-two` is not "inside"
/// `/proj`.
fn prune_nested_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    // Sort so every ancestor is seen before the descendants it covers.
    roots.sort();
    let mut kept: Vec<PathBuf> = Vec::new();
    for root in roots {
        if kept.iter().any(|k| root.starts_with(k)) {
            continue;
        }
        kept.push(root);
    }
    kept
}

fn revalidate_within_roots(
    path: &Path,
    roots: &[PathBuf],
    ctx: &Ctx,
) -> Result<(PathBuf, String), String> {
    let category = revalidate(path, ctx).map_err(str::to_owned)?;
    let canonical = fs::canonicalize(path).map_err(|e| format!("cannot resolve path: {e}"))?;
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err("path resolves outside the scanned roots".to_string());
    }
    Ok((canonical, category))
}

/// Input mode: measure — or, with `delete`, remove — an explicit list of paths.
/// Every path is re-validated first; rejects never touch the filesystem.
fn run_input_mode(paths: &[PathBuf], ctx: &Ctx, out: OutputFormat, delete: bool, min_size: u64) {
    let stream = out == OutputFormat::StreamJson;

    if stream {
        emit(format!(
            "{{\"event\":\"start\",\"input\":true,\"delete\":{},\"count\":{}}}",
            delete,
            paths.len()
        ));
    }

    let mut valid: Vec<Sized> = Vec::new();
    let mut freed = 0u64;
    let mut deleted = 0u64;
    let mut failed = 0u64;

    for p in paths {
        match revalidate(p, ctx) {
            Ok(category) => {
                let (bytes, files) = dir_size(p, ctx.follow_symlinks);
                if bytes < min_size {
                    continue;
                }
                if delete {
                    match fs::remove_dir_all(p) {
                        Ok(_) => {
                            freed += bytes;
                            deleted += 1;
                            if stream {
                                emit(format!(
                                    "{{\"event\":\"deleted\",\"path\":\"{}\",\"bytes\":{}}}",
                                    json_escape(&p.to_string_lossy()),
                                    bytes
                                ));
                            }
                        }
                        Err(e) => {
                            failed += 1;
                            if stream {
                                emit(format!(
                                    "{{\"event\":\"delete_error\",\"path\":\"{}\",\"error\":\"{}\"}}",
                                    json_escape(&p.to_string_lossy()),
                                    json_escape(&e.to_string())
                                ));
                            } else {
                                eprintln!("failed to delete {}: {}", p.display(), e);
                            }
                        }
                    }
                } else {
                    if stream {
                        emit(format!(
                            "{{\"event\":\"match\",\"path\":\"{}\",\"category\":\"{}\",\"bytes\":{},\"files\":{}}}",
                            json_escape(&p.to_string_lossy()),
                            json_escape(&category),
                            bytes,
                            files
                        ));
                    }
                    valid.push(Sized {
                        path: p.clone(),
                        category,
                        bytes,
                        files,
                    });
                }
            }
            Err(reason) => {
                failed += 1;
                if stream {
                    emit(format!(
                        "{{\"event\":\"{}\",\"path\":\"{}\",\"error\":\"{}\"}}",
                        if delete { "delete_error" } else { "rejected" },
                        json_escape(&p.to_string_lossy()),
                        json_escape(reason)
                    ));
                } else {
                    eprintln!("skipped {}: {}", p.display(), reason);
                }
            }
        }
    }

    if delete {
        match out {
            OutputFormat::StreamJson => emit(format!(
                "{{\"event\":\"delete_summary\",\"deleted\":{},\"failed\":{},\"freed_bytes\":{}}}",
                deleted, failed, freed
            )),
            OutputFormat::Json => println!(
                "{{\"deleted\":{},\"failed\":{},\"freed_bytes\":{}}}",
                deleted, failed, freed
            ),
            OutputFormat::Text => println!(
                "Deleted {} director{}, freed {}{}.",
                deleted,
                if deleted == 1 { "y" } else { "ies" },
                human(freed),
                if failed > 0 {
                    format!(" ({} skipped/failed)", failed)
                } else {
                    String::new()
                }
            ),
        }
    } else {
        valid.sort_by_key(|s| Reverse(s.bytes));
        match out {
            OutputFormat::StreamJson => stream_summary(&valid, paths.len() as u64, 0.0),
            OutputFormat::Json => print_json(&valid, paths.len() as u64, 0.0),
            OutputFormat::Text => {
                if valid.is_empty() {
                    println!("No reclaimable directories in the provided list.");
                } else {
                    println!("{:>10}  {:<11}  PATH", "SIZE", "CATEGORY");
                    for s in &valid {
                        println!(
                            "{:>10}  {:<11}  {}",
                            human(s.bytes),
                            s.category,
                            s.path.display()
                        );
                    }
                    print_breakdown(&valid, false);
                }
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();

    let mut targets = default_targets();
    if let Some(cfg) = &cli.config {
        match parse_config(cfg) {
            Ok(mut extra) => targets.append(&mut extra),
            Err(e) => {
                eprintln!("error reading config {}: {}", cfg.display(), e);
                std::process::exit(2);
            }
        }
    }
    for name in &cli.include {
        targets.push(Target {
            name: name.clone(),
            category: "include".to_string(),
            markers: vec![],
        });
    }
    let index = build_index(&targets);

    if cli.list_targets {
        print_targets(&targets);
        return;
    }

    let out = effective_output(&cli);

    // A delete we can't confirm interactively (machine output, stdin-fed paths, or a
    // non-tty) must be explicitly opted into with --yes.
    let can_prompt =
        out == OutputFormat::Text && cli.input_format.is_none() && io::stdin().is_terminal();
    if cli.delete && !cli.yes && !can_prompt {
        eprintln!("--delete without an interactive terminal requires --yes");
        std::process::exit(2);
    }

    let min_size = match &cli.min_size {
        Some(s) => match parse_size(s) {
            Some(v) => v,
            None => {
                eprintln!("invalid --min-size: {}", s);
                std::process::exit(2);
            }
        },
        None => 0,
    };

    let excludes: HashSet<String> = cli.exclude.iter().cloned().collect();
    let ctx = Ctx {
        targets: &targets,
        index: &index,
        excludes: &excludes,
        follow_symlinks: cli.follow_symlinks,
    };

    // ---- Input mode: operate on an explicit list of paths from stdin ----
    if let Some(fmt) = cli.input_format {
        let paths = match read_input_paths(fmt) {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("invalid stdin input: {e}");
                std::process::exit(2);
            }
        };
        run_input_mode(&paths, &ctx, out, cli.delete, min_size);
        return;
    }

    // ---- Scan mode: walk one or more trees ----
    let requested = if cli.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        cli.paths.clone()
    };

    // Canonicalize every requested root, warning about (and skipping) the ones we
    // can't read, then collapse duplicates and nested roots so an overlapping
    // drop scans each tree exactly once.
    let mut resolved: Vec<PathBuf> = Vec::new();
    for p in &requested {
        match fs::canonicalize(p) {
            Ok(c) => resolved.push(c),
            Err(e) => eprintln!("cannot access {}: {}", p.display(), e),
        }
    }
    if resolved.is_empty() {
        std::process::exit(2);
    }
    let roots = prune_nested_roots(resolved);
    // A representative root for the human-facing text reporters (which describe a
    // single "under <root>" scope). The stream-json path the UI drives ignores it.
    let display_root = roots[0].clone();

    if cli.delete && out == OutputFormat::Json {
        eprintln!(
            "--output-format json with --delete is ambiguous; use --output-format stream-json"
        );
        std::process::exit(2);
    }

    let live = out == OutputFormat::Text
        && match cli.progress {
            ProgressWhen::Always => true,
            ProgressWhen::Never => false,
            ProgressWhen::Auto => io::stderr().is_terminal(),
        };

    // Path column budget: table prefix is 25 cols (size 10 + gap 2 + cat 11 + gap 2).
    // TTY width wins; else honor COLUMNS (an app/pipe can request a width); else 0 = full paths.
    let path_width = terminal_size()
        .map(|(Width(w), _)| w as usize)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        })
        .map(|w| w.saturating_sub(25).max(24))
        .unwrap_or(0);

    let prog = Progress {
        scanned: AtomicU64::new(0),
        matches: Mutex::new(Vec::new()),
        // Pre-seed the visited set with every root so overlapping/nested roots
        // don't re-walk shared subtrees or emit a directory twice.
        visited: Mutex::new(roots.iter().cloned().collect()),
    };
    let done = AtomicBool::new(false);

    if out == OutputFormat::StreamJson {
        let mut o = io::stdout().lock();
        let _ = writeln!(
            o,
            "{{\"event\":\"start\",\"root\":\"{}\",\"roots\":{},\"min_bytes\":{}}}",
            json_escape(&display_root.to_string_lossy()),
            roots.len(),
            min_size
        );
        let _ = o.flush();
    }

    let start = Instant::now();
    thread::scope(|s| {
        if live {
            let pr = &prog;
            let dn = &done;
            s.spawn(move || human_reporter(pr, dn, start, min_size, path_width));
        } else if out == OutputFormat::StreamJson {
            let pr = &prog;
            let dn = &done;
            s.spawn(move || ndjson_reporter(pr, dn, start, min_size));
        }
        for root in &roots {
            scan(root, &ctx, &prog, root);
        }
        done.store(true, Ordering::Relaxed);
    });
    let elapsed = start.elapsed();
    let scanned = prog.scanned.load(Ordering::Relaxed);

    let mut sized: Vec<Sized> = prog
        .matches
        .into_inner()
        .unwrap()
        .into_iter()
        .filter(|s| s.bytes >= min_size)
        .collect();
    sized.sort_by_key(|s| Reverse(s.bytes));

    match out {
        OutputFormat::StreamJson => stream_summary(&sized, scanned, elapsed.as_secs_f64()),
        OutputFormat::Json => print_json(&sized, scanned, elapsed.as_secs_f64()),
        OutputFormat::Text => {
            if live {
                print_live_summary(&sized, scanned, elapsed, &roots, min_size, cli.delete);
            } else {
                print_report(
                    &sized, scanned, elapsed, &roots, min_size, cli.delete, path_width,
                );
            }
        }
    }

    if cli.delete && !sized.is_empty() {
        match out {
            OutputFormat::Text => delete_interactive(&sized, cli.yes, &roots, &ctx),
            OutputFormat::StreamJson => delete_stream(&sized, &roots, &ctx),
            OutputFormat::Json => unreachable!("json delete is rejected before scanning"),
        }
    }
}

fn delete_stream(sized: &[Sized], roots: &[PathBuf], ctx: &Ctx) {
    let mut freed = 0u64;
    let mut deleted = 0u64;
    let mut failed = 0u64;
    {
        let mut out = io::stdout().lock();
        for s in sized {
            let deletion_path = match revalidate_within_roots(&s.path, roots, ctx) {
                Ok((path, _)) => path,
                Err(e) => {
                    failed += 1;
                    let _ = writeln!(
                        out,
                        "{{\"event\":\"delete_error\",\"path\":\"{}\",\"error\":\"{}\"}}",
                        json_escape(&s.path.to_string_lossy()),
                        json_escape(&e)
                    );
                    continue;
                }
            };
            match fs::remove_dir_all(&deletion_path) {
                Ok(_) => {
                    freed += s.bytes;
                    deleted += 1;
                    let _ = writeln!(
                        out,
                        "{{\"event\":\"deleted\",\"path\":\"{}\",\"bytes\":{}}}",
                        json_escape(&s.path.to_string_lossy()),
                        s.bytes
                    );
                }
                Err(e) => {
                    failed += 1;
                    let _ = writeln!(
                        out,
                        "{{\"event\":\"delete_error\",\"path\":\"{}\",\"error\":\"{}\"}}",
                        json_escape(&s.path.to_string_lossy()),
                        json_escape(&e.to_string())
                    );
                }
            }
            let _ = out.flush();
        }
        let _ = writeln!(
            out,
            "{{\"event\":\"delete_summary\",\"deleted\":{},\"failed\":{},\"freed_bytes\":{}}}",
            deleted, failed, freed
        );
        let _ = out.flush();
    }
}

fn delete_interactive(sized: &[Sized], yes: bool, roots: &[PathBuf], ctx: &Ctx) {
    let total: u64 = sized.iter().map(|s| s.bytes).sum();
    if !yes {
        print!(
            "\nDelete these {} directories and free {}? [y/N] ",
            sized.len(),
            human(total)
        );
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line).ok();
        let ans = line.trim().to_lowercase();
        if ans != "y" && ans != "yes" {
            println!("Aborted. Nothing deleted.");
            return;
        }
    }
    let mut freed = 0u64;
    let mut failures = 0u64;
    for s in sized {
        let deletion_path = match revalidate_within_roots(&s.path, roots, ctx) {
            Ok((path, _)) => path,
            Err(e) => {
                failures += 1;
                eprintln!("skipped {}: {}", s.path.display(), e);
                continue;
            }
        };
        match fs::remove_dir_all(&deletion_path) {
            Ok(_) => freed += s.bytes,
            Err(e) => {
                failures += 1;
                eprintln!("failed to delete {}: {}", s.path.display(), e);
            }
        }
    }
    println!(
        "\nDeleted {} directories, freed {}{}.",
        sized.len() as u64 - failures,
        human(freed),
        if failures > 0 {
            format!(" ({} failed)", failures)
        } else {
            String::new()
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "buildcleaner-test-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn elide_leaves_short_paths() {
        assert_eq!(elide("/a/b/c", 40), "/a/b/c");
    }

    #[test]
    fn elide_zero_disables() {
        let s = "/very/long/path/that/keeps/going/node_modules";
        assert_eq!(elide(s, 0), s);
    }

    #[test]
    fn elide_keeps_head_and_tail_at_exact_width() {
        let s = "/Users/dev/code/workspace/project/node_modules";
        let out = elide(s, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(out.contains('…'));
        let head: String = s.chars().take(14).collect();
        let tail: String = s.chars().skip(s.chars().count() - 15).collect();
        assert!(out.starts_with(&head), "got {out}");
        assert!(out.ends_with(&tail), "got {out}");
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1.5M"), Some(1_572_864));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("bogus"), None);
    }

    #[test]
    fn human_rounds() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1_572_864), "1.5 MB");
    }

    #[test]
    fn json_extract_path_roundtrips_a_match_event() {
        let p = "/Users/x/a b/node_modules";
        let line = format!(
            "{{\"event\":\"match\",\"path\":\"{}\",\"bytes\":10}}",
            json_escape(p)
        );
        assert_eq!(json_extract_path(&line).as_deref(), Some(p));
    }

    #[test]
    fn json_extract_path_handles_escapes() {
        let p = "/tmp/we\"ird\\path/\tnode_modules";
        let line = format!("{{\"path\":\"{}\"}}", json_escape(p));
        assert_eq!(json_extract_path(&line).as_deref(), Some(p));
    }

    #[test]
    fn json_extract_path_handles_unicode_surrogate_pairs() {
        assert_eq!(
            json_extract_path(r#"{"path":"/tmp/rocket-\uD83D\uDE80/node_modules"}"#).as_deref(),
            Some("/tmp/rocket-🚀/node_modules")
        );
    }

    #[test]
    fn json_extract_path_none_without_a_path_key() {
        assert_eq!(
            json_extract_path("{\"event\":\"progress\",\"scanned\":5}"),
            None
        );
        assert_eq!(json_extract_path("just a plain path string"), None);
        assert_eq!(
            json_extract_path(r#"{"payload":{"path":"/tmp/node_modules"}}"#),
            None
        );
    }

    #[test]
    fn stdin_input_limits_path_count_and_line_size() {
        let too_many = "node_modules\n".repeat(MAX_INPUT_PATHS + 1);
        assert!(
            read_input_paths_from(Cursor::new(too_many), InputFormat::Text)
                .unwrap_err()
                .contains("path limit")
        );

        let too_long = format!("{}\n", "a".repeat(MAX_INPUT_LINE_BYTES + 1));
        assert!(
            read_input_paths_from(Cursor::new(too_long), InputFormat::Text)
                .unwrap_err()
                .contains("line exceeds")
        );
    }

    #[test]
    fn revalidation_requires_the_scanned_root() {
        let base = test_dir("root-boundary");
        let root = base.join("root");
        let inside = root.join("node_modules");
        let outside = base.join("outside/node_modules");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let targets = default_targets();
        let index = build_index(&targets);
        let excludes = HashSet::new();
        let ctx = Ctx {
            targets: &targets,
            index: &index,
            excludes: &excludes,
            follow_symlinks: false,
        };
        let roots = vec![fs::canonicalize(&root).unwrap()];
        assert!(revalidate_within_roots(&inside, &roots, &ctx).is_ok());
        assert!(revalidate_within_roots(&outside, &roots, &ctx).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    /// Dropping a folder AND a folder inside it must not hide results: the
    /// descendant is redundant, and keeping both once made the nested dir
    /// invisible (the ancestor's walk skipped it as already-visited before it
    /// was ever match-tested).
    #[test]
    fn nested_roots_are_pruned_so_no_match_is_lost() {
        let base = test_dir("nested-roots");
        let outer = base.join("proj");
        let inner = outer.join("packages/app");
        fs::create_dir_all(inner.join("node_modules")).unwrap();
        fs::create_dir_all(outer.join("node_modules")).unwrap();

        let outer_c = fs::canonicalize(&outer).unwrap();
        let inner_c = fs::canonicalize(&inner).unwrap();

        // Ancestor first, descendant second, and the reverse: both prune to the ancestor.
        assert_eq!(
            prune_nested_roots(vec![outer_c.clone(), inner_c.clone()]),
            vec![outer_c.clone()]
        );
        assert_eq!(
            prune_nested_roots(vec![inner_c.clone(), outer_c.clone()]),
            vec![outer_c.clone()]
        );
        // A sibling that merely shares a name prefix is NOT nested.
        let sibling = base.join("proj-two");
        fs::create_dir_all(&sibling).unwrap();
        let sibling_c = fs::canonicalize(&sibling).unwrap();
        assert_eq!(
            prune_nested_roots(vec![outer_c.clone(), sibling_c.clone()]).len(),
            2
        );
        // Exact duplicates collapse.
        assert_eq!(
            prune_nested_roots(vec![outer_c.clone(), outer_c.clone()]),
            vec![outer_c.clone()]
        );

        // End to end: scanning the pruned root still finds the nested target.
        let targets = default_targets();
        let index = build_index(&targets);
        let excludes = HashSet::new();
        let ctx = Ctx {
            targets: &targets,
            index: &index,
            excludes: &excludes,
            follow_symlinks: false,
        };
        let roots = prune_nested_roots(vec![outer_c.clone(), inner_c.clone()]);
        let prog = Progress {
            scanned: AtomicU64::new(0),
            matches: Mutex::new(Vec::new()),
            visited: Mutex::new(roots.iter().cloned().collect()),
        };
        for root in &roots {
            scan(root, &ctx, &prog, root);
        }
        let found: HashSet<PathBuf> = prog
            .matches
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.path.clone())
            .collect();
        assert!(found.contains(&outer_c.join("node_modules")));
        assert!(
            found.contains(&inner_c.join("node_modules")),
            "nested root's own node_modules must still be reported"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn follow_symlinks_traverses_in_root_links_without_cycles() {
        let base = test_dir("follow-links");
        let root = base.join("root");
        let source = root.join("source");
        let target = source.join("node_modules");
        fs::create_dir_all(&target).unwrap();
        symlink(&source, root.join("linked")).unwrap();
        symlink(&source, source.join("cycle")).unwrap();

        let targets = default_targets();
        let index = build_index(&targets);
        let excludes = HashSet::from(["source".to_string()]);
        let ctx = Ctx {
            targets: &targets,
            index: &index,
            excludes: &excludes,
            follow_symlinks: true,
        };
        let canonical_root = fs::canonicalize(&root).unwrap();
        let prog = Progress {
            scanned: AtomicU64::new(0),
            matches: Mutex::new(Vec::new()),
            visited: Mutex::new(HashSet::from([canonical_root.clone()])),
        };
        scan(&canonical_root, &ctx, &prog, &canonical_root);
        let matches = prog.matches.lock().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, fs::canonicalize(target).unwrap());
        drop(matches);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn target_gate_rejects_non_targets_and_ungated_markers() {
        let targets = default_targets();
        let index = build_index(&targets);
        // Unambiguous target: matches with no marker required.
        assert!(match_target(
            "node_modules",
            Path::new("/proj/node_modules"),
            &index,
            &targets
        )
        .is_some());
        // Unknown name: never a target.
        assert!(match_target("src", Path::new("/proj/src"), &index, &targets).is_none());
        // Marker-gated `target` with no Cargo.toml sibling: rejected.
        assert!(match_target(
            "target",
            Path::new("/no/marker/here/target"),
            &index,
            &targets
        )
        .is_none());
    }
}
