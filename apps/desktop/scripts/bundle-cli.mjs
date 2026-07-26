// Build `dbsync` and stage it where Tauri expects an external binary.
//
// The app offers a crontab line for every schedule, and that line invokes
// `dbsync`. Shipping the CLI inside the bundle is what makes that offer real:
// otherwise the first thing a user is told is to go and obtain a second
// binary that was never published anywhere.
//
// Tauri resolves external binaries by target triple and strips the suffix when
// bundling, so the file has to be named `dbsync-<triple>` (plus `.exe` on
// Windows). Written in Node rather than shell because it runs on macOS,
// Windows and Linux from `beforeBuildCommand`.

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, existsSync, chmodSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const desktop = resolve(here, "..");
const workspace = resolve(desktop, "../..");
const outDir = join(desktop, "src-tauri", "binaries");

/**
 * The triple to build for.
 *
 * Tauri exports TAURI_ENV_TARGET_TRIPLE when it is cross-compiling — notably
 * for macOS universal builds, which invoke this once per architecture. Falling
 * back to the host triple keeps a plain `npm run bundle:cli` working.
 */
function targetTriple() {
  const fromTauri = process.env.TAURI_ENV_TARGET_TRIPLE;
  if (fromTauri) return fromTauri;

  const verbose = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const host = verbose.split("\n").find((l) => l.startsWith("host:"));
  if (!host) throw new Error("could not read the host triple from `rustc -vV`");
  return host.slice("host:".length).trim();
}

function main() {
  const triple = targetTriple();
  const isWindows = triple.includes("windows");
  const exe = isWindows ? ".exe" : "";

  // Only pass --target when cross-compiling. Passing the host triple
  // explicitly would move the output into target/<triple>/ and defeat the
  // build cache shared with every other cargo invocation in this repo.
  const hostTriple = execFileSync("rustc", ["-vV"], { encoding: "utf8" })
    .split("\n")
    .find((l) => l.startsWith("host:"))
    .slice("host:".length)
    .trim();
  const cross = triple !== hostTriple;

  const args = ["build", "--release", "-p", "db-sync-cli"];
  if (cross) args.push("--target", triple);

  console.log(`building dbsync for ${triple}${cross ? " (cross)" : ""}`);
  execFileSync("cargo", args, { cwd: workspace, stdio: "inherit" });

  const built = cross
    ? join(workspace, "target", triple, "release", `dbsync${exe}`)
    : join(workspace, "target", "release", `dbsync${exe}`);

  if (!existsSync(built)) {
    throw new Error(`cargo reported success but ${built} is missing`);
  }

  mkdirSync(outDir, { recursive: true });
  const staged = join(outDir, `dbsync-${triple}${exe}`);
  copyFileSync(built, staged);
  // copyFileSync preserves content, not the executable bit on every platform.
  if (!isWindows) chmodSync(staged, 0o755);

  console.log(`staged ${staged}`);
}

main();
