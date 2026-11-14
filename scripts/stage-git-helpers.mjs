#!/usr/bin/env node
// Stage a remote-host binary, or fail closed on the complete desktop asset set.
// Parse ELF/Mach-O directly so Windows can validate all four downloaded assets.
import { chmodSync, copyFileSync, mkdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const args = process.argv.slice(2);
function option(name, fallback) {
  const index = args.indexOf(name);
  if (index === -1) return fallback;
  if (!args[index + 1] || args[index + 1].startsWith('--')) throw new Error(`Missing ${name}`);
  return args[index + 1];
}
const targets = ['linux-x86_64', 'linux-aarch64', 'macos-aarch64', 'macos-x86_64'];
const output = resolve(option('--output', 'target/debug/remote-git-helpers'));

function validate(source, target) {
  if (!statSync(source).isFile()) throw new Error(`Helper must be a regular file: ${source}`);
  const bytes = readFileSync(source);
  function require(condition, detail) {
    if (!condition) throw new Error(`Invalid ${target} helper (${source}): ${detail}`);
  }
  function span(offset, size) {
    require(Number.isSafeInteger(offset) && Number.isSafeInteger(size)
      && offset >= 0 && size >= 0 && offset <= bytes.length && size <= bytes.length - offset,
    'truncated or invalid binary range');
  }
  function u64(offset) {
    span(offset, 8);
    const value = bytes.readBigUInt64LE(offset);
    require(value <= BigInt(Number.MAX_SAFE_INTEGER), 'binary offset exceeds safe integer range');
    return Number(value);
  }
  if (target.startsWith('linux-')) {
    span(0, 64);
    require(bytes.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))
      && bytes[4] === 2 && bytes[5] === 1 && bytes[6] === 1, 'expected little-endian ELF64');
    require([2, 3].includes(bytes.readUInt16LE(16)), 'ELF is not an executable');
    require(bytes.readUInt16LE(18) === (target === 'linux-x86_64' ? 62 : 183), 'wrong ELF architecture');
    require(bytes.readUInt32LE(20) === 1 && bytes.readUInt16LE(52) === 64, 'invalid ELF header');
    const offset = u64(32);
    const size = bytes.readUInt16LE(54);
    const count = bytes.readUInt16LE(56);
    require(size === 56 && count > 0 && count < 0xffff, 'invalid ELF program header table');
    span(offset, size * count);
    let executableLoad = false;
    for (let index = 0; index < count; index++) {
      const entry = offset + index * size;
      const type = bytes.readUInt32LE(entry);
      require(type !== 3, 'PT_INTERP is forbidden: Linux helpers must be static musl binaries');
      if (type === 1 && (bytes.readUInt32LE(entry + 4) & 1)) executableLoad = true;
      const start = u64(entry + 8);
      const length = u64(entry + 32);
      span(start, length);
      if (type !== 2) continue;
      require(length > 0 && length % 16 === 0, 'invalid ELF dynamic table');
      let terminated = false;
      for (let position = start; position < start + length; position += 16) {
        const tag = bytes.readBigInt64LE(position);
        require(tag !== 1n, 'DT_NEEDED is forbidden: Linux helpers must not need shared libraries');
        if (tag === 0n) { terminated = true; break; }
      }
      require(terminated, 'unterminated ELF dynamic table');
    }
    require(executableLoad, 'ELF has no executable load segment');
  } else {
    span(0, 32);
    require(bytes.readUInt32LE(0) === 0xfeedfacf, 'expected a thin little-endian Mach-O64 executable');
    require(bytes.readUInt32LE(4) === (target === 'macos-x86_64' ? 0x01000007 : 0x0100000c), 'wrong Mach-O architecture');
    require(bytes.readUInt32LE(12) === 2, 'Mach-O is not an executable');
    const count = bytes.readUInt32LE(16);
    const length = bytes.readUInt32LE(20);
    span(32, length);
    require(count > 0 && count <= length / 8, 'invalid Mach-O command count');
    let position = 32;
    for (let index = 0; index < count; index++) {
      span(position, 8);
      const size = bytes.readUInt32LE(position + 4);
      require(size >= 8 && size % 8 === 0 && position + size <= 32 + length, 'invalid Mach-O load command');
      position += size;
    }
    require(position === 32 + length, 'invalid Mach-O command table length');
  }
}

if (args.includes('--verify-all')) {
  // No partial bundles: every desktop ships every remote-host architecture.
  for (const target of targets) validate(join(output, target, 'mt-git-helper'), target);
  for (const target of targets) chmodSync(join(output, target, 'mt-git-helper'), 0o755);
  console.log(`[remote-git-helper] validated all ${targets.length} assets in ${output}`);
} else {
  const target = option('--target');
  const binary = option('--binary');
  if (!targets.includes(target) || !binary) {
    throw new Error('Usage: node scripts/stage-git-helpers.mjs --target <linux-x86_64|linux-aarch64|macos-aarch64|macos-x86_64> --binary <built-helper> [--output <asset-root>] OR --verify-all [--output <asset-root>]');
  }
  const source = resolve(binary);
  validate(source, target);
  const destination = join(output, target, 'mt-git-helper');
  mkdirSync(dirname(destination), { recursive: true });
  copyFileSync(source, destination);
  chmodSync(destination, 0o755);
  console.log(`[remote-git-helper] ${target}: ${source} -> ${destination}`);
}
