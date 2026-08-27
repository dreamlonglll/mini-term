import { spawnSync } from "node:child_process";
import fs from "node:fs";

const [base, messagesPath] = process.argv.slice(2);
if (!base || !messagesPath) {
  console.error("usage: check_changed_clippy.mjs <base-sha> <cargo-jsonl>");
  process.exit(2);
}

const normalize = (path) => path.replaceAll("\\", "/").replace(/^\.\//, "");

const diff = spawnSync(
  "git",
  ["diff", "--unified=0", "--no-color", base, "HEAD", "--", "*.rs"],
  { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);
if (diff.error) throw diff.error;
if (diff.status !== 0) {
  process.stderr.write(diff.stderr);
  process.exit(diff.status ?? 1);
}

const changedRanges = new Map();
let currentFile = null;
for (const line of diff.stdout.split("\n")) {
  if (line.startsWith("+++ b/")) {
    currentFile = normalize(line.slice(6));
    continue;
  }
  const match = line.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/);
  if (!currentFile || !match) continue;
  const start = Math.max(1, Number.parseInt(match[1], 10));
  const count = match[2] === undefined ? 1 : Number.parseInt(match[2], 10);
  const end = count === 0 ? start : start + count - 1;
  const ranges = changedRanges.get(currentFile) ?? [];
  ranges.push({ start, end });
  changedRanges.set(currentFile, ranges);
}

function warningTouchesChangedLine(message) {
  return (message.spans ?? [])
    .filter((span) => span.is_primary)
    .some((span) => {
      const ranges = changedRanges.get(normalize(span.file_name)) ?? [];
      return ranges.some(
        (range) => span.line_start <= range.end && range.start <= span.line_end,
      );
    });
}

const matched = new Map();
let ignoredBaseline = 0;
for (const line of fs.readFileSync(messagesPath, "utf8").split("\n")) {
  if (!line.trim()) continue;
  let record;
  try {
    record = JSON.parse(line);
  } catch {
    continue;
  }
  if (record.reason !== "compiler-message" || record.message?.level !== "warning") {
    continue;
  }
  if (warningTouchesChangedLine(record.message)) {
    const primary = (record.message.spans ?? []).find((span) => span.is_primary);
    const key = [
      record.message.code?.code ?? "",
      primary ? normalize(primary.file_name) : "",
      primary?.line_start ?? 0,
      record.message.message,
    ].join("\0");
    matched.set(key, record.message);
  } else {
    ignoredBaseline += 1;
  }
}

console.log(
  `Clippy baseline ignored outside changed lines: ${ignoredBaseline}; changed-line warnings: ${matched.size}`,
);
if (matched.size > 0) {
  for (const message of matched.values()) {
    console.error(message.rendered ?? message.message);
  }
  process.exit(1);
}
