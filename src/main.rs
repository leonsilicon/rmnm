#![deny(clippy::all)]

//! `remnem` — delete every nested `node_modules` under a project root, fast.
//!
//! A single self-contained binary: scan (a lean parallel directory walk), show
//! the user what will go, confirm, then dispose of them. The instant default
//! renames each `node_modules` out of the repo into an OS-temp staging dir and
//! reclaims the space in a detached background process. Sizing (`-m`) and
//! workspace resolution (`-w`) are extra tree walks, so they are opt-in.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use remnem::{finder, resolve_workspace, workspace_kind_str, Mode, WorkspaceKind};

const HELP: &str = "\
remnem — delete every nested node_modules, fast

Usage:
  remnem [path] [options]

Arguments:
  path                 Project root to clean (default: current directory)

Options:
  -l, --list           List what would be cleared; touch nothing
  -m, --measure        Size each node_modules (slow: walks every dependency tree)
  -w, --workspace      Also resolve & report the bun/pnpm workspace layout (slow)
      --sync           Wait for the disk space to actually free before returning
      --json           Print the raw result as JSON
  -y, --yes            Skip the confirmation prompt
  -h, --help           Show this help

Finds every node_modules directory under <path> (root + all workspace packages
+ any nested ones), then deletes them.

By default each node_modules is instantly renamed out of the repository into the
OS temp directory (an O(1) same-volume operation) so it is gone from its
location immediately — a clean reinstall can start right away, and nothing is
left in the tree for git to see — while the bytes are reclaimed by a detached
background process. Pass --sync to block until that reclaim finishes. (If the
temp dir is on a different filesystem, remnem deletes in place synchronously.)
";

#[derive(Default)]
struct Opts {
  root: Option<String>,
  list: bool,
  measure: bool,
  workspace: bool,
  sync: bool,
  json: bool,
  yes: bool,
  help: bool,
}

fn parse_args(argv: &[String]) -> Result<Opts, ExitCode> {
  let mut opts = Opts::default();
  for arg in argv {
    match arg.as_str() {
      "-h" | "--help" => opts.help = true,
      "-l" | "--list" => opts.list = true,
      "-m" | "--measure" => opts.measure = true,
      "-w" | "--workspace" => opts.workspace = true,
      "--sync" => opts.sync = true,
      "--json" => opts.json = true,
      "-y" | "--yes" => opts.yes = true,
      other => {
        if other.starts_with('-') {
          eprint!("remnem: unknown option {other}\n\n{HELP}");
          return Err(ExitCode::from(2));
        }
        if opts.root.is_some() {
          eprintln!("remnem: unexpected extra argument {other}");
          return Err(ExitCode::from(2));
        }
        opts.root = Some(other.to_string());
      }
    }
  }
  Ok(opts)
}

fn main() -> ExitCode {
  let argv: Vec<String> = std::env::args().skip(1).collect();

  // Hidden background-reaper subcommand: `remnem __reap <staging-dir>`. Spawned
  // detached by the instant delete path to hard-remove the staging directory
  // off the critical path. Not part of the public CLI.
  if argv.first().map(String::as_str) == Some("__reap") {
    if let Some(dir) = argv.get(1) {
      finder::reap(Path::new(dir));
    }
    return ExitCode::SUCCESS;
  }

  let opts = match parse_args(&argv) {
    Ok(o) => o,
    Err(code) => return code,
  };

  if opts.help {
    print!("{HELP}");
    return ExitCode::SUCCESS;
  }

  match run(&opts) {
    Ok(code) => code,
    Err(e) => {
      eprintln!("remnem: {e}");
      ExitCode::FAILURE
    }
  }
}

