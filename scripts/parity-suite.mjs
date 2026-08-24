#!/usr/bin/env node
// Deterministic leaf-F2 release evidence.
//
// The suite is intentionally fixture-driven. Every declared offline case is
// an exact comparison or a declared structural projection of the pinned
// upstream oracle. Dynamic values are normalized only at paths named by the
// fixture; a failing comparison remains a failure.

import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const piCodingParityRoot = join(packageRoot, "crates", "pi-coding-agent", "tests", "fixtures", "parity");
const piAiParityRoot = join(packageRoot, "crates", "pi-ai", "tests", "fixtures", "parity");
const piAgentParityRoot = join(packageRoot, "crates", "pi-agent", "tests", "fixtures", "parity");
const upstreamRoot = join(packageRoot, "upstream_pi");
const upstreamCommit = "5cd93f688aaab89dbb6dfa4aca535f21796ae185";
const cargoBinary = process.env.PI_PARITY_CARGO || "/home/mustbearnold/.cargo/bin/cargo";

const cliArgs = process.argv.slice(2);
const noBuild = cliArgs.includes("--no-build");
const liveRequested = cliArgs.includes("--live");
const binaryIndex = cliArgs.indexOf("--binary");
if (binaryIndex >= 0 && !cliArgs[binaryIndex + 1]) {
  console.error("--binary requires a path");
  process.exit(2);
}
const binaryPath = binaryIndex >= 0
  ? resolve(packageRoot, cliArgs[binaryIndex + 1])
  : join(packageRoot, "target", "release", "pi");

const allowedEvidenceTiers = new Set(["unit", "mock", "live"]);
const failures = [];
const results = [];
const matrixStats = {
  offline: { declared: 0, passed: 0, failed: 0, notRun: 0 },
  live: { declared: 0, passed: 0, failed: 0, notRun: 0 },
};
const providerStats = { declared: 0, passed: 0, failed: 0 };

function tempDir(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

function runCommand(program, args, options = {}) {
  const command = program === "cargo" ? cargoBinary : program;
  const result = spawnSync(command, args, {
    cwd: packageRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
  return {
    status: result.status ?? -1,
    stdout: result.stdout || "",
    stderr: result.stderr || "",
    signal: result.signal || null,
    error: result.error?.message || null,
  };
}

function commandText(program, args) {
  return [program === "cargo" ? cargoBinary : program, ...args]
    .map((part) => (part.includes(" ") ? JSON.stringify(part) : part))
    .join(" ");
}

function shortText(value, limit = 900) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  return text.length > limit ? `${text.slice(0, limit)}…` : text;
}

function record(category, id, status, detail, options = {}) {
  const {
    evidenceTier = "unit",
    required = true,
    matrix = true,
  } = options;
  if (!allowedEvidenceTiers.has(evidenceTier)) {
    status = "fail";
    detail = `invalid evidence tier ${JSON.stringify(evidenceTier)}; ${detail}`;
  }
  const result = { category, id, status, detail, evidenceTier, required, matrix };
  results.push(result);
  if (matrix) {
    const bucket = evidenceTier === "live" ? matrixStats.live : matrixStats.offline;
    bucket.declared += 1;
    if (status === "pass") bucket.passed += 1;
    else if (status === "fail") bucket.failed += 1;
    else if (status === "not-run") bucket.notRun += 1;
  }
  if (status === "fail" && required) failures.push(result);
  const marker = status === "pass" ? "PASS" : status === "not-run" ? "NOT RUN" : "FAIL";
  console.log(`[${marker}] ${category}/${id}${detail ? ` — ${detail}` : ""}`);
}

function loadJson(path, label) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    record("fixture", label, "fail", `${path}: ${error.message}`, { matrix: false });
    return null;
  }
}

function validateOracle(meta, label) {
  const oracle = meta?.upstream_oracle;
  if (!oracle || oracle.commit !== upstreamCommit || !Array.isArray(oracle.paths) || oracle.paths.length === 0) {
    throw new Error(`${label}: missing or incorrect pinned upstream_oracle metadata`);
  }
  for (const path of oracle.paths) {
    if (!existsSync(join(upstreamRoot, path))) throw new Error(`${label}: upstream oracle path is absent: ${path}`);
  }
  if (!allowedEvidenceTiers.has(meta.evidence_tier)) throw new Error(`${label}: invalid evidence tier ${meta.evidence_tier}`);
}

