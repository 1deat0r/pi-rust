#!/usr/bin/env node
// Parity suite — P9 deliverable.
//
// Runs the workspace test suite plus the CLI matrix smoke against the real
// `pi` binary (version/help, faux-provider one-shot, RPC round-trip incl.
// export-html), reporting pass/fail per step. Exit code is nonzero when any
// step fails.
//
// Usage:
//   node scripts/parity-suite.mjs [--no-build] [--binary path/to/pi]
//
// The default binary is target/release/pi (built by this script unless
// --no-build is passed).

import { spawnSync, spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const noBuild = args.includes("--no-build");
const binaryIndex = args.indexOf("--binary");
const binaryPath = binaryIndex >= 0 ? resolve(packageRoot, args[binaryIndex + 1]) : join(packageRoot, "target", "release", "pi");

let failures = 0;
const results = [];

function check(name, ok, detail = "") {
  results.push({ name, ok, detail });
  if (!ok) failures += 1;
  const marker = ok ? "PASS" : "FAIL";
  console.log(`[${marker}] ${name}${detail ? ` — ${detail}` : ""}`);
}

function run(cmd, argv, options = {}) {
  const result = spawnSync(cmd, argv, { encoding: "utf8", ...options });
  if (result.error) throw result.error;
  return result;
}

function step(name, fn) {
  try {
    fn();
  } catch (error) {
    check(name, false, error.message);
  }
}

// ---------------------------------------------------------------------------
// 0. CLI parsing
// ---------------------------------------------------------------------------

if (!existsSync(binaryPath)) {
  console.error(`[ERROR] pi binary not found at ${binaryPath}`);
  console.error("Run `cargo build --release -p pi-coding-agent` first (or pass --binary).");
  process.exit(2);
}

function tempDir(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

// ---------------------------------------------------------------------------
// 1. Release binary check
// ---------------------------------------------------------------------------

console.log("\n== 1. Release binary ==");
check("target/release/pi exists", existsSync(binaryPath), binaryPath);

let piVersion = "unknown";
step("pi --version", () => {
  const { stdout, status } = run(binaryPath, ["--version"]);
  const expected = /^pi \d+\.\d+\.\d+/;
  piVersion = stdout.trim();
  check("pi --version prints version", expected.test(stdout) && status === 0, stdout.trim());
});

step("pi --help lists the CLI surface", () => {
  const { stdout, status } = run(binaryPath, ["--help"]);
  const hasUsage = stdout.includes("Usage:");
  const hasProvider = stdout.includes("--provider");
  const hasPrint = stdout.includes("--print") || stdout.includes("-p");
  check(
    "pi --help lists usage/options",
    status === 0 && hasUsage && hasProvider && hasPrint,
    hasUsage && hasProvider && hasPrint ? "usage + provider + print present" : "missing expected sections",
  );
});

// ---------------------------------------------------------------------------
// 2. Workspace test suite
// ---------------------------------------------------------------------------

console.log("\n== 2. Workspace test suite ==");
step("cargo test --workspace", () => {
  const { stdout, status } = run("cargo", ["test", "--workspace"], { cwd: packageRoot, maxBuffer: 64 * 1024 * 1024 });
  const summary = (stdout.match(/test result: ok\. \d+ passed/g) || []).length;
  check("cargo test --workspace passes", status === 0 && summary > 0, `${summary} ok test binaries`);
});

// ---------------------------------------------------------------------------
// 3. CLI matrix
// ---------------------------------------------------------------------------

console.log("\n== 3. CLI matrix (faux provider) ==");

step("faux-provider one-shot run", () => {
  const { stdout, status } = run(binaryPath, ["-p", "--provider", "faux", "--model", "faux-1", "hello parity"], {
    timeout: 30_000,
  });
  const ok = status === 0 && stdout.includes("faux response to: hello parity");
  check("faux run replies 'faux response to: …'", ok, ok ? stdout.trim() : `stdout: ${stdout}`);
});

async function rpcRoundTrip() {
  const workDir = tempDir("pi-parity-rpc-");
  const agentDir = join(workDir, "agent");
  const exportPath = join(workDir, "export.html");
  const commands = [
    { type: "get_state" },
    { type: "set_thinking_level", level: "high" },
    { type: "set_session_name", name: "parity-suite" },
    { type: "get_state" },
    { type: "prompt", message: "hello" },
    { id: "1", type: "get_messages" },
    { type: "export_html", outputPath: exportPath },
  ].map((command) => `${JSON.stringify(command)}\n`).join("");

  const child = spawn(binaryPath, ["--mode", "rpc", "--provider", "faux", "--model", "faux-1", "--no-tools"], {
    cwd: workDir,
    env: {
      ...process.env,
      PI_CODING_AGENT_DIR: agentDir,
      PI_OFFLINE: "1",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });

  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });

  const exitCode = await new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`rpc round-trip timed out; stderr: ${stderr.slice(0, 500)}`));
    }, 60_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolvePromise(code ?? -1);
    });
    child.stdin.write(commands);
    child.stdin.end();
  });

  const lines = stdout.split("\n").filter((line) => line.trim().length > 0).map((line) => JSON.parse(line));
  const responses = lines.filter((line) => line.type === "response");

  const getState = responses.find((line) => line.command === "get_state");
  const thinking = responses.find((line) => line.command === "set_thinking_level");
  const nameSet = responses.find((line) => line.command === "set_session_name");
  const messages = responses.find((line) => line.command === "get_messages");
  const exportHtml = responses.find((line) => line.command === "export_html");
  const settled = lines.some((line) => line.type === "agent_settled");

  const getState2 = responses.filter((line) => line.command === "get_state")[1];
  const ok =
    exitCode === 0 &&
    getState?.success === true &&
    typeof getState.data?.sessionId === "string" &&
    thinking?.success === true &&
    nameSet?.success === true &&
    getState2?.data?.sessionName === "parity-suite" &&
    getState2?.data?.thinkingLevel === "high" &&
    settled &&
    messages?.success === true &&
    Array.isArray(messages.data?.messages) &&
    messages.data.messages.at(-1)?.content?.some?.((part) => part.text === "faux response to: hello") &&
    exportHtml?.success === true &&
    existsSync(exportPath) &&
    readFileSync(exportPath, "utf8").includes("<html");

  check(
    "rpc round-trip (state/name/thinking/prompt/messages/export-html)",
    ok,
    ok ? `export written (${existsSync(exportPath) ? readFileSync(exportPath, "utf8").length : 0} bytes)` : `last response: ${JSON.stringify(responses.at(-1))}`,
  );
  rmSync(workDir, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// 4. Summary
// ---------------------------------------------------------------------------

async function main() {
  try {
    await rpcRoundTrip();
  } catch (error) {
    check("rpc round-trip (state/name/thinking/prompt/messages/export-html)", false, error.message);
  }
  console.log("\n== Parity suite summary ==");
  for (const result of results) {
    console.log(`  ${result.ok ? "✓" : "✗"} ${result.name}`);
  }
  console.log(`\n${results.length - failures}/${results.length} steps passed`);
  if (failures > 0) {
    process.exit(1);
  }
}

main();

// -- helper ----------------------------------------------------------------
function dirname(path) {
  const normalized = path.replace(/[\\/]+$/, "");
  const lastSlash = Math.max(normalized.lastIndexOf("/"), normalized.lastIndexOf("\\"));
  return lastSlash < 0 ? "." : normalized.slice(0, lastSlash);
}
