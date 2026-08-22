#!/usr/bin/env node
// Oracle for the export-html port (packages/coding-agent/src/core/export-html).
// Reproduces the upstream generateHtml string pipeline against the SAME
// vendored template assets, so the Rust port must produce byte-identical
// output for a given session file + theme.
//
// Usage: node scripts/oracle_export_html.mjs <session.jsonl> <theme> [out.html]
// Reads template assets from upstream_pi/packages/coding-agent/src/core/export-html/.

import { readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO = join(__dirname, "..");
const TEMPLATE_DIR = join(REPO, "upstream_pi/packages/coding-agent/src/core/export-html");
const THEME_DIR = join(REPO, "upstream_pi/packages/coding-agent/src/modes/interactive/theme");

function parseColor(color) {
  const m = /^#([0-9a-fA-F]{2})([0-9a-fA-F]{2})([0-9a-fA-F]{2})$/.exec(color);
  if (m) return { r: parseInt(m[1],16), g: parseInt(m[2],16), b: parseInt(m[3],16) };
  const rgb = /^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$/.exec(color);
  if (rgb) return { r: +rgb[1], g: +rgb[2], b: +rgb[3] };
  return undefined;
}
function getLuminance(c) {
  const toLinear = (v) => { const s = v/255; return s <= 0.03928 ? s/12.92 : ((s+0.055)/1.055)**2.4; };
  return 0.2126*toLinear(c.r) + 0.7152*toLinear(c.g) + 0.0722*toLinear(c.b);
}
function adjustBrightness(color, factor) {
  const p = parseColor(color);
  if (!p) return color;
  const adj = (c) => Math.min(255, Math.max(0, Math.round(c*factor)));
  return `rgb(${adj(p.r)}, ${adj(p.g)}, ${adj(p.b)})`;
}
function deriveExportColors(baseColor) {
  const p = parseColor(baseColor);
  if (!p) return { pageBg: "rgb(24, 24, 30)", cardBg: "rgb(30, 30, 36)", infoBg: "rgb(60, 55, 40)" };
  if (getLuminance(p) > 0.5) {
    return {
      pageBg: adjustBrightness(baseColor, 0.96),
      cardBg: baseColor,
      infoBg: `rgb(${Math.min(255,p.r+10)}, ${Math.min(255,p.g+5)}, ${Math.max(0,p.b-20)})`,
    };
  }
  return {
    pageBg: adjustBrightness(baseColor, 0.7),
    cardBg: adjustBrightness(baseColor, 0.85),
    infoBg: `rgb(${Math.min(255,p.r+20)}, ${Math.min(255,p.g+15)}, ${p.b})`,
  };
}
function parseThemeJson(name) {
  const content = readFileSync(join(THEME_DIR, `${name}.json`), "utf-8");
  return JSON.parse(content.replace(/^\uFEFF/, ""));
}
function resolveVarRefs(value, vars, visited = new Set()) {
  if (typeof value === "number" || value === "" || value.startsWith("#")) return value;
  if (visited.has(value)) throw new Error("Circular variable reference: " + value);
  if (!(value in vars)) throw new Error("Variable reference not found: " + value);
  visited.add(value);
  return resolveVarRefs(vars[value], vars, visited);
}
function resolveThemeColors(colors, vars = {}) {
  const out = {};
  for (const [k,v] of Object.entries(colors)) out[k] = resolveVarRefs(v, vars);
  return out;
}
function withFallbacks(colors) {
  return { ...colors, thinkingMax: colors.thinkingMax ?? colors.thinkingXhigh,
    scrollbarThumb: colors.scrollbarThumb ?? colors.selectedBg,
    searchMatchBg: colors.searchMatchBg ?? colors.selectedBg,
    searchMatchText: colors.searchMatchText ?? colors.text };
}
const BASIC = ["#000000","#800000","#008000","#808000","#000080","#800080","#008080","#c0c0c0","#808080","#ff0000","#00ff00","#ffff00","#0000ff","#ff00ff","#00ffff","#ffffff"];
function ansi256ToHex(index) {
  if (index < 16) return BASIC[index];
  if (index < 232) {
    const cube = index-16, r = Math.floor(cube/36), g = Math.floor((cube%36)/6), b = cube%6;
    const h = (n) => (n===0?0:55+n*40).toString(16).padStart(2,"0");
    return `#${h(r)}${h(g)}${h(b)}`;
  }
  const gray = 8 + (index-232)*10;
  const g = gray.toString(16).padStart(2,"0");
  return `#${g}${g}${g}`;
}
function getResolvedThemeColors(themeName) {
  const name = themeName ?? "dark";
  const isLight = name === "light";
  const tj = parseThemeJson(name);
  const resolved = resolveThemeColors(withFallbacks(tj.colors), tj.vars);
  const defaultText = isLight ? "#000000" : "#e5e5e7";
  const css = {};
  for (const [k,v] of Object.entries(resolved)) {
    css[k] = typeof v === "number" ? ansi256ToHex(v) : v === "" ? defaultText : v;
  }
  return css;
}
function getThemeExportColors(themeName) {
  const name = themeName ?? "dark";
  const tj = parseThemeJson(name);
  const exp = tj.export;
  if (!exp) return {};
  const vars = tj.vars ?? {};
  const res = (v) => {
    if (v === undefined) return undefined;
    const r = resolveVarRefs(v, vars);
    if (typeof r === "number") return ansi256ToHex(r);
    if (r === "") return undefined;
    return r;
  };
  return { pageBg: res(exp.pageBg), cardBg: res(exp.cardBg), infoBg: res(exp.infoBg) };
}
function generateThemeVars(themeName) {
  const colors = getResolvedThemeColors(themeName);
  const lines = [];
  for (const [k,v] of Object.entries(colors)) lines.push(`--${k}: ${v};`);
  const themeExport = getThemeExportColors(themeName);
  const userMessageBg = colors.userMessageBg || "#343541";
  const derived = deriveExportColors(userMessageBg);
  lines.push(`--exportPageBg: ${themeExport.pageBg ?? derived.pageBg};`);
  lines.push(`--exportCardBg: ${themeExport.cardBg ?? derived.cardBg};`);
  lines.push(`--exportInfoBg: ${themeExport.infoBg ?? derived.infoBg};`);
  return lines.join("\n      ");
}
function generateHtml(sessionData, themeName) {
  const template  = readFileSync(join(TEMPLATE_DIR, "template.html"), "utf8");
  const templateCss = readFileSync(join(TEMPLATE_DIR, "template.css"), "utf8");
  const templateJs = readFileSync(join(TEMPLATE_DIR, "template.js"), "utf8");
  const markedJs = readFileSync(join(TEMPLATE_DIR, "vendor/marked.min.js"), "utf8");
  const hljsJs = readFileSync(join(TEMPLATE_DIR, "vendor/highlight.min.js"), "utf8");
  const themeVars = generateThemeVars(themeName);
  const colors = getResolvedThemeColors(themeName);
  const themeExport = getThemeExportColors(themeName);
  const derived = deriveExportColors(colors.userMessageBg || "#343541");
  const bodyBg = themeExport.pageBg ?? derived.pageBg;
  const containerBg = themeExport.cardBg ?? derived.cardBg;
  const infoBg = themeExport.infoBg ?? derived.infoBg;
  const b64 = Buffer.from(JSON.stringify(sessionData)).toString("base64");
  const css = templateCss.replace("{{THEME_VARS}}", themeVars).replace("{{BODY_BG}}", bodyBg)
    .replace("{{CONTAINER_BG}}", containerBg).replace("{{INFO_BG}}", infoBg);
  return template.replace("{{CSS}}", css).replace("{{JS}}", templateJs)
    .replace("{{SESSION_DATA}}", b64).replace("{{MARKED_JS}}", markedJs).replace("{{HIGHLIGHT_JS}}", hljsJs);
}
function loadSessionFile(rel) {
  const content = readFileSync(rel, "utf8");
  const fileEntries = content.split("\n").filter((l) => l.trim()).map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
  if (!fileEntries.length) return null;
  if (fileEntries[0].type !== "session" || typeof fileEntries[0].id !== "string") return null;
  const header = fileEntries[0];
  const entries = fileEntries.filter((e) => e.type !== "session");
  const leafId = entries.length ? entries[entries.length-1].id : null;
  return { header, entries, leafId };
}
const [,, inputPath, themeName, outputPath] = process.argv;
const sessionData = loadSessionFile(inputPath);
const html = generateHtml(sessionData, themeName ?? "dark");
const out = outputPath ?? `pi-session-${basename(inputPath, ".jsonl")}.html`;
writeFileSync(out, html, "utf8");
console.log(out);