function normalizeSandboxText(text, sandbox) {
  return String(text || "")
    .replaceAll(sandbox, "<sandbox>")
    .replaceAll(join(packageRoot, "target", "release"), "<release>")
    .replaceAll(join(packageRoot, "target", "debug"), "<debug>");
}

function getPath(value, path) {
  let current = value;
  const tokens = [];
  const tokenPattern = /([^.[\]]+)|\[(\d+)\]/g;
  let match;
  while ((match = tokenPattern.exec(path)) !== null) tokens.push(match[1] ?? Number(match[2]));
  for (const token of tokens) {
    if (current === null || current === undefined) return undefined;
    current = current[token];
  }
  return current;
}

function sameValue(actual, expected) {
  return JSON.stringify(actual) === JSON.stringify(expected);
}

function matchesFields(value, fields) {
  return Object.entries(fields || {}).every(([path, expected]) => sameValue(getPath(value, path), expected));
}

function matchSequence(records, sequence) {
  let index = 0;
  for (const pattern of sequence) {
    if (pattern.repeat === "one_or_more") {
      let count = 0;
      while (index < records.length && matchesFields(records[index], pattern.fields)) {
        index += 1;
        count += 1;
      }
      if (count === 0) return `expected one or more records matching ${JSON.stringify(pattern.fields)} at index ${index}`;
      continue;
    }
    if (index >= records.length) return `missing record ${index}: ${JSON.stringify(pattern.fields)}`;
    if (!matchesFields(records[index], pattern.fields)) return `record ${index} mismatch: expected ${JSON.stringify(pattern.fields)}, got ${JSON.stringify(records[index])}`;
    index += 1;
  }
  return index === records.length ? null : `unexpected records after index ${index}: ${JSON.stringify(records.slice(index))}`;
}

function compareText(actual, spec) {
  if (!spec || typeof spec.mode !== "string") return "missing text comparison specification";
  if (spec.mode === "exact") return actual === spec.value ? null : `expected ${JSON.stringify(spec.value)}, got ${JSON.stringify(actual)}`;
  if (spec.mode === "contains") {
    const missing = (spec.values || []).filter((value) => !actual.includes(value));
    return missing.length === 0 ? null : `missing substrings: ${missing.join(", ")}`;
  }
  if (spec.mode === "regex") {
    let expression;
    try {
      expression = new RegExp(spec.value, "s");
    } catch (error) {
      return `invalid fixture regex: ${error.message}`;
    }
    return expression.test(actual) ? null : `regex ${JSON.stringify(spec.value)} did not match ${JSON.stringify(actual)}`;
  }
  if (spec.mode === "jsonl-sequence") {
    const records = [];
    for (const [lineNumber, line] of actual.split("\n").filter((line) => line.trim()).entries()) {
      try {
        records.push(JSON.parse(line));
      } catch (error) {
        return `line ${lineNumber + 1} is not JSON: ${error.message}`;
      }
    }
    return matchSequence(records, spec.sequence || []);
  }
  return `unknown text comparison mode ${spec.mode}`;
}

function sandboxEnvironment(sandbox, agentDir, sessionDir) {
  mkdirSync(join(sandbox, "home"), { recursive: true });
  mkdirSync(agentDir, { recursive: true });
  mkdirSync(sessionDir, { recursive: true });
  const environment = {
    ...process.env,
    HOME: join(sandbox, "home"),
    PI_CODING_AGENT_DIR: agentDir,
    PI_CODING_AGENT_SESSION_DIR: sessionDir,
    PI_OFFLINE: "1",
  };
  delete environment.PI_PROVIDER;
  delete environment.PI_MODEL;
  delete environment.PI_KEY;
  delete environment.PI_SESSION_ID;
  return environment;
}

