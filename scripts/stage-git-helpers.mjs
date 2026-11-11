#!/usr/bin/env node
// Stage a helper that was built for the remote host, not necessarily this desktop.
import { copyFileSync, mkdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { execFileSync } from 'node:child_process';

const args = process.argv.slice(2);
function option(name, fallback) {
  const index = args.indexOf(name);
  if (index === -1) return fallback;
  if (!args[index + 1] || args[index + 1].startsWith('--')) throw new Error(`Missing ${name}`);
  return args[index + 1];
}
const target = option('--target');
const binary = option('--binary');
const output = resolve(option('--output', 'target/debug/remote-git-helpers'));
const formats = {
  'linux-x86_64': ['ELF', 'x86-64'],
  'linux-aarch64': ['ELF', 'ARM aarch64'],
  'macos-aarch64': ['Mach-O', 'arm64'],
  'macos-x86_64': ['Mach-O', 'x86_64'],
};
if (!formats[target] || !binary) {
  throw new Error('Usage: node scripts/stage-git-helpers.mjs --target <linux-x86_64|linux-aarch64|macos-aarch64|macos-x86_64> --binary <built-helper> [--output <asset-root>]');
}
const source = resolve(binary);
if (!statSync(source).isFile()) throw new Error('Helper must be a regular file');
const description = execFileSync('file', ['-b', source], { encoding: 'utf8' });
if (!formats[target].every(part => description.includes(part))) {
  throw new Error(`Helper does not match ${target}: ${description.trim()}`);
}
const destination = join(output, target, 'mt-git-helper');
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);
console.log(`[remote-git-helper] ${target}: ${source} -> ${destination}`);
