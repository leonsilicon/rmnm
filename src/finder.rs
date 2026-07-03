//! Finding and deleting every nested `node_modules` directory under a root.
//!
//! # Finding
//!
//! We do a lean, parallel directory-only walk. The `ignore` crate (ripgrep's
//! walker) is built to yield *files* and apply gitignore machinery; here we care
//! about neither — we only need directories, and we want to visit as few entries
//! as possible. So we hand-roll the walk on `std::fs::read_dir`, which on macOS
//! and Linux exposes each entry's type via `d_type` (from `readdir`) so we can
//! tell directories from files **without a per-entry `stat`**.
//!
//! The key tricks for speed:
//!   - Once we see a `node_modules` directory we record it and DO NOT descend —
//!     the whole subtree is slated for deletion, so walking its (often enormous)
//!     contents would be pure waste. This is what keeps the walk proportional to
//!     the *source* tree, not the installed dependency tree.
//!   - We never touch regular files: on each `read_dir` we only recurse into
//!     sub-directories and skip everything else with no extra syscalls.
//!   - `.git` is pruned (huge, never holds a `node_modules` we care about).
//!   - Work is fanned across a `rayon` scope so independent subtrees walk in
//!     parallel, and results are gathered per-thread (no shared lock on the hot
//!     path) then merged once at the end.
//!
//! # Deleting
//!
//! `Mode::Remove` (the instant default) renames each `node_modules` out of the
//! repository into a per-run staging directory under the OS temp dir — an O(1)
//! metadata op — then a detached background process `remove_dir_all`s the staging
//! directory so the disk-freeing I/O never blocks. Nothing is ever left inside
//! the repo tree, so `git` can't see it. `Mode::RemoveSync` skips the staging and
//! `remove_dir_all`s in place, blocking until the space is actually reclaimed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A discovered `node_modules` directory and the number of bytes it holds.
#[derive(Debug)]
pub struct FoundNodeModules {
  pub path: PathBuf,
  pub bytes: u64,
}

/// Walk `root` in parallel and collect every `node_modules` directory, without
/// descending into any of them (nested `node_modules` inside a `node_modules`
/// are covered by deleting the outer one, so we never recurse in).
///
/// `measure` controls whether each directory's on-disk size is summed (a second
/// parallel pass). Sizing is inherently expensive (it has to touch every file in
/// every dependency tree), so it is off by default and only done when explicitly
/// requested.
pub fn find(root: &Path, measure: bool) -> Vec<FoundNodeModules> {
  let mut paths = find_node_modules(root);
  paths.sort();

  if !measure {
    return paths
      .into_iter()
      .map(|path| FoundNodeModules { path, bytes: 0 })
      .collect();
  }

  use rayon::prelude::*;
  paths
    .into_par_iter()
    .map(|path| {
      let bytes = dir_size(&path);
      FoundNodeModules { path, bytes }
    })
    .collect()
}

/// Parallel, directory-only walk that collects every top-level `node_modules`
/// directory under `root`. Regular files are never inspected; `.git` and the
/// interior of any `node_modules` are never entered.
///
/// Findings are gathered into a single `Mutex<Vec<..>>`, but the lock is touched
/// at most once per *directory that actually contains a `node_modules`* (to
/// append that dir's finds in one batch) — never on the per-entry hot path — so
/// contention is negligible.
fn find_node_modules(root: &Path) -> Vec<PathBuf> {
  let sink = Mutex::new(Vec::new());
  rayon::scope(|scope| {
    walk_dir(root.to_path_buf(), scope, &sink);
  });
  sink.into_inner().unwrap()
}

/// Recursively walk `dir`, appending discovered `node_modules` paths to `sink`
/// and spawning parallel tasks for each sub-directory that must be descended.
///
/// Only sub-directories are ever recursed into; regular files are skipped with
/// no extra syscall (their type comes from `readdir`'s `d_type`). `node_modules`
/// and `.git` are recorded/pruned without descending.
fn walk_dir<'scope>(dir: PathBuf, scope: &rayon::Scope<'scope>, sink: &'scope Mutex<Vec<PathBuf>>) {
  let entries = match fs::read_dir(&dir) {
    Ok(e) => e,
    // A directory we cannot read (permissions, race) simply contributes nothing.
    Err(_) => return,
  };

  // Sub-directories we must descend into, and any node_modules found right here.
  let mut subdirs: Vec<PathBuf> = Vec::new();
  let mut found_here: Vec<PathBuf> = Vec::new();

  for entry in entries.flatten() {
    // `file_type()` is served from the `readdir` `d_type` on macOS/Linux — no
    // extra `stat` syscall. (Filesystems that don't report a type fall back to a
    // stat inside `is_dir()`, but that is the uncommon path.)
    let Ok(file_type) = entry.file_type() else {
      continue;
    };
    if !file_type.is_dir() {
      // Regular file / symlink: never relevant to finding node_modules.
      continue;
    }

    let name = entry.file_name();
    if name == "node_modules" {
      // Found one. Record it and DO NOT descend — the whole tree goes.
      found_here.push(entry.path());
      continue;
    }
    if name == ".git" {
      // VCS metadata: huge, never holds a node_modules we care about.
      continue;
    }

    subdirs.push(entry.path());
  }

  if !found_here.is_empty() {
    sink.lock().unwrap().append(&mut found_here);
  }

  // Fan the sub-directories across the pool. Keep the last one on this thread to
  // avoid spawning a task only to immediately block on it (and to keep shallow
  // trees from paying task-spawn overhead they don't need).
  let inline = subdirs.pop();
  for child in subdirs {
    scope.spawn(move |s| walk_dir(child, s, sink));
  }
  if let Some(child) = inline {
    walk_dir(child, scope, sink);
  }
}