async function runRpc(binary, args, input, sandbox) {
  const agentDir = join(sandbox, "agent");
  const sessionDir = join(sandbox, "sessions");
  const env = sandboxEnvironment(sandbox, agentDir, sessionDir);
  return await new Promise((resolvePromise) => {
    let stdout = "";
    let stderr = "";
    let settled = false;
    const child = spawn(binary, args, { cwd: sandbox, env, stdio: ["pipe", "pipe", "pipe"] });
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      finish({ status: -1, timedOut: true });
    }, 60_000);
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolvePromise({ ...result, stdout, stderr });
    };
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => finish({ status: -1, error: error.message }));
    child.on("close", (status, signal) => finish({ status: status ?? -1, signal }));
    child.stdin.end(input);
  });
}

function parseJsonl(text, label) {
  const records = [];
  for (const [index, line] of text.split("\n").filter((line) => line.trim()).entries()) {
    try {
      records.push(JSON.parse(line));
    } catch (error) {
      throw new Error(`${label} line ${index + 1} is malformed JSON: ${error.message}`);
    }
  }
  return records;
}

function canonicalJsonl(text, label, strict = true) {
  if (!text.endsWith("\n")) throw new Error(`${label} does not end with LF`);
  const lines = text.slice(0, -1).split("\n");
  if (lines.some((line) => line.length === 0)) throw new Error(`${label} contains an empty JSONL line`);
  const records = lines.map((line, index) => {
    try {
      return JSON.parse(line);
    } catch (error) {
      throw new Error(`${label} line ${index + 1}: ${error.message}`);
    }
  });
  const canonical = records.map((record) => `${JSON.stringify(record)}\n`).join("");
  if (strict && canonical !== text) throw new Error(`${label} is not compact canonical JSONL bytes`);
  return records;
}

function messageText(message) {
  if (!message) return "";
  if (typeof message.content === "string") return message.content;
  if (!Array.isArray(message.content)) return "";
  return message.content.filter((part) => part?.type === "text" && typeof part.text === "string").map((part) => part.text).join("");
}

function assertSessionRecords(records, expected, label, mode) {
  if (!Array.isArray(records) || records.length < 1) throw new Error(`${label} has no records`);
  const header = records[0];
  if (header.kind !== "header" || header.version !== expected.header_version) throw new Error(`${label} header is not v${expected.header_version}: ${JSON.stringify(header)}`);
  if (header.id !== expected.header_id) throw new Error(`${label} header id ${header.id} != ${expected.header_id}`);
  if (expected.created_at !== undefined && header.createdAt !== expected.created_at) throw new Error(`${label} createdAt ${header.createdAt} != ${expected.created_at}`);
  if (expected.cwd && header.cwd !== expected.cwd) throw new Error(`${label} cwd ${header.cwd} != ${expected.cwd}`);
  const entries = records.filter((record) => record.kind === "entry");
  const expectedCount = expected.entry_ids?.length || expected.entry_count_at_least || expected.entry_types?.length || 0;
  if (entries.length < expectedCount) throw new Error(`${label} has ${entries.length} entries, expected at least ${expectedCount}`);
  if (expected.entry_ids) expected.entry_ids.forEach((id, index) => {
    if (entries[index]?.id !== id) throw new Error(`${label} entry ${index} id ${entries[index]?.id} != ${id}`);
  });
  if (expected.ids === "generated-8-lowercase-hex") for (const entry of entries.slice(0, expected.entry_count_at_least)) {
    if (!/^[0-9a-f]{8}$/.test(entry.id)) throw new Error(`${label} generated id is not 8 lowercase hex: ${entry.id}`);
  }
  if (expected.entry_types) expected.entry_types.forEach((type, index) => {
    if (entries[index]?.type !== type) throw new Error(`${label} entry ${index} type ${entries[index]?.type} != ${type}`);
  });
  if (expected.sequence) expected.sequence.forEach((seq, index) => {
    if (entries[index]?.seq !== seq) throw new Error(`${label} entry ${index} seq ${entries[index]?.seq} != ${seq}`);
  });
  if (expected.parent_chain) for (let index = 0; index < Math.min(entries.length, Math.max(expectedCount, 2)); index += 1) {
    const expectedParent = index === 0 ? null : entries[index - 1].id;
    if (entries[index]?.parentId !== expectedParent) throw new Error(`${label} entry ${index} parent ${entries[index]?.parentId} != ${expectedParent}`);
  }
  if (expected.custom_role_at_entry !== undefined && entries[expected.custom_role_at_entry]?.message?.role !== "custom") throw new Error(`${label} custom role migration missing`);
  if (expected.user_text !== undefined && messageText(entries[0]?.message) !== expected.user_text) throw new Error(`${label} user text mismatch: ${JSON.stringify(messageText(entries[0]?.message))}`);
  if (expected.assistant_text !== undefined && messageText(entries[1]?.message) !== expected.assistant_text) throw new Error(`${label} assistant text mismatch: ${JSON.stringify(messageText(entries[1]?.message))}`);
  if (mode === "current-write" && expected.cwd_is_sandbox && !header.cwd.startsWith("/tmp/pi-parity-current-") && !header.cwd.startsWith("/tmp/pi-parity-session-current-")) throw new Error(`${label} current write cwd was not isolated: ${header.cwd}`);
}

