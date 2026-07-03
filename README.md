# remnem

**r**e**m**ove **n**ode_**m**odules — find every nested `node_modules` in a project (root + all workspaces + any nested ones) and delete them all, **instantly**.

A single self-contained CLI written in **Rust**: a lean, parallel, directory-only walker finds every `node_modules`, then each one is disposed of by an **O(1) rename** rather than a slow recursive unlink — so clearing a 1000-package monorepo takes a couple hundred milliseconds instead of tens of seconds.

```
$ remnem
root: /Users/you/dev/my-monorepo
found 1947 node_modules:
  node_modules
  apps/web/node_modules
  ...
permanently delete these 1947 directories? [y/N] y

deleted: 1947/1947 node_modules in 130ms
(space is being reclaimed in the background)
```

## How it's instant

Physically deleting `node_modules` means unlinking hundreds of thousands of
files — that is I/O-bound and unavoidably slow (tens of seconds on a big
monorepo). remnem sidesteps that on the critical path:

1. **Find** — a parallel, directory-only walk. It reads directory entries via
   `readdir`'s `d_type` (no per-file `stat`), never looks at regular files, and
   never descends *into* a `node_modules` (the whole subtree is going anyway).
   So the walk is proportional to your *source* tree, not the installed
   dependency tree.
2. **Rename out of the repo** — each `node_modules` is `rename`d out of the
   repository entirely, into a per-run staging directory under the OS temp dir
   (`$TMPDIR/remnem-<pid>/`). On one filesystem that is an O(1) metadata
   operation no matter how large the tree is. The instant it returns,
   `node_modules` is gone from its location — **a clean reinstall can start
   immediately** — and because the staged copy lives outside the repo, it can
   never be picked up by `git status` / `git add`.
3. **Reclaim in the background** — a detached background process `rm -rf`s the
   staging directory, so the disk-freeing I/O never blocks you. Space comes back
   within a few seconds, hands-free.

Pass **`--sync`** if you'd rather block until the space is actually reclaimed
(e.g. a script that measures free disk right after) — that mode deletes in place
instead of staging.

If the OS temp dir happens to be on a *different* filesystem than the repo (so a
`rename` would need a slow cross-device copy), remnem detects it and falls back
to a synchronous in-place delete — still never leaving anything in the tree.

## Install

```sh
npm install -g remnem
# or: bun install -g remnem
```

The right prebuilt binary is pulled in automatically for your platform via
`optionalDependencies` — the main `remnem` package is a tiny launcher that execs
it. Supported: **macOS** (arm64, x64), **Linux** (arm64 & x64, glibc & musl),
**Windows** (arm64, x64).

Then from any repo root:

```sh
remnem            # or: npx remnem  /  bunx remnem
```

### From source

```sh
cargo build --release       # produces target/release/remnem
./target/release/remnem --help
```

## What it clears

**Every `node_modules` directory** under the given root — the root's own, every
workspace package's, and any stray nested ones — leaving all your source and
`package.json` files untouched.

## Usage

```
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
```

By default `remnem` deletes each `node_modules` after printing what it found and
asking for confirmation (skipped with `-y`, or when stdin isn't a TTY, e.g. in
CI). Use `-l` to list without touching anything, or `--sync` to wait for the
space to be reclaimed before returning.

Sizing (`-m`) and workspace-layout resolution (`-w`) each require an extra tree
walk, so they are **off by default** — the fast path does neither.

## Workspace resolution (`-w`)

With `-w`, `remnem` reports the workspace layout the way bun and pnpm resolve it:

| Source | Field | Example |
| --- | --- | --- |
| bun / npm / yarn | `package.json` → `workspaces` | `["packages/*", "!packages/excluded"]` |
| bun / npm / yarn | `package.json` → `workspaces.packages` | `{ "packages": ["libs/*"] }` |
| pnpm | `pnpm-workspace.yaml` → `packages` | `- 'packages/*'`<br>`- '!**/test/**'` |

Glob semantics match [picomatch](https://github.com/micromatch/picomatch) (the
matcher bun/npm/yarn use):

- `*` matches exactly one path segment (`packages/*` → `packages/a`, not `packages/a/b`)
- `**` matches any number of segments, and a trailing `/**` is **optional**
  (`components/**` matches `components` itself and everything beneath it)
- `!pattern` excludes previously-matched directories (`!**/test/**` drops a
  directory named `test` and its contents)

This is purely informational: clearing always targets every nested
`node_modules`, not only workspace packages.

## Development

```sh
cargo test                 # Rust unit tests (workspace resolution + glob semantics)
cargo build --release      # release build (LTO)
node __test__/smoke.mjs ./target/release/remnem   # end-to-end smoke test
```

## License

MIT