/// Sum the apparent size of all regular files under `dir`, in parallel.
fn dir_size(dir: &Path) -> u64 {
  use std::sync::atomic::AtomicU64;
  let total = AtomicU64::new(0);
  rayon::scope(|scope| {
    size_dir(dir.to_path_buf(), scope, &total);
  });
  total.into_inner()
}

fn size_dir<'scope>(
  dir: PathBuf,
  scope: &rayon::Scope<'scope>,
  total: &'scope std::sync::atomic::AtomicU64,
) {
  use std::sync::atomic::Ordering;
  let entries = match fs::read_dir(&dir) {
    Ok(e) => e,
    Err(_) => return,
  };
  let mut subdirs: Vec<PathBuf> = Vec::new();
  for entry in entries.flatten() {
    let Ok(file_type) = entry.file_type() else {
      continue;
    };
    if file_type.is_dir() {
      subdirs.push(entry.path());
    } else if file_type.is_file() {
      if let Ok(meta) = entry.metadata() {
        total.fetch_add(meta.len(), Ordering::Relaxed);
      }
    }
  }
  let inline = subdirs.pop();
  for child in subdirs {
    scope.spawn(move |s| size_dir(child, s, total));
  }
  if let Some(child) = inline {
    size_dir(child, scope, total);
  }
}

/// Outcome of disposing of one directory.
#[derive(Debug)]
pub struct DeleteResult {
  pub path: PathBuf,
  pub error: Option<String>,
}

/// How a directory should be disposed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  /// **Default, and the reason `remnem` is instant.** Each `node_modules` is
  /// `rename`d out of the repository entirely, into a per-run staging directory
  /// under the OS temp dir (`$TMPDIR/remnem-<pid>/`). On one filesystem that is
  /// an O(1) metadata operation no matter how large the tree is. The moment the
  /// rename returns the `node_modules` is gone from its location — a clean
  /// reinstall can start immediately — and because the staged copy lives outside
  /// the repo, it can never be seen by `git status`/`git add`. The staging
  /// directory is then `remove_dir_all`d by a **detached background process**
  /// (see [`reap`]), so the disk-freeing I/O never blocks the foreground; space
  /// comes back within seconds, hands-free. If the temp dir is on a *different*
  /// filesystem (so `rename` would need a cross-device copy), we fall back to a
  /// synchronous in-place `remove_dir_all` — never leaving anything in the tree.
  Remove,
  /// Synchronous, blocking `remove_dir_all` — waits until the space is actually
  /// reclaimed. Slower (I/O-bound on huge trees) but self-contained: used by the
  /// background reaper and by callers/tests that must observe the space freed
  /// before returning.
  RemoveSync,
}

/// Dispose of every given directory according to `mode`. Each operation is
/// independent; an error on one does not stop the others.
pub fn delete_all(dirs: Vec<PathBuf>, mode: Mode) -> Vec<DeleteResult> {
  match mode {
    Mode::Remove => rename_and_reap(dirs),
    Mode::RemoveSync => remove_all_parallel(dirs)
      .into_iter()
      .map(|(path, error)| DeleteResult { path, error })
      .collect(),
  }
}