function findJsonlFiles(root) {
  const files = [];
  if (!existsSync(root)) return files;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isFile() && entry.name.endsWith(".jsonl")) files.push(path);
    else if (entry.isDirectory()) files.push(...findJsonlFiles(path));
  }
  return files;
}

function readFixtureBytes(path, label) {
  const bytes = readFileSync(path);
  if (bytes.length === 0) throw new Error(`${label} is empty`);
  return bytes;
}

function validatePathExists(value, path, label) {
  if (getPath(value, path) === undefined) throw new Error(`${label} missing path ${path}`);
}

function runFocusedCommands(matrix, category) {
  for (const command of matrix.focused_commands || []) {
    const args = command.args || [];
    const result = runCommand(command.program, args, { timeout: 600_000 });
    const ok = result.status === 0;
    record(category, command.id, ok ? "pass" : "fail", ok ? commandText(command.program, args) : `${commandText(command.program, args)}; ${shortText(result.stderr || result.stdout || result.error)}`, {
      evidenceTier: matrix.evidence_tier,
      matrix: true,
    });
  }
}

function runCliMatrix(binary, fixture) {
  if (!fixture) return;
  try {
    validateOracle(fixture, "cli");
    if (!Array.isArray(fixture.cases) || fixture.cases.length === 0) throw new Error("CLI matrix has no cases");
  } catch (error) {
    record("cli", "fixture-metadata", "fail", error.message, { evidenceTier: fixture?.evidence_tier || "unit" });
    return;
  }
  for (const testCase of fixture.cases) {
    const sandbox = tempDir(`pi-parity-cli-${testCase.id}-`);
    try {
      if (!allowedEvidenceTiers.has(testCase.evidence_tier)) throw new Error("case has no valid evidence_tier");
      const result = runCommand(binary, testCase.args || [], {
        cwd: sandbox,
        env: sandboxEnvironment(sandbox, join(sandbox, "agent"), join(sandbox, "sessions")),
        timeout: 60_000,
      });
      const stdout = normalizeSandboxText(result.stdout, sandbox);
      const stderr = normalizeSandboxText(result.stderr, sandbox);
      const errors = [];
      if (result.status !== testCase.expected_exit) errors.push(`exit ${result.status} != ${testCase.expected_exit}`);
      const stdoutError = compareText(stdout, testCase.stdout);
      const stderrError = compareText(stderr, testCase.stderr);
      if (stdoutError) errors.push(`stdout: ${stdoutError}`);
      if (stderrError) errors.push(`stderr: ${stderrError}`);
      record("cli", testCase.id, errors.length === 0 ? "pass" : "fail", errors.length === 0 ? `exit ${result.status}` : errors.join("; "), {
        evidenceTier: testCase.evidence_tier,
        matrix: true,
      });
    } catch (error) {
      record("cli", testCase.id, "fail", error.message, { evidenceTier: testCase.evidence_tier || fixture.evidence_tier });
    } finally {
      rmSync(sandbox, { recursive: true, force: true });
    }
  }
}

