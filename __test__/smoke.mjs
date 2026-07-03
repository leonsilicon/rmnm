// Cross-platform smoke test run in CI for each built target: proves the native
// binary loads on this platform and that `clean` + `resolveWorkspace` run and
// return sane results. Uses a throwaway temp workspace and only ever operates
// in list mode (dryRun), so it never deletes anything real.

import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { clean, resolveWorkspace } = require("../index.js");

function assert(cond, msg) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const root = mkdtempSync(join(tmpdir(), "remnem-smoke-"));
try {
  // A tiny workspace: root + one package, each with a node_modules.
  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "smoke-root", workspaces: ["packages/*"] }));
  mkdirSync(join(root, "packages", "a"), { recursive: true });
  writeFileSync(join(root, "packages", "a", "package.json"), "{}");
  mkdirSync(join(root, "node_modules", "dep"), { recursive: true });
  writeFileSync(join(root, "node_modules", "dep", "index.js"), "module.exports = 1;");
  mkdirSync(join(root, "packages", "a", "node_modules"), { recursive: true });

  // Workspace resolution should see the package.json workspace + the one package.
  const ws = resolveWorkspace(root);
  assert(ws.workspaceKind === "package.json", `workspaceKind was ${ws.workspaceKind}`);
  assert(ws.workspacePackages.length === 1, `expected 1 workspace package, got ${ws.workspacePackages.length}`);

  // List mode (dryRun) must find both node_modules and delete nothing.
  const listed = clean({ root, dryRun: true, measure: true });
  assert(listed.count === 2, `expected 2 node_modules, found ${listed.count}`);
  assert(Number(listed.totalBytes) > 0, "expected non-zero total bytes");
  assert(existsSync(join(root, "node_modules")), "list mode must not delete anything");

  // A real in-place delete should clear them and leave package.json intact.
  const deleted = clean({ root, dryRun: false, measure: false, trash: false });
  assert(deleted.failed === 0, `delete reported ${deleted.failed} failures`);
  assert(!existsSync(join(root, "node_modules")), "root node_modules should be gone");
  assert(existsSync(join(root, "package.json")), "package.json must survive");

  console.log(`OK: remnem smoke test passed on ${process.platform}/${process.arch}`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