/// The instant path. Rename every `node_modules` **out of the repository** into a
/// per-run staging directory under the OS temp dir, then hand that staging
/// directory to a detached background process that hard-deletes it. Returns as
/// soon as the renames are done — the caller sees every `node_modules` already
/// gone from its location, and nothing is ever left inside the repo tree.
fn rename_and_reap(dirs: Vec<PathBuf>) -> Vec<DeleteResult> {
  let timing = std::env::var_os("REMNEM_TIMING").is_some();
  let rename_start = std::time::Instant::now();
  let pid = std::process::id();

  // Per-run staging root under the OS temp dir. Keeping every staged tree here
  // (rather than a sibling of the original) means nothing doomed ever appears
  // inside the repository, so `git status`/`git add` can't pick it up.
  let staging_root = std::env::temp_dir().join(format!("remnem-{pid}"));

  // If we can't even create the staging root, fall back to a plain synchronous
  // remove of everything — correctness over speed.
  if std::fs::create_dir_all(&staging_root).is_err() {
    return remove_all_parallel(dirs)
      .into_iter()
      .map(|(path, error)| DeleteResult { path, error })
      .collect();
  }

  // Probe once whether the temp dir is on the same filesystem as the targets:
  // rename the first directory into staging. `EXDEV` (cross-device) means every
  // rename here would need a slow copy, so we fall back to in-place removal for
  // the whole batch (still never touching the repo tree).
  let same_volume = probe_same_volume(&dirs, &staging_root);
  if !same_volume {
    let _ = std::fs::remove_dir_all(&staging_root);
    if timing {
      eprintln!("[timing]   temp dir cross-volume; hard-removing in place");
    }
    return remove_all_parallel(dirs)
      .into_iter()
      .map(|(path, error)| DeleteResult { path, error })
      .collect();
  }

  let results = parallel_rename(dirs, &staging_root);
  if timing {
    eprintln!(
      "[timing]   rename: {:.1}ms",
      rename_start.elapsed().as_secs_f64() * 1e3,
    );
  }

  // Hand the whole staging directory to a detached background reaper. If we
  // can't spawn one (unlikely), delete it synchronously so we never leak space.
  let spawn_start = std::time::Instant::now();
  if let Err(_e) = spawn_reaper(&staging_root) {
    let _ = std::fs::remove_dir_all(&staging_root);
  }
  if timing {
    eprintln!(
      "[timing]   spawn reaper: {:.1}ms",
      spawn_start.elapsed().as_secs_f64() * 1e3
    );
  }

  results
}

/// Decide whether `staging_root` is on the same filesystem as the target dirs by
/// attempting to rename the first non-vanished target into it. On success the
/// rename stands (that dir is now staged); the target is renamed back only if we
/// later decide *not* to proceed — but we always proceed when same-volume, so a
/// successful probe just means the first item is already staged.
///
/// Returns `true` if a rename into `staging_root` succeeds (same volume), `false`
/// on `EXDEV`/cross-device. A successful probe leaves the first dir renamed into
/// `staging_root/probe`, which the caller's staging pass and reaper both cover
/// (the reaper deletes the whole staging root).
fn probe_same_volume(dirs: &[PathBuf], staging_root: &Path) -> bool {
  let Some(first) = dirs.iter().find(|d| d.exists()) else {
    // Nothing to move — treat as same-volume (the empty batch is a no-op).
    return true;
  };
  let probe = staging_root.join("probe");
  match std::fs::rename(first, &probe) {
    Ok(()) => {
      // Move it back so the main pass renames every dir uniformly (and so the
      // per-dir DeleteResult ordering is produced in one place).
      let _ = std::fs::rename(&probe, first);
      true
    }
    Err(e) => e.raw_os_error() != Some(libc_exdev()),
  }
}

/// `EXDEV` errno — "cross-device link". A `rename` across filesystems fails with
/// this; anything else (permissions, already-gone) we let the per-item rename
/// handle so it can fall back individually.
fn libc_exdev() -> i32 {
  // 18 on Linux and macOS. Kept as a constant to avoid a libc dependency.
  18
}