async function runRpcMatrix(binary, fixture) {
  if (!fixture) return;
  try {
    validateOracle(fixture, "rpc");
    if (!Array.isArray(fixture.transcripts) || fixture.transcripts.length === 0) throw new Error("RPC matrix has no transcripts");
  } catch (error) {
    record("rpc", "fixture-metadata", "fail", error.message, { evidenceTier: fixture?.evidence_tier || "mock" });
    return;
  }
  for (const transcript of fixture.transcripts) {
    const sandbox = tempDir(`pi-parity-rpc-${transcript.id}-`);
    try {
      const input = (transcript.input || []).map((line) => `${typeof line === "string" ? line : JSON.stringify(line)}\n`).join("");
      const result = await runRpc(binary, transcript.args || [], input, sandbox);
      const records = parseJsonl(result.stdout, transcript.id);
      const errors = [];
      if (result.status !== transcript.expected_exit) errors.push(`exit ${result.status} != ${transcript.expected_exit}`);
      const sequenceError = matchSequence(records, transcript.expected?.sequence || []);
      if (sequenceError) errors.push(sequenceError);
      if (result.stderr.trim()) errors.push(`stderr is not empty: ${shortText(result.stderr)}`);
      record("rpc", transcript.id, errors.length === 0 ? "pass" : "fail", errors.length === 0 ? `${records.length} JSONL records` : errors.join("; "), {
        evidenceTier: transcript.evidence_tier,
        matrix: true,
      });
    } catch (error) {
      record("rpc", transcript.id, "fail", error.message, { evidenceTier: transcript.evidence_tier || fixture.evidence_tier });
    } finally {
      rmSync(sandbox, { recursive: true, force: true });
    }
  }
}

function runSessionMatrix(binary, fixture) {
  if (!fixture) return;
  try {
    validateOracle(fixture, "session");
    if (!Array.isArray(fixture.fixtures) || fixture.fixtures.length === 0) throw new Error("session matrix has no fixtures");
  } catch (error) {
    record("session", "fixture-metadata", "fail", error.message, { evidenceTier: fixture?.evidence_tier || "unit" });
    return;
  }
  for (const sessionFixture of fixture.fixtures) {
    const evidenceTier = sessionFixture.evidence_tier || fixture.evidence_tier;
    try {
      if (!allowedEvidenceTiers.has(evidenceTier)) throw new Error("session fixture has no valid evidence_tier");
      if (sessionFixture.mode === "current-write") {
        runCurrentSessionWrite(binary, sessionFixture);
        continue;
      }
      const sourcePath = join(piAgentParityRoot, "session", sessionFixture.file);
      const sourceText = readFixtureBytes(sourcePath, sessionFixture.id).toString("utf8");
      const original = canonicalJsonl(sourceText, sessionFixture.id);
      if (sessionFixture.mode === "static-v4") {
        assertSessionRecords(original, sessionFixture.expected, sessionFixture.id, sessionFixture.mode);
        record("session", sessionFixture.id, "pass", `${Buffer.byteLength(sourceText)} canonical bytes`, { evidenceTier, matrix: true });
        continue;
      }
      const sandbox = tempDir(`pi-parity-session-${sessionFixture.id}-`);
      try {
        const path = join(sandbox, `${sessionFixture.id}.jsonl`);
        writeFileSync(path, sourceText, "utf8");
        const result = runCommand(binary, [
          "--session", path,
          "-p", "--provider", "faux", "--model", "faux-1", "--no-tools",
          `resume ${sessionFixture.id}`,
        ], {
          cwd: sandbox,
          env: sandboxEnvironment(sandbox, join(sandbox, "agent"), join(sandbox, "sessions")),
          timeout: 60_000,
        });
        if (result.status !== 0) throw new Error(`migration run exit ${result.status}: ${shortText(result.stderr || result.stdout)}`);
        const migratedText = readFileSync(path, "utf8");
        const migrated = canonicalJsonl(migratedText, `${sessionFixture.id} migrated`, false);
        assertSessionRecords(migrated, sessionFixture.expected, sessionFixture.id, sessionFixture.mode);
        record("session", sessionFixture.id, "pass", `${Buffer.byteLength(migratedText)} canonical bytes after v${sessionFixture.version} migration`, { evidenceTier, matrix: true });
      } finally {
        rmSync(sandbox, { recursive: true, force: true });
      }
    } catch (error) {
      record("session", sessionFixture.id, "fail", error.message, { evidenceTier, matrix: true });
    }
  }
  runFocusedCommands(fixture, "session");
}

