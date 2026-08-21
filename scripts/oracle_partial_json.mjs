// Parity oracle: what upstream pi (packages/ai/src/utils/json-parse.ts, pinned
// 5cd93f6) actually returns for streaming JSON fragments, via npm partial-json@0.1.7.
// Run: node scripts/oracle_partial_json.mjs
// Output is the golden table used by crates/pi-ai tests.
// Cases 0-19: core partial behavior. Cases 20-27: repairJson-path (raw control
// chars, invalid escapes, trailing backslash, partial exponent) per reviewer
// condition 2 — P2-A must not pass without implementing repairJson.

import { createRequire } from "node:module";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const vendored = join(here, "partial-json-0.1.7", "dist", "index.js");
function partialParseVendored(json) {
  const { parse } = require(vendored); // vendored partial-json@0.1.7 (network-free)
  return parse(json);
}

const VALID_JSON_ESCAPES = new Set(['"', "\\", "/", "b", "f", "n", "r", "t", "u"]);
function isControlCharacter(char) {
  const codePoint = char.codePointAt(0);
  return codePoint !== undefined && codePoint >= 0x00 && codePoint <= 0x1f;
}
function escapeControlCharacter(char) {
  switch (char) {
    case "\b": return "\\b";
    case "\f": return "\\f";
    case "\n": return "\\n";
    case "\r": return "\\r";
    case "\t": return "\\t";
    default: return `\\u${char.codePointAt(0)?.toString(16).padStart(4, "0") ?? "0000"}`;
  }
}
function repairJson(json) {
  let repaired = "";
  let inString = false;
  for (let index = 0; index < json.length; index++) {
    const char = json[index];
    if (!inString) {
      repaired += char;
      if (char === '"') inString = true;
      continue;
    }
    if (char === '"') { repaired += char; inString = false; continue; }
    if (char === "\\") {
      const nextChar = json[index + 1];
      if (nextChar === undefined) { repaired += "\\\\"; continue; }
      if (nextChar === "u") {
        const unicodeDigits = json.slice(index + 2, index + 6);
        if (/^[0-9a-fA-F]{4}$/.test(unicodeDigits)) { repaired += `\\u${unicodeDigits}`; index += 5; continue; }
      }
      if (VALID_JSON_ESCAPES.has(nextChar)) { repaired += `\\${nextChar}`; index += 1; continue; }
      repaired += "\\\\";
      continue;
    }
    repaired += isControlCharacter(char) ? escapeControlCharacter(char) : char;
  }
  return repaired;
}
// EXACT upstream parseJsonWithRepair + parseStreamingJson (json-parse.ts @ 5cd93f6)
function parseJsonWithRepair(json) {
  try { return JSON.parse(json); }
  catch (error) {
    const repairedJson = repairJson(json);
    if (repairedJson !== json) { return JSON.parse(repairedJson); }
    throw error;
  }
}
function parseStreamingJson(partialJson) {
  if (!partialJson || partialJson.trim() === "") return {};
  try { return parseJsonWithRepair(partialJson); }
  catch {
    try { return partialParseVendored(partialJson) ?? {}; }
    catch {
      try { return partialParseVendored(repairJson(partialJson)) ?? {}; }
      catch { return {}; }
    }
  }
}

const cases = [
  "{\"a",
  "{\"a\":",
  "{\"a\": 1",
  "{\"a\": \"hel",
  "\"hel",
  "-",
  "12.",
  "12",
  "tru",
  "{\"a\": tru",
  "[1, 2,",
  "",
  "nul",
  "{\"a\": 1,",
  "{\"a\": {\"b\": 2}",
  "{\"a\": \"he\\\"",
  "tru\"e",
  "Inf",
  "-Inf",
  "{\"a\": [1, 2",
  "{\"a\": \"b\u0001c\"}",
  "[\"x\u0001y\"]",
  "{\"a\": \"b\\xc\"}",
  "{\"a\": \"b\\qc\"}",
  "{\"a\": \"b\\",
  "\\",
  "{\"a\": \"b\u0001c",
  "1e"
];
console.log("case\t=>\tresult");
for (const c of cases) {
  const r = parseStreamingJson(c);
  console.log(JSON.stringify(c), "\t=>\t", JSON.stringify(r), r === null ? "null" : typeof r);
}