fn run(opts: &Opts) -> std::io::Result<ExitCode> {
  let root = resolve_root(opts.root.as_deref())?;
  let timing = std::env::var_os("REMNEM_TIMING").is_some();

  // Fast path: when we are going to permanently delete (default mode), the user
  // has pre-authorized it (`-y`, or a non-TTY where the prompt auto-confirms),
  // and no extra reporting is requested (`-m`/`-w`/`--json`/`--list`), we skip
  // printing the full list of directories and go straight scan → dispose →
  // summary. (We deliberately keep discovery and disposal as two separate passes
  // rather than fusing them: renaming a node_modules mutates its parent directory
  // mid-walk, which invalidates the readdir cache and contends on directory
  // locks — measurably slower than a clean read-only walk followed by a rename
  // pass.)
  let auto_yes = opts.yes || !std::io::stdin().is_terminal();
  let fast_delete =
    auto_yes && !opts.list && !opts.sync && !opts.measure && !opts.workspace && !opts.json;
  if fast_delete {
    let scan_start = std::time::Instant::now();
    let found = finder::find(&root, false);
    if timing {
      eprintln!(
        "[timing] scan: {:.1}ms",
        scan_start.elapsed().as_secs_f64() * 1e3
      );
    }
    let paths: Vec<PathBuf> = found.into_iter().map(|f| f.path).collect();
    let start = std::time::Instant::now();
    let results = finder::delete_all(paths, Mode::Remove);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1e3;
    if timing {
      eprintln!("[timing] dispose: {elapsed_ms:.1}ms");
    }
    return report_deletion(&root, opts, &results, elapsed_ms, 0);
  }

  // Workspace resolution is a separate source-tree walk; only pay for it on -w.
  let (ws_kind, ws_packages) = if opts.workspace {
    resolve_workspace(&root)
  } else {
    (WorkspaceKind::None, Vec::new())
  };

  // Phase 1: the scan. A bare scan is just the fast node_modules find; `-m` adds
  // the (expensive) size pass.
  let scan_start = std::time::Instant::now();
  let found = finder::find(&root, opts.measure);
  if timing {
    eprintln!(
      "[timing] scan: {:.1}ms",
      scan_start.elapsed().as_secs_f64() * 1e3
    );
  }
  let count = found.len();
  let total_bytes: u64 = found.iter().map(|f| f.bytes).sum();

  let kind_label = if opts.workspace {
    Some(if ws_kind == WorkspaceKind::None {
      "no workspace config".to_string()
    } else {
      let n = ws_packages.len();
      format!(
        "{} workspace ({n} package{})",
        workspace_kind_str(ws_kind),
        if n == 1 { "" } else { "s" }
      )
    })
  } else {
    None
  };

  if opts.json && opts.list {
    print!("{}", scan_json(&root, ws_kind, &ws_packages, &found));
    return Ok(ExitCode::SUCCESS);
  }

  let stdout = std::io::stdout();
  let mut out = stdout.lock();

  // The bare-`--json` (non-list) delete path prints nothing but the final JSON,
  // so all the human-readable scan output and the confirmation prompt are gated
  // behind `!opts.json`. (`--json --list` was already handled above.)
  if !opts.json {
    if count == 0 {
      let where_ = kind_label
        .as_deref()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
      writeln!(
        out,
        "remnem: no node_modules found under {}{where_}",
        root.display()
      )?;
      return Ok(ExitCode::SUCCESS);
    }

    writeln!(out, "root: {}", root.display())?;
    if let Some(label) = &kind_label {
      writeln!(out, "      {label}")?;
    }
    if opts.measure {
      writeln!(
        out,
        "found {count} node_modules totalling {}:",
        format_bytes(total_bytes)
      )?;
    } else {
      writeln!(out, "found {count} node_modules:")?;
    }
    for f in &found {
      if opts.measure {
        writeln!(
          out,
          "  {:>8}  {}",
          format_bytes(f.bytes),
          relativize(&root, &f.path)
        )?;
      } else {
        writeln!(out, "  {}", relativize(&root, &f.path))?;
      }
    }

    if opts.list {
      writeln!(out, "\n(list only — nothing deleted)")?;
      return Ok(ExitCode::SUCCESS);
    }

    if !opts.yes
      && !confirm(
        &mut out,
        &format!("\npermanently delete these {count} directories? [y/N] "),
      )?
    {
      writeln!(out, "aborted.")?;
      return Ok(ExitCode::FAILURE);
    }
  }

  // Drop the stdout lock before disposal so `report_deletion` can take its own.
  drop(out);

  // Phase 2: real disposal. Reuse the paths we already found — no second walk.
  let mode = if opts.sync {
    Mode::RemoveSync
  } else {
    Mode::Remove
  };
  let paths: Vec<PathBuf> = found.iter().map(|f| f.path.clone()).collect();

  let start = std::time::Instant::now();
  let results = finder::delete_all(paths, mode);
  let elapsed_ms = start.elapsed().as_secs_f64() * 1e3;
  if timing {
    eprintln!("[timing] dispose: {elapsed_ms:.1}ms");
  }

  report_deletion(&root, opts, &results, elapsed_ms, total_bytes)
}

/// Report a disposal (fused fast path or normal path) as JSON or a human summary,
/// and return the appropriate exit code.
fn report_deletion(
  root: &Path,
  opts: &Opts,
  results: &[finder::DeleteResult],
  elapsed_ms: f64,
  total_bytes: u64,
) -> std::io::Result<ExitCode> {
  if opts.json {
    print!("{}", delete_json(results));
    return Ok(ExitCode::SUCCESS);
  }

  let stdout = std::io::stdout();
  let mut out = stdout.lock();

  let failed = results.iter().filter(|r| r.error.is_some()).count();
  let done = results.len() - failed;

  if opts.measure {
    writeln!(
      out,
      "deleted: {done}/{} node_modules ({}) in {elapsed_ms:.0}ms",
      results.len(),
      format_bytes(total_bytes)
    )?;
  } else {
    writeln!(
      out,
      "deleted: {done}/{} node_modules in {elapsed_ms:.0}ms",
      results.len()
    )?;
  }

  if !opts.sync && done > 0 {
    // The instant path renames the trees out of the repo and frees the disk in a
    // detached background process, so `node_modules` is already gone everywhere
    // but the bytes come back a moment later. (With --sync they're already freed.)
    writeln!(out, "(space is being reclaimed in the background)")?;
  }

  if failed > 0 {
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    for r in results {
      if let Some(msg) = &r.error {
        writeln!(err, "  failed: {} — {msg}", relativize(root, &r.path))?;
      }
    }
    return Ok(ExitCode::FAILURE);
  }

  Ok(ExitCode::SUCCESS)
}

