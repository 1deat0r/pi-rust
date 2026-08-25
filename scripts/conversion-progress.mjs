#!/usr/bin/env node

import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const taskPattern = /^- \[([ xX])\] (\d+\.|S-\d+)\s+/;
const checklistPattern = /^- \[[^\]]*\]/;

/** Parse and validate every conversion-task checklist line. */
export function parseTasks(source) {
  const ids = new Set();
  let checked = 0;
  let total = 0;

  for (const [index, line] of source.split(/\r?\n/).entries()) {
    if (!checklistPattern.test(line)) continue;
    const match = taskPattern.exec(line);
    if (!match) {
      throw new Error(`malformed conversion task checklist at line ${index + 1}`);
    }
    const id = match[2].replace(/\.$/, "");
    if (ids.has(id)) {
      throw new Error(`duplicate conversion task id: ${id}`);
    }
    ids.add(id);
    total += 1;
    if (match[1].toLowerCase() === "x") checked += 1;
  }

  if (total === 0) {
    throw new Error("no conversion tasks found");
  }
  return { checked, total, open: total - checked };
}

export function formatProgress({ checked, total, open }) {
  const progress = ((checked / total) * 100).toFixed(2);
  return `Conversion progress: ${progress}% (${checked}/${total}; ${open} open)`;
}

const invokedPath = process.argv[1] ? resolve(process.argv[1]) : null;
const scriptPath = fileURLToPath(import.meta.url);
if (invokedPath === scriptPath) {
  const ledgerPath = fileURLToPath(new URL("../CONVERSION-LEDGER.md", import.meta.url));
  const stats = parseTasks(readFileSync(ledgerPath, "utf8"));
  console.log(formatProgress(stats));
}
