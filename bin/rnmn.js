#!/usr/bin/env node
"use strict";

const { clean } = require("../index.js");

const HELP = `rnmn — delete every nested node_modules, fast

Usage:
  rnmn [path] [options]

Arguments:
  path                 Project root to clean (default: current directory)

Options:
  -t, --trash          Move to the Trash instead of deleting (instant, recoverable)
  -l, --list           List what would be cleared; touch nothing
      --no-measure     Skip sizing each node_modules (faster; sizes show as 0)
      --json           Print the raw result as JSON
  -y, --yes            Skip the confirmation prompt
  -h, --help           Show this help

Finds every node_modules directory under <path> (root + all workspace packages
+ any nested ones), using the same workspace resolution as bun / pnpm to report
the layout, then permanently deletes them in parallel.

With -t, moves them to the Trash instead — on the same volume that is a rename
(instant no matter how large) and recoverable in Finder; the space is reclaimed
when you empty the Trash.
`;

function parseArgs(argv) {
  const opts = {
    root: undefined,
    list: false,
    measure: true,
    trash: false,
    json: false,
    yes: false,
    help: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case "-h":
      case "--help":
        opts.help = true;
        break;
      case "-l":
      case "--list":
        opts.list = true;
        break;
      case "-t":
      case "--trash":
        opts.trash = true;
        break;
      case "--no-measure":
        opts.measure = false;
        break;
      case "--measure":
        opts.measure = true;
        break;
      case "--json":
        opts.json = true;
        break;
      case "-y":
      case "--yes":
        opts.yes = true;
        break;
      default:
        if (arg.startsWith("-")) {
          process.stderr.write(`rnmn: unknown option ${arg}\n\n${HELP}`);
          process.exit(2);
        }
        if (opts.root !== undefined) {
          process.stderr.write(`rnmn: unexpected extra argument ${arg}\n`);
          process.exit(2);
        }
        opts.root = arg;
    }
  }
  return opts;
}

function formatBytes(n) {
  const bytes = typeof n === "bigint" ? Number(n) : n;
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exp;
  return `${value.toFixed(value >= 10 || exp === 0 ? 0 : 1)} ${units[exp]}`;
}

function relativizePath(root, p) {
  if (p === root) return ".";
  if (p.startsWith(root + "/")) return p.slice(root.length + 1);
  return p;
}

// Confirmation prompt (synchronous) so a bare `rnmn` in a real repo can't nuke
// node_modules by a stray keystroke. Skipped with -y, with --list, or when not
// attached to a TTY (CI / piped).
function confirm(question) {
  if (!process.stdin.isTTY) return true;
  const fs = require("fs");
  process.stdout.write(question);
  const buf = Buffer.alloc(64);
  let bytesRead = 0;
  try {
    bytesRead = fs.readSync(0, buf, 0, buf.length, null);
  } catch {
    return false;
  }
  const answer = buf.toString("utf8", 0, bytesRead).trim().toLowerCase();
  return answer === "y" || answer === "yes";
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    process.stdout.write(HELP);
    return;
  }

  // Phase 1: dry-run scan so we can show the user exactly what will go, then
  // (unless -y / non-TTY) confirm before the real deletion.
  const scan = clean({ root: opts.root, dryRun: true, measure: opts.measure });

  const kindLabel =
    scan.workspaceKind === "none"
      ? "no workspace config"
      : `${scan.workspaceKind} workspace (${scan.workspacePackages.length} package${
          scan.workspacePackages.length === 1 ? "" : "s"
        })`;

  if (opts.json && opts.list) {
    process.stdout.write(JSON.stringify(scan, replacer, 2) + "\n");
    return;
  }

  if (scan.count === 0) {
    process.stdout.write(`rnmn: no node_modules found under ${scan.root} (${kindLabel})\n`);
    return;
  }

  process.stdout.write(`root: ${scan.root}\n`);
  process.stdout.write(`      ${kindLabel}\n`);
  process.stdout.write(
    `found ${scan.count} node_modules${
      opts.measure ? ` totalling ${formatBytes(scan.totalBytes)}` : ""
    }:\n`,
  );
  for (const dir of scan.cleaned) {
    const size = opts.measure ? `  ${formatBytes(dir.bytes).padStart(8)}` : "";
    process.stdout.write(`${size}  ${relativizePath(scan.root, dir.path)}\n`);
  }

  if (opts.list) {
    process.stdout.write(`\n(list only — nothing ${opts.trash ? "trashed" : "deleted"})\n`);
    return;
  }

  const verb = opts.trash ? "move to Trash" : "permanently delete";
  if (!opts.yes && !confirm(`\n${verb} these ${scan.count} directories? [y/N] `)) {
    process.stdout.write("aborted.\n");
    process.exit(1);
  }

  // Phase 2: real disposal. Re-measure is unnecessary — reuse sizes we have.
  const start = process.hrtime.bigint();
  const result = clean({ root: opts.root, dryRun: false, measure: false, trash: opts.trash });
  const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;

  if (opts.json) {
    process.stdout.write(JSON.stringify(result, replacer, 2) + "\n");
    return;
  }

  const done = result.count - result.failed;
  const trashedCount = result.cleaned.filter((d) => d.trashed).length;
  // Report how the space went: "trashed" (recoverable, empty Trash to reclaim)
  // vs "deleted" (gone). A trash run that fell back to hard-remove for some
  // items is noted so the count still adds up.
  let action;
  if (!opts.trash) {
    action = "deleted";
  } else if (trashedCount === done) {
    action = "moved to Trash";
  } else {
    action = `moved to Trash (${done - trashedCount} hard-deleted)`;
  }
  process.stdout.write(
    `\n${action}: ${done}/${result.count} node_modules${
      opts.measure ? ` (${formatBytes(scan.totalBytes)})` : ""
    } in ${elapsedMs.toFixed(0)}ms\n`,
  );
  if (opts.trash && trashedCount > 0) {
    process.stdout.write(`empty the Trash to reclaim the space (or re-run without -t to delete).\n`);
  }
  if (result.failed > 0) {
    for (const dir of result.cleaned) {
      if (dir.error) {
        process.stderr.write(`  failed: ${relativizePath(result.root, dir.path)} — ${dir.error}\n`);
      }
    }
    process.exit(1);
  }
}

function replacer(_key, value) {
  return typeof value === "bigint" ? Number(value) : value;
}

main();
