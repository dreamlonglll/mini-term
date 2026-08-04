/**
 * 项目技术栈徽标(devicon 裸 SVG,Vite 资产导入,每枚约 2KB 进主 bundle)。
 *
 * 命名注意 devicon 的归一化目录名:nodejs、vuejs、vitejs(TechIcon 的 key
 * 仍用 projectKind 的 'vite')。original 变体为多色原色、不跟随主题;
 * 个别徽标在浅色主题下观感差时可换 plain 变体 + currentColor。
 * 扩展新类型 = classifyProject 加一条规则 + 此表加一行 import。
 */
import javaIcon from 'devicon/icons/java/java-original.svg';
import rustIcon from 'devicon/icons/rust/rust-original.svg';
import goIcon from 'devicon/icons/go/go-original.svg';
import pythonIcon from 'devicon/icons/python/python-original.svg';
import flutterIcon from 'devicon/icons/flutter/flutter-original.svg';
import phpIcon from 'devicon/icons/php/php-original.svg';
import vueIcon from 'devicon/icons/vuejs/vuejs-original.svg';
import nextIcon from 'devicon/icons/nextjs/nextjs-original.svg';
import reactIcon from 'devicon/icons/react/react-original.svg';
import svelteIcon from 'devicon/icons/svelte/svelte-original.svg';
import viteIcon from 'devicon/icons/vitejs/vitejs-original.svg';
import nodeIcon from 'devicon/icons/nodejs/nodejs-original.svg';
import type { ProjectKind } from '../utils/projectKind';
import { PROJECT_KIND_LABELS } from '../utils/projectKind';

const TECH_ICONS: Record<ProjectKind, string> = {
  java: javaIcon,
  rust: rustIcon,
  go: goIcon,
  python: pythonIcon,
  flutter: flutterIcon,
  php: phpIcon,
  vuejs: vueIcon,
  nextjs: nextIcon,
  react: reactIcon,
  svelte: svelteIcon,
  vite: viteIcon,
  nodejs: nodeIcon,
};

interface Props {
  kind: ProjectKind;
  size?: number;
  className?: string;
}

export function TechIcon({ kind, size = 14, className }: Props) {
  const src = TECH_ICONS[kind];
  if (!src) return null;
  return (
    <img
      src={src}
      width={size}
      height={size}
      alt=""
      aria-hidden
      draggable={false}
      title={PROJECT_KIND_LABELS[kind]}
      className={`mt-icon mt-icon-tech ${className ?? ''}`}
    />
  );
}
