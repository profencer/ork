#!/usr/bin/env node
// ADR-0055 AC #6: bundle size ≤ 1 MiB gzip for `pnpm build`. CI runs
// `pnpm bundle-size:check` and fails the build above the budget. This
// is a deliberate budget rather than a soft warning — Mastra's Studio
// is in the same ballpark and we want the same discipline.

import { readdirSync, statSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const DIST = new URL("../dist/", import.meta.url).pathname;
const BUDGET_BYTES = 1024 * 1024; // 1 MiB gzip

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    const s = statSync(p);
    return s.isDirectory() ? walk(p) : [p];
  });
}

let total = 0;
try {
  for (const file of walk(DIST)) {
    const buf = readFileSync(file);
    total += gzipSync(buf).length;
  }
} catch (e) {
  if (e && typeof e === "object" && "code" in e && e.code === "ENOENT") {
    console.error(
      "bundle-size:check: dist/ missing — run `pnpm build` before this script.",
    );
    process.exit(2);
  }
  throw e;
}

const mib = total / (1024 * 1024);
console.log(`bundle gzip size: ${mib.toFixed(3)} MiB`);
if (total > BUDGET_BYTES) {
  console.error(
    `bundle-size:check: ${mib.toFixed(3)} MiB > 1.000 MiB budget (ADR-0055 AC #6).`,
  );
  process.exit(1);
}
