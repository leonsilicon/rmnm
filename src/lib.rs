#![deny(clippy::all)]

//! remnem — find every nested `node_modules` under a project root (using the same
//! workspace resolution as bun / pnpm to describe the layout) and delete them
//! all, as fast as possible.

use std::path::{Path, PathBuf};

use napi::bindgen_prelude::*;
use napi_derive::napi;

mod finder;
mod workspace;

/// Options for [`clean`].
#[napi(object)]
pub struct CleanOptions {
  /// Project root to clean. Defaults to the current working directory.
  pub root: Option<String>,
  /// When `true`, only find (do not delete). Defaults to `false`.
  pub dry_run: Option<bool>,
  /// When `true`, measure the on-disk size of each `node_modules` before
  /// deleting (a second parallel pass). Defaults to `true`.
  pub measure: Option<bool>,
  /// When `true`, move each `node_modules` to the OS Trash — a same-volume
  /// rename, effectively instant, recoverable in Finder; space is reclaimed
  /// when the Trash is emptied. When `false` (the default), permanently
  /// `remove_dir_all` them in parallel (reclaims space immediately, not
  /// recoverable).
  pub trash: Option<bool>,
}

/// A single deleted (or would-be-deleted) `node_modules` directory.
#[napi(object)]
pub struct CleanedDir {
  pub path: String,
  /// Bytes the directory held (0 when `measure` is false).
  pub bytes: BigInt,
  /// `true` if this directory was (or would be) disposed of successfully.
  pub deleted: bool,
  /// `true` if it was moved to the Trash (vs. permanently removed).
  pub trashed: bool,
  /// Error message if disposal failed.
  pub error: Option<String>,
}

/// Result of a [`clean`] run.
#[napi(object)]
pub struct CleanResult {
  /// Absolute project root that was cleaned.
  pub root: String,
  /// Which workspace manifest drove resolution: `"none" | "package.json" | "pnpm"`.
  pub workspace_kind: String,
  /// Resolved workspace-package directories (absolute paths). Informational —
  /// deletion targets every nested `node_modules`, not just these.
  pub workspace_packages: Vec<String>,
  /// Every `node_modules` directory found (and, unless `dry_run`, deleted).
  pub cleaned: Vec<CleanedDir>,
  /// Total bytes across all found directories (0 when `measure` is false).
  pub total_bytes: BigInt,
  /// Total directories found.
  pub count: u32,
  /// Directories that failed to delete.
  pub failed: u32,
}

fn workspace_kind_str(kind: workspace::WorkspaceKind) -> &'static str {
  match kind {
    workspace::WorkspaceKind::None => "none",
    workspace::WorkspaceKind::PackageJson => "package.json",
    workspace::WorkspaceKind::Pnpm => "pnpm",
  }
}

/// Resolve workspace layout without deleting anything. Useful for inspecting how
/// bun/pnpm would see the workspace.
#[napi]
pub fn resolve_workspace(root: Option<String>) -> Result<CleanResult> {
  let root = resolve_root(root)?;
  let ws = workspace::resolve(&root);
  let packages = collect_packages(&root, &ws);
  Ok(CleanResult {
    root: root.to_string_lossy().into_owned(),
    workspace_kind: workspace_kind_str(ws.kind).to_string(),
    workspace_packages: packages,
    cleaned: Vec::new(),
    total_bytes: BigInt::from(0u64),
    count: 0,
    failed: 0,
  })
}

/// Find every nested `node_modules` under `root` and (unless `dry_run`) delete
/// them in parallel. Returns a structured summary.
#[napi]
pub fn clean(options: Option<CleanOptions>) -> Result<CleanResult> {
  let options = options.unwrap_or(CleanOptions {
    root: None,
    dry_run: None,
    measure: None,
    trash: None,
  });
  let root = resolve_root(options.root)?;
  let dry_run = options.dry_run.unwrap_or(false);
  let measure = options.measure.unwrap_or(true);
  // Direct in-place removal is the default; `-t` opts into moving to the Trash.
  let mode = if options.trash.unwrap_or(false) {
    finder::Mode::Trash
  } else {
    finder::Mode::Remove
  };

  let ws = workspace::resolve(&root);
  let workspace_packages = collect_packages(&root, &ws);

  let found = finder::find(&root, measure);
  let total_bytes: u64 = found.iter().map(|f| f.bytes).sum();
  let count = found.len() as u32;

  let paths: Vec<PathBuf> = found.iter().map(|f| f.path.clone()).collect();

  let cleaned: Vec<CleanedDir> = if dry_run {
    found
      .into_iter()
      .map(|f| CleanedDir {
        path: f.path.to_string_lossy().into_owned(),
        bytes: BigInt::from(f.bytes),
        deleted: false,
        trashed: false,
        error: None,
      })
      .collect()
  } else {
    let bytes_by_path: std::collections::HashMap<PathBuf, u64> =
      found.into_iter().map(|f| (f.path, f.bytes)).collect();
    let results = finder::delete_all(paths, mode);
    results
      .into_iter()
      .map(|r| {
        let bytes = bytes_by_path.get(&r.path).copied().unwrap_or(0);
        CleanedDir {
          path: r.path.to_string_lossy().into_owned(),
          bytes: BigInt::from(bytes),
          deleted: r.error.is_none(),
          trashed: r.trashed,
          error: r.error,
        }
      })
      .collect()
  };

  let failed = cleaned.iter().filter(|c| c.error.is_some()).count() as u32;

  Ok(CleanResult {
    root: root.to_string_lossy().into_owned(),
    workspace_kind: workspace_kind_str(ws.kind).to_string(),
    workspace_packages,
    cleaned,
    total_bytes: BigInt::from(total_bytes),
    count,
    failed,
  })
}

fn resolve_root(root: Option<String>) -> Result<PathBuf> {
  let raw = match root {
    Some(r) if !r.is_empty() => PathBuf::from(r),
    _ => std::env::current_dir()
      .map_err(|e| Error::from_reason(format!("cannot read current directory: {e}")))?,
  };
  // Canonicalize so results are absolute and symlink-resolved. Fall back to the
  // raw path if canonicalization fails (e.g. path does not exist yet).
  let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
  if !canonical.is_dir() {
    return Err(Error::from_reason(format!(
      "root is not a directory: {}",
      canonical.to_string_lossy()
    )));
  }
  Ok(canonical)
}

fn collect_packages(root: &Path, ws: &workspace::Workspace) -> Vec<String> {
  match workspace::WorkspaceMatcher::build(ws) {
    Ok(matcher) => workspace::collect_workspace_dirs(root, &matcher)
      .into_iter()
      .map(|p| p.to_string_lossy().into_owned())
      .collect(),
    Err(_) => Vec::new(),
  }
}
