import fs from "node:fs";

const [changedPath, messagesPath] = process.argv.slice(2);
if (!changedPath || !messagesPath) {
  console.error("usage: check_changed_clippy.mjs <changed-files-nul> <cargo-jsonl>");
  process.exit(2);
}

const normalize = (path) => path.replaceAll("\\", "/").replace(/^\.\//, "");
const changed = new Set(
  fs
    .readFileSync(changedPath, "utf8")
    .split("\0")
    .filter(Boolean)
    .map(normalize),
);

const matched = [];
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
  const files = (record.message.spans ?? [])
    .filter((span) => span.is_primary)
    .map((span) => normalize(span.file_name));
  if (files.some((file) => changed.has(file))) {
    matched.push(record.message);
  } else {
    ignoredBaseline += 1;
  }
}

console.log(
  `Clippy baseline ignored outside changed files: ${ignoredBaseline}; changed-file warnings: ${matched.length}`,
);
if (matched.length > 0) {
  for (const message of matched) {
    console.error(message.rendered ?? message.message);
  }
  process.exit(1);
}