/// Resolve and validate the project root, canonicalizing so results are absolute
/// and symlink-resolved.
fn resolve_root(root: Option<&str>) -> std::io::Result<PathBuf> {
  let raw = match root {
    Some(r) if !r.is_empty() => PathBuf::from(r),
    _ => std::env::current_dir()?,
  };
  let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
  if !canonical.is_dir() {
    return Err(std::io::Error::new(
      std::io::ErrorKind::NotADirectory,
      format!("root is not a directory: {}", canonical.display()),
    ));
  }
  Ok(canonical)
}

/// Confirmation prompt so a bare `remnem` in a real repo can't nuke node_modules
/// on a stray keystroke. Auto-confirms when not attached to a TTY (CI / piped).
fn confirm(out: &mut impl Write, question: &str) -> std::io::Result<bool> {
  if !std::io::stdin().is_terminal() {
    return Ok(true);
  }
  write!(out, "{question}")?;
  out.flush()?;
  let mut buf = [0u8; 64];
  let n = std::io::stdin().read(&mut buf).unwrap_or(0);
  let answer = String::from_utf8_lossy(&buf[..n]).trim().to_lowercase();
  Ok(answer == "y" || answer == "yes")
}

fn relativize(root: &Path, p: &Path) -> String {
  if p == root {
    return ".".to_string();
  }
  match p.strip_prefix(root) {
    Ok(rel) => rel.display().to_string(),
    Err(_) => p.display().to_string(),
  }
}

fn format_bytes(bytes: u64) -> String {
  if bytes == 0 {
    return "0 B".to_string();
  }
  const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
  let exp = ((bytes as f64).ln() / 1024f64.ln())
    .floor()
    .min((UNITS.len() - 1) as f64) as usize;
  let value = bytes as f64 / 1024f64.powi(exp as i32);
  if value >= 10.0 || exp == 0 {
    format!("{value:.0} {}", UNITS[exp])
  } else {
    format!("{value:.1} {}", UNITS[exp])
  }
}

// --- JSON output (hand-rolled to avoid pulling serde into the hot path) -------

fn json_escape(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c => out.push(c),
    }
  }
  out
}

fn scan_json(
  root: &Path,
  kind: WorkspaceKind,
  packages: &[PathBuf],
  found: &[finder::FoundNodeModules],
) -> String {
  let mut s = String::from("{\n");
  s.push_str(&format!(
    "  \"root\": \"{}\",\n",
    json_escape(&root.display().to_string())
  ));
  s.push_str(&format!(
    "  \"workspaceKind\": \"{}\",\n",
    workspace_kind_str(kind)
  ));
  s.push_str("  \"workspacePackages\": [");
  for (i, p) in packages.iter().enumerate() {
    if i > 0 {
      s.push(',');
    }
    s.push_str(&format!(
      "\n    \"{}\"",
      json_escape(&p.display().to_string())
    ));
  }
  if !packages.is_empty() {
    s.push_str("\n  ");
  }
  s.push_str("],\n");
  let total: u64 = found.iter().map(|f| f.bytes).sum();
  s.push_str(&format!("  \"count\": {},\n", found.len()));
  s.push_str(&format!("  \"totalBytes\": {total},\n"));
  s.push_str("  \"cleaned\": [");
  for (i, f) in found.iter().enumerate() {
    if i > 0 {
      s.push(',');
    }
    s.push_str(&format!(
      "\n    {{ \"path\": \"{}\", \"bytes\": {}, \"deleted\": false }}",
      json_escape(&f.path.display().to_string()),
      f.bytes
    ));
  }
  if !found.is_empty() {
    s.push_str("\n  ");
  }
  s.push_str("]\n}\n");
  s
}

fn delete_json(results: &[finder::DeleteResult]) -> String {
  let failed = results.iter().filter(|r| r.error.is_some()).count();
  let mut s = String::from("{\n");
  s.push_str(&format!("  \"count\": {},\n", results.len()));
  s.push_str(&format!("  \"failed\": {failed},\n"));
  s.push_str("  \"cleaned\": [");
  for (i, r) in results.iter().enumerate() {
    if i > 0 {
      s.push(',');
    }
    let err = match &r.error {
      Some(e) => format!("\"{}\"", json_escape(e)),
      None => "null".to_string(),
    };
    s.push_str(&format!(
      "\n    {{ \"path\": \"{}\", \"deleted\": {}, \"error\": {err} }}",
      json_escape(&r.path.display().to_string()),
      r.error.is_none(),
    ));
  }
  if !results.is_empty() {
    s.push_str("\n  ");
  }
  s.push_str("]\n}\n");
  s
}