function runCurrentSessionWrite(binary, sessionFixture) {
  const sandbox = tempDir("pi-parity-session-current-");
  try {
    const result = runCommand(binary, [
      "-p", "--provider", "faux", "--model", "faux-1", "--session-id", sessionFixture.expected.header_id,
      sessionFixture.prompt,
    ], {
      cwd: sandbox,
      env: sandboxEnvironment(sandbox, join(sandbox, "agent"), join(sandbox, "sessions")),
      timeout: 60_000,
    });
    if (result.status !== 0) throw new Error(`current write exit ${result.status}: ${shortText(result.stderr || result.stdout)}`);
    const files = findJsonlFiles(join(sandbox, "sessions"));
    if (files.length !== 1) throw new Error(`current write produced ${files.length} JSONL files`);
    const text = readFileSync(files[0], "utf8");
    const records = canonicalJsonl(text, "current-write", false);
    assertSessionRecords(records, sessionFixture.expected, sessionFixture.id, sessionFixture.mode);
    record("session", sessionFixture.id, "pass", `${Buffer.byteLength(text)} canonical bytes written`, { evidenceTier: sessionFixture.evidence_tier, matrix: true });
  } finally {
    rmSync(sandbox, { recursive: true, force: true });
  }
}

function runStorageMatrix(matrix) {
  if (!matrix) return;
  try {
    validateOracle(matrix, "storage");
    for (const entry of matrix.fixtures || []) {
      const path = join(piCodingParityRoot, "storage", entry.file);
      const value = loadJson(path, `storage/${entry.id}`);
      if (!value) continue;
      for (const unknownPath of entry.unknown_paths || []) validatePathExists(value, unknownPath, `storage/${entry.id}`);
      record("storage", entry.id, "pass", `${entry.unknown_paths?.length || 0} declared unknown-key paths present`, { evidenceTier: entry.evidence_tier, matrix: true });
    }
  } catch (error) {
    record("storage", "fixture-contract", "fail", error.message, { evidenceTier: matrix.evidence_tier || "unit" });
  }
  runFocusedCommands(matrix, "storage");
}

function runResourceMatrix(matrix) {
  if (!matrix) return;
  try {
    validateOracle(matrix, "resources");
    const root = join(piCodingParityRoot, "package-resource");
    const manifest = loadJson(join(root, matrix.package_manifest), "package-resource/package.json");
    if (!manifest) throw new Error("package manifest is unavailable");
    for (const relativePath of matrix.round_trip.expected_files || []) readFixtureBytes(join(root, relativePath), `package resource ${relativePath}`);
    for (const [kind, expected] of Object.entries(matrix.round_trip.expected_manifest_paths || {})) if (!sameValue(manifest.pi?.[kind], expected)) throw new Error(`package manifest pi.${kind} does not round-trip exactly`);
    record("resources", "package-resource-round-trip", "pass", `${matrix.round_trip.expected_files.length} fixture files and four manifest lists`, { evidenceTier: matrix.evidence_tier, matrix: true });
  } catch (error) {
    record("resources", "package-resource-round-trip", "fail", error.message, { evidenceTier: matrix.evidence_tier || "unit" });
  }
  runFocusedCommands(matrix, "resources");
}

