/**
 * 项目类型探测(纯函数,无依赖,可进 node --test)。
 *
 * 规则从具体到泛化:标记文件直判,package.json 再按依赖细分前端框架。
 * 探测输入是项目根目录一层的文件名集合 + package.json 的 deps 合并表。
 */

import type { ProjectKind } from '../types';

export type { ProjectKind };

/** 手动指定菜单的可选项顺序(常用在前)。 */
export const PROJECT_KINDS: ProjectKind[] = [
  'java', 'rust', 'go', 'python', 'nodejs', 'react', 'vuejs', 'nextjs', 'svelte', 'vite', 'flutter', 'php',
];

/** 技术栈徽标的展示名(专有名词,不进 i18n)。 */
export const PROJECT_KIND_LABELS: Record<ProjectKind, string> = {
  java: 'Java',
  rust: 'Rust',
  go: 'Go',
  python: 'Python',
  flutter: 'Flutter',
  php: 'PHP',
  vuejs: 'Vue',
  nextjs: 'Next.js',
  react: 'React',
  svelte: 'Svelte',
  vite: 'Vite',
  nodejs: 'Node.js',
};

/** 出现在项目根目录即触发(重)探测的标记文件。 */
export const PROJECT_MARKER_FILES = new Set([
  'pom.xml', 'build.gradle', 'build.gradle.kts', 'Cargo.toml', 'go.mod',
  'pyproject.toml', 'requirements.txt', 'pubspec.yaml', 'composer.json', 'package.json',
]);

export function classifyProject(
  files: Set<string>,
  deps?: Record<string, string>,
): ProjectKind | null {
  if (files.has('pom.xml') || files.has('build.gradle') || files.has('build.gradle.kts')) return 'java';
  if (files.has('Cargo.toml')) return 'rust';
  if (files.has('go.mod')) return 'go';
  if (files.has('pyproject.toml') || files.has('requirements.txt')) return 'python';
  if (files.has('pubspec.yaml')) return 'flutter';
  if (files.has('composer.json')) return 'php';
  if (files.has('package.json')) {
    if (deps?.['vue']) return 'vuejs';
    if (deps?.['next']) return 'nextjs';
    if (deps?.['react']) return 'react';
    if (deps?.['svelte']) return 'svelte';
    if (deps?.['vite']) return 'vite';
    return 'nodejs';
  }
  return null;
}

/** 解析 package.json 文本为 dependencies/devDependencies 合并表;解析失败返回 undefined。 */
export function parsePackageDeps(jsonText: string): Record<string, string> | undefined {
  try {
    const pkg = JSON.parse(jsonText) as {
      dependencies?: Record<string, string>;
      devDependencies?: Record<string, string>;
    };
    if (typeof pkg !== 'object' || pkg === null) return undefined;
    return { ...pkg.dependencies, ...pkg.devDependencies };
  } catch {
    return undefined;
  }
}
