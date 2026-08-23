#!/usr/bin/env node

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const ledgerPath = fileURLToPath(new URL("../CONVERSION-LEDGER.md", import.meta.url));
const source = readFileSync(ledgerPath, "utf8");
const taskPattern = /^- \[([ xX])\] (\d+\.|S-\d+)\s+/gm;
const ids = new Set();
let checked = 0;
let total = 0;
let match;

while ((match = taskPattern.exec(source)) !== null) {
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

const open = total - checked;
const progress = ((checked / total) * 100).toFixed(2);
console.log(`Conversion progress: ${progress}% (${checked}/${total}; ${open} open)`);