function runProviderMatrix(fixture) {
  if (!fixture) return;
  try {
    validateOracle(fixture, "provider");
    const indexPath = join(piAiParityRoot, "..", "provider-matrix", "index.json");
    const index = loadJson(indexPath, "provider-matrix/index.json");
    if (!index || index.schema_version !== 1 || !Array.isArray(index.variants) || index.variants.length === 0) throw new Error("provider matrix index is empty or invalid");
    for (const variant of index.variants) {
      if (!variant.provider || !variant.api || !variant.fixture || !allowedEvidenceTiers.has(variant.evidence_tier) || !Array.isArray(variant.upstream_oracle) || variant.upstream_oracle.length === 0) throw new Error(`provider variant lacks explicit metadata: ${JSON.stringify(variant)}`);
      if (!existsSync(join(packageRoot, "crates", "pi-ai", "tests", "fixtures", "provider-matrix", variant.fixture))) throw new Error(`provider fixture missing: ${variant.fixture}`);
    }
    providerStats.declared = index.variants.length;
    record("provider", "fixture-index", "pass", `${index.variants.length} catalog/API variants with pinned oracle metadata`, { evidenceTier: fixture.evidence_tier, matrix: true });
  } catch (error) {
    providerStats.declared = 0;
    record("provider", "fixture-index", "fail", error.message, { evidenceTier: fixture.evidence_tier || "mock" });
  }
  const command = fixture.offline_harness?.test_command || ["cargo", "test", "-p", "pi-ai", "--test", "provider_matrix", "--offline", "--quiet"];
  const program = command[0];
  const args = command.slice(1);
  const result = runCommand(program, args, { timeout: 600_000 });
  const ok = result.status === 0 && providerStats.declared > 0;
  providerStats.passed = ok ? providerStats.declared : 0;
  providerStats.failed = ok ? 0 : providerStats.declared;
  record("provider", "offline-mock-server-matrix", ok ? "pass" : "fail", ok ? `${providerStats.declared} variants; ${commandText(program, args)}` : `${providerStats.declared} variants; ${shortText(result.stderr || result.stdout || result.error)}`, { evidenceTier: "mock", matrix: true });
  const live = fixture.boundaries?.find((boundary) => boundary.evidence_tier === "live");
  if (live) record("provider", live.id, "not-run", liveRequested ? "live flag supplied, but no credentialed live command is declared; no live pass claimed" : live.description, { evidenceTier: "live", required: live.required === true, matrix: true });
}

function runTelemetryMatrix(matrix) {
  if (!matrix) return;
  try {
    validateOracle(matrix, "telemetry");
    const required = matrix.required_schema;
    if (required?.version !== 1 || required?.ai_span?.name !== "pi.ai.request") throw new Error("telemetry schema version or AI span contract is invalid");
    if (!Array.isArray(required.harness_spans) || required.harness_spans.length !== 11) throw new Error("telemetry harness span matrix must contain 11 spans");
    if (!sameValue(required.session_mutations, ["entry", "record", "lane", "fact"])) throw new Error("telemetry mutation values do not match pinned contract");
    record("telemetry", "schema-fixture-contract", "pass", `${required.harness_spans.length} harness spans plus ${required.ai_span.name}`, { evidenceTier: matrix.evidence_tier, matrix: true });
  } catch (error) {
    record("telemetry", "schema-fixture-contract", "fail", error.message, { evidenceTier: matrix.evidence_tier || "unit" });
  }
  runFocusedCommands(matrix, "telemetry");
}

function checkPinnedOracle() {
  if (!existsSync(upstreamRoot)) {
    record("oracle", "upstream-clone", "fail", `missing ${upstreamRoot}`, { matrix: false });
    return;
  }
  const revision = runCommand("git", ["-C", upstreamRoot, "rev-parse", "HEAD"]);
  const status = runCommand("git", ["-C", upstreamRoot, "status", "--porcelain"]);
  const clean = revision.status === 0 && revision.stdout.trim() === upstreamCommit && status.status === 0 && status.stdout.trim() === "";
  record("oracle", "pinned-upstream", clean ? "pass" : "fail", clean ? upstreamCommit : `HEAD=${revision.stdout.trim()} status=${shortText(status.stdout || status.stderr)}`, { matrix: false });
}

function ensureReleaseBinary() {
  if (!noBuild) {
    const buildArgs = ["build", "--release", "-p", "pi-coding-agent", "--offline"];
    const build = runCommand("cargo", buildArgs, { timeout: 900_000 });
    const ok = build.status === 0 && existsSync(binaryPath);
    record("build", "release-binary", ok ? "pass" : "fail", ok ? binaryPath : `${commandText("cargo", buildArgs)}; ${shortText(build.stderr || build.stdout || build.error)}`, { matrix: false });
  } else {
    record("build", "release-binary", existsSync(binaryPath) ? "pass" : "fail", existsSync(binaryPath) ? `${binaryPath} (--no-build)` : `missing ${binaryPath} (--no-build)`, { matrix: false });
  }
  return existsSync(binaryPath);
}