/// Rename every directory into the staging area, spreading the work over an
/// **oversubscribed** thread pool. Renames are I/O-syscall-bound (each blocks on
/// the filesystem's directory-metadata journal, not the CPU), so running many
/// more threads than cores hides that latency: on APFS this lifts throughput from
/// ~16k renames/sec (core-count threads) toward the filesystem's ceiling. We use
/// our own scratch threads rather than rayon's CPU-sized global pool for exactly
/// this reason.
///
/// Each thread renames into **its own shard directory** (`staging_root/<tid>/`).
/// This matters: a `rename` mutates the *destination* directory's inode, so if
/// every thread renamed into one shared staging dir they'd serialize on that one
/// directory lock (measurably slower). Per-thread shards remove that contention
/// while keeping everything under the temp staging root (so nothing lands in the
/// repo). Work is split into contiguous chunks, one owned by each thread — no
/// shared state, no locks, no unsafe.
fn parallel_rename(dirs: Vec<PathBuf>, staging_root: &Path) -> Vec<DeleteResult> {
  let n = dirs.len();
  if n == 0 {
    return Vec::new();
  }

  // Oversubscribe: ~5× cores, capped, and never more threads than items.
  let cores = std::thread::available_parallelism()
    .map(|c| c.get())
    .unwrap_or(4);
  let threads = (cores * 5).clamp(1, 64).min(n);
  let chunk = n.div_ceil(threads);

  // One shard directory per thread, created up front so the threads never race
  // to create them. Pre-creating is cheap (a handful of mkdirs).
  let mut chunks: Vec<(PathBuf, Vec<PathBuf>)> = Vec::with_capacity(threads);
  let mut tid = 0;
  let mut remaining = dirs;
  while !remaining.is_empty() {
    let take = chunk.min(remaining.len());
    let rest = remaining.split_off(take);
    let shard = staging_root.join(tid.to_string());
    let _ = std::fs::create_dir(&shard);
    chunks.push((shard, remaining));
    tid += 1;
    remaining = rest;
  }

  let mut per_thread: Vec<Vec<DeleteResult>> = std::thread::scope(|scope| {
    let handles: Vec<_> = chunks
      .into_iter()
      .map(|(shard, chunk_dirs)| {
        scope.spawn(move || {
          chunk_dirs
            .into_iter()
            .enumerate()
            .map(|(i, path)| dispose_one(path, &shard, i))
            .collect::<Vec<_>>()
        })
      })
      .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
  });

  // Re-concatenate the per-thread results in order.
  let mut results = Vec::with_capacity(n);
  for chunk in &mut per_thread {
    results.append(chunk);
  }
  results
}

/// Dispose of one directory by renaming it into `shard/<idx>`. On any rename
/// failure that isn't "already gone" (a late cross-device edge, permissions, …)
/// fall back to a synchronous in-place `remove_dir_all`, so we always make
/// progress and never leave the `node_modules` behind in the tree.
fn dispose_one(path: PathBuf, shard: &Path, idx: usize) -> DeleteResult {
  let staged = shard.join(idx.to_string());
  match std::fs::rename(&path, &staged) {
    Ok(()) => DeleteResult { path, error: None },
    // Already gone — nothing to do.
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => DeleteResult { path, error: None },
    Err(_) => {
      let error = match std::fs::remove_dir_all(&path) {
        Ok(()) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(e.to_string()),
      };
      DeleteResult { path, error }
    }
  }
}

/// Spawn a detached child process that hard-deletes the staged directories in
/// the background, then exits — so the disk-freeing I/O never blocks `remnem`.
///
/// The whole per-run staging directory is passed as a single argument; the reaper
/// `remove_dir_all`s it. The child is fully detached: new session, no controlling
/// terminal, stdio to /dev/null, so it outlives the parent shell without holding
/// it open.
fn spawn_reaper(staging_root: &Path) -> std::io::Result<()> {
  let exe = std::env::current_exe()?;
  let mut cmd = std::process::Command::new(exe);
  cmd
    .arg("__reap")
    .arg(staging_root)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());

  // Detach from the controlling terminal / process group so the reaper is not
  // killed when the invoking shell command returns.
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` in the pre-exec hook of the child only touches the child.
    unsafe {
      cmd.pre_exec(|| {
        // Start a new session; detaches from the parent's controlling terminal.
        libc_setsid();
        Ok(())
      });
    }
  }

  cmd.spawn()?;
  Ok(())
}

/// `setsid(2)` via a tiny extern binding so we don't pull in the whole `libc`
/// crate for one call. Detaches the reaper into its own session.
#[cfg(unix)]
fn libc_setsid() {
  extern "C" {
    fn setsid() -> i32;
  }
  // SAFETY: `setsid` has no memory effects; a failure (already a session leader)
  // is harmless for our purposes.
  unsafe {
    setsid();
  }
}

/// Hard-delete the staging directory (recursively, in parallel over its immediate
/// children). This is the body of the detached `__reap` subcommand; errors are
/// ignored — the reaper is best-effort background cleanup.
pub fn reap(staging_root: &Path) {
  // Delete each staged tree in parallel, then the (now-empty) staging root.
  let children: Vec<PathBuf> = match std::fs::read_dir(staging_root) {
    Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
    Err(_) => return,
  };
  let _ = remove_all_parallel(children);
  let _ = std::fs::remove_dir_all(staging_root);
}

fn remove_all_parallel(dirs: Vec<PathBuf>) -> Vec<(PathBuf, Option<String>)> {
  use rayon::prelude::*;
  dirs
    .into_par_iter()
    .map(|path| {
      let error = match std::fs::remove_dir_all(&path) {
        Ok(()) => None,
        // A concurrent delete / already-gone directory is not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(e.to_string()),
      };
      (path, error)
    })
    .collect()
}