function loadAllFixtures() {
  const manifest = loadJson(join(piCodingParityRoot, "manifest.json"), "parity/manifest");
  if (manifest) {
    try {
      validateOracle(manifest, "parity manifest");
      if (manifest.schema_version !== 1 || !Array.isArray(manifest.matrices)) throw new Error("parity manifest schema is invalid");
      record("fixture", "manifest", "pass", `${manifest.matrices.length} owned matrices`, { matrix: false });
    } catch (error) {
      record("fixture", "manifest", "fail", error.message, { matrix: false });
    }
  }
  return {
    cli: loadJson(join(piCodingParityRoot, "cli-matrix.json"), "cli-matrix"),
    rpc: loadJson(join(piCodingParityRoot, "rpc", "golden-transcripts.json"), "rpc/golden-transcripts"),
    session: loadJson(join(piAgentParityRoot, "session", "session-matrix.json"), "session/session-matrix"),
    storage: loadJson(join(piCodingParityRoot, "storage", "storage-matrix.json"), "storage/storage-matrix"),
    resources: loadJson(join(piCodingParityRoot, "package-resource", "resource-matrix.json"), "package-resource/resource-matrix"),
    provider: loadJson(join(piAiParityRoot, "provider-boundaries.json"), "provider-boundaries"),
    telemetry: loadJson(join(piCodingParityRoot, "telemetry-schema.json"), "telemetry-schema"),
  };
}

function reportSummary() {
  const checkPassed = results.filter((result) => result.status === "pass").length;
  const checkFailed = results.filter((result) => result.status === "fail").length;
  const checkNotRun = results.filter((result) => result.status === "not-run").length;
  console.log("\n== leaf-F2 parity summary ==");
  console.log(`checks: ${checkPassed} passed, ${checkFailed} failed, ${checkNotRun} not-run, ${results.length} total`);
  console.log(`offline matrix: ${matrixStats.offline.passed} passed, ${matrixStats.offline.failed} failed, ${matrixStats.offline.notRun} not-run, ${matrixStats.offline.declared} declared`);
  console.log(`live matrix: ${matrixStats.live.passed} passed, ${matrixStats.live.failed} failed, ${matrixStats.live.notRun} not-run, ${matrixStats.live.declared} declared`);
  console.log(`provider variants: ${providerStats.passed} passed, ${providerStats.failed} failed, ${providerStats.declared} declared offline variants`);
  if (failures.length > 0) {
    console.log("blockers:");
    for (const failure of failures) console.log(`  - ${failure.category}/${failure.id}: ${failure.detail}`);
  }
}

async function main() {
  console.log("== leaf-F2 parity suite ==");
  console.log(`oracle: upstream_pi @ ${upstreamCommit}`);
  console.log(`binary: ${binaryPath}`);
  checkPinnedOracle();
  const fixtures = loadAllFixtures();
  const hasBinary = ensureReleaseBinary();
  if (hasBinary) {
    runCliMatrix(binaryPath, fixtures.cli);
    await runRpcMatrix(binaryPath, fixtures.rpc);
    runSessionMatrix(binaryPath, fixtures.session);
  } else {
    for (const testCase of fixtures.cli?.cases || []) record("cli", testCase.id, "fail", "release binary unavailable; no debug fallback", { evidenceTier: testCase.evidence_tier || "unit" });
    for (const transcript of fixtures.rpc?.transcripts || []) record("rpc", transcript.id, "fail", "release binary unavailable; no debug fallback", { evidenceTier: transcript.evidence_tier || "mock" });
    for (const sessionFixture of fixtures.session?.fixtures || []) record("session", sessionFixture.id, "fail", "release binary unavailable; no debug fallback", { evidenceTier: sessionFixture.evidence_tier || "unit" });
  }
  runStorageMatrix(fixtures.storage);
  runResourceMatrix(fixtures.resources);
  runProviderMatrix(fixtures.provider);
  runTelemetryMatrix(fixtures.telemetry);
  reportSummary();
  if (failures.length > 0) {
    process.exitCode = 1;
    return;
  }
  console.log("parity-suite-passed");
}

await main();
