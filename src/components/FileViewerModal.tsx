import { useState, useEffect, useMemo, useRef, useCallback } from 'react';
import type { MouseEvent as ReactMouseEvent, ReactNode } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeRaw from 'rehype-raw';
import type { FileContentResult } from '../types';
import { openExternalUrl } from '../utils/externalLink';
import { Modal } from './Modal';
import { useT } from '../i18n';

interface FileViewerModalProps {
  open: boolean;
  onClose: () => void;
  filePath: string;
  projectRoot: string;
  highlightLine?: number;
}

function isMarkdownFile(path: string) {
  return /\.(md|markdown|mkd|mdx)$/i.test(path);
}

function isImageFile(path: string) {
  return /\.(png|jpe?g|gif|bmp|webp|svg|ico|avif|tiff?)$/i.test(path);
}

function isHtmlFile(path: string) {
  return /\.html?$/i.test(path);
}

/** 把 Markdown 里的相对/绝对本地链接解析成规范化的绝对路径（正斜杠、去掉 ./ 与 ..） */
function resolveLocalHref(currentFile: string, href: string): string | null {
  let raw = href.split('#')[0].split('?')[0].trim();
  if (!raw) return null;
  try { raw = decodeURI(raw); } catch { /* 保留原值 */ }
  raw = raw.replace(/\\/g, '/');
  const curr = currentFile.replace(/\\/g, '/');
  const dir = curr.slice(0, curr.lastIndexOf('/'));
  const isWinAbs = /^[a-zA-Z]:\//.test(raw);
  const isPosixAbs = raw.startsWith('/');
  const base = isWinAbs || isPosixAbs ? raw : `${dir}/${raw}`;
  const out: string[] = [];
  for (const seg of base.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') { out.pop(); continue; }
    out.push(seg);
  }
  if (isPosixAbs && !/^[a-zA-Z]:$/.test(out[0] ?? '')) return '/' + out.join('/');
  return out.join('/');
}

/** 提取 React 子节点的纯文本（用于给标题生成锚点 id） */
function nodeText(node: ReactNode): string {
  if (node == null || node === false || node === true) return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join('');
  if (typeof node === 'object' && 'props' in node) {
    return nodeText((node as { props?: { children?: ReactNode } }).props?.children);
  }
  return '';
}

/** GitHub 风格 slug：小写、空格转连字符、保留中文与字母数字 */
function slugify(text: string): string {
  return text
    .trim()
    .toLowerCase()
    .replace(/[^\w一-龥\s-]/g, '')
    .replace(/\s+/g, '-');
}

/** 给标题加上锚点 id，使文档内 [文字](#标题) 链接可以滚动定位 */
function makeHeading(Tag: 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6') {
  return function Heading({ node: _node, children, ...props }: { node?: unknown; children?: ReactNode }) {
    return <Tag id={slugify(nodeText(children))} {...props}>{children}</Tag>;
  };
}
const headingComponents = {
  h1: makeHeading('h1'),
  h2: makeHeading('h2'),
  h3: makeHeading('h3'),
  h4: makeHeading('h4'),
  h5: makeHeading('h5'),
  h6: makeHeading('h6'),
};

export function FileViewerModal({ open, onClose, filePath, projectRoot, highlightLine }: FileViewerModalProps) {
  const t = useT();
  const [result, setResult] = useState<FileContentResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [preview, setPreview] = useState(true);
  const [editing, setEditing] = useState(false);
  const [editContent, setEditContent] = useState('');
  const [saving, setSaving] = useState(false);
  // 当前正在查看的文件，可随 Markdown 内的本地链接跳转；初始为传入的 filePath
  const [currentPath, setCurrentPath] = useState(filePath);
  const [history, setHistory] = useState<string[]>([]);
  const highlightRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const editLineNumbersRef = useRef<HTMLDivElement>(null);
  const isDirty = editing && result !== null && editContent !== result.content;

  const isMd = useMemo(() => isMarkdownFile(currentPath), [currentPath]);
  const isImg = useMemo(() => isImageFile(currentPath), [currentPath]);
  const isHtml = useMemo(() => isHtmlFile(currentPath), [currentPath]);

  const htmlSrcDoc = useMemo(() => {
    if (!isHtml || !result?.content) return '';
    const normalized = currentPath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return result.content.replace(
      /((?:src|href|poster)\s*=\s*["'])(?!https?:|data:|blob:|mailto:|tel:|#|javascript:)([^"']+)(["'])/gi,
      (_match, prefix, url, suffix) => prefix + convertFileSrc(fileDir + '/' + url) + suffix
    );
  }, [isHtml, result?.content, currentPath]);

  const resolveImgSrc = useCallback((src: string | undefined) => {
    if (!src || /^(https?:|data:|blob:)/i.test(src)) return src;
    const normalized = currentPath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return convertFileSrc(fileDir + '/' + src);
  }, [currentPath]);

  // 跳转到链接目标文件，记录历史以支持返回
  // 用两次独立 setState（而非在 updater 里嵌套 setState），避免 StrictMode 下
  // updater 被二次调用导致 history 重复入栈。
  const navigateTo = useCallback((absPath: string) => {
    if (absPath === currentPath) return;
    setHistory((h) => [...h, currentPath]);
    setCurrentPath(absPath);
  }, [currentPath]);

  const goBack = useCallback(() => {
    if (!history.length) return;
    setCurrentPath(history[history.length - 1]);
    setHistory((h) => h.slice(0, -1));
  }, [history]);

  const handleClose = useCallback(() => {
    if (isDirty && !window.confirm(t('fileViewer.unsavedConfirm'))) return;
    onClose();
  }, [isDirty, onClose, t]);

  const handleSave = useCallback(async () => {
    if (!result || saving) return;
    setSaving(true);
    try {
      await invoke('write_file_content', { projectRoot, path: currentPath, content: editContent });
      setResult({ ...result, content: editContent });
    } catch (e) {
      setError(t('fileViewer.saveFailed') + ': ' + String(e));
    } finally {
      setSaving(false);
    }
  }, [currentPath, editContent, projectRoot, result, saving, t]);

  // 拦截 Markdown 内的 <a> 点击：先 preventDefault 避免整个程序重载
  const handleLinkClick = useCallback((e: ReactMouseEvent<HTMLAnchorElement>) => {
    e.preventDefault();
    const href = e.currentTarget.getAttribute('href');
    if (!href) return;
    // http(s) 外链：弹确认后系统浏览器打开
    if (/^https?:\/\//i.test(href)) {
      void openExternalUrl(href);
      return;
    }
    // 文档内锚点：在当前预览里滚动到对应标题
    if (href.startsWith('#')) {
      let id = href.slice(1);
      try { id = decodeURIComponent(id); } catch { /* 保留原值 */ }
      const el = contentRef.current?.querySelector(`[id="${CSS.escape(id)}"]`);
      el?.scrollIntoView({ behavior: 'smooth', block: 'start' });
      return;
    }
    // mailto:/tel: 等非 http 协议（排除 Windows 盘符 X:\ 形式）：交给系统处理
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(href) && !/^[a-zA-Z]:[\\/]/.test(href)) {
      openUrl(href).catch((err) => console.error('打开链接失败:', err));
      return;
    }
    // 本地文件链接：解析为绝对路径后在预览内打开
    const target = resolveLocalHref(currentPath, href);
    if (target) navigateTo(target);
  }, [currentPath, navigateTo]);

  // 非图片文件时读取文本内容
  useEffect(() => {
    if (!open || isImg) return;
    setLoading(true);
    setError('');
    setResult(null);

    invoke<FileContentResult>('read_file_content', { projectRoot, path: currentPath })
      .then((nextResult) => { setResult(nextResult); setEditContent(nextResult.content); })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, currentPath, projectRoot, isImg]);

  // 外部传入的 filePath 变化（或重新打开）时，重置到该文件并清空跳转历史
  useEffect(() => {
    setCurrentPath(filePath);
    setHistory([]);
  }, [filePath, open]);

  // 跳转后内容区滚回顶部
  useEffect(() => {
    if (contentRef.current) contentRef.current.scrollTop = 0;
  }, [currentPath]);

useEffect(() => {
    // 1. 处理滚动重置 (只要 editing 变化就执行)
    if (contentRef.current) contentRef.current.scrollTop = 0;
    if (editLineNumbersRef.current) editLineNumbersRef.current.scrollTop = 0;

    // 2. 处理键盘事件监听 (只有 open 为 true 时才添加)
    if (!open) return;

    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's' && editing) { 
          e.preventDefault(); 
          void handleSave(); 
      }
      else if (e.key === 'Escape') handleClose();
    };

    window.addEventListener('keydown', handleKeyDown);

    // 清理函数：移除事件监听器
    return () => {
      window.removeEventListener('keydown', handleKeyDown);
    };

}, [editing, open, handleClose, handleSave]);
  // 仅当查看的是原始 filePath 时才高亮跳转行
  useEffect(() => {
    if (currentPath === filePath && result && highlightLine && highlightRef.current) {
      highlightRef.current.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  }, [result, highlightLine, currentPath, filePath]);

  if (!open) return null;

  const fileName = currentPath.replace(/\\/g, '/').split('/').pop() ?? currentPath;
  const canEdit = !isImg && result && !result.isBinary && !result.tooLarge;
  const toggleEditMode = () => {
    if (editing) {
      setEditing(false);
      return;
    }
    if (!editContent) setEditContent(result?.content ?? '');
    setEditing(true);
    setPreview(false);
  };

  return (
    <Modal open={open} onClose={onClose} align="center" ariaLabel={fileName}
      panelClassName="w-[90vw] h-[80vh] select-text">
        {/* 工具栏 */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2 min-w-0">
            {history.length > 0 && (
              <button
                className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-base leading-none px-1 -ml-1 flex-shrink-0"
                onClick={goBack}
                title={t("fileViewer.back")}
              >
                ←
              </button>
            )}
            <span className="text-base font-medium text-[var(--accent)] flex-shrink-0">{fileName}</span>
            <span className="text-sm text-[var(--text-muted)] truncate">
              {currentPath}
            </span>
          </div>
          <div className="flex items-center gap-4 flex-shrink-0">
            {canEdit && (
              <>
                {editing && (
                  <button onClick={() => void handleSave()} disabled={saving || !isDirty}>
                    {saving ? t('fileViewer.saving') : t('fileViewer.save')}
                  </button>
                )}
                <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
                  <button
                    className={`px-2.5 py-1 transition-colors ${editing ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                    onClick={toggleEditMode}
                  >
                    {editing ? t('fileViewer.view') : t('fileViewer.edit')}
                  </button>
                </div>
              </>
            )}
            {(isMd || isHtml) && result && !result.isBinary && !result.tooLarge && (
              <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
                <button
                  className={`px-2.5 py-1 transition-colors ${preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                  onClick={() => setPreview(true)}
                >
                  {t("fileViewer.preview")}
                </button>
                <button
                  className={`px-2.5 py-1 transition-colors ${!preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                  onClick={() => setPreview(false)}
                >
                  {t("fileViewer.source")}
                </button>
              </div>
            )}
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
              onClick={handleClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 内容区 */}
        <div
          ref={contentRef}
          className={`flex-1 min-h-0 bg-[var(--bg-base)] ${editing ? 'overflow-hidden' : 'overflow-auto'}`}
        >
          {loading && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              {t("fileViewer.loading")}
            </div>
          )}
          {error && (
            <div className="flex items-center justify-center h-full text-[var(--color-error)]">
              {error}
            </div>
          )}
          {isImg && (
            <div className="flex items-center justify-center h-full p-6">
              <img
                src={convertFileSrc(currentPath)}
                alt={fileName}
                className="max-w-full max-h-full object-contain"
                draggable={false}
              />
            </div>
          )}
          {!isImg && result && result.isBinary && (
            <div className="flex flex-col items-center justify-center h-full gap-4 text-[var(--text-muted)]">
              <span>{t("fileViewer.binaryNotSupported")}</span>
              <button
                className="px-4 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity"
                onClick={() => invoke('open_path_with_default_app', { path: currentPath })}
              >
                {t("fileViewer.openWithDefaultApp")}
              </button>
            </div>
          )}
          {!isImg && result && result.tooLarge && (
            <div className="flex flex-col items-center justify-center h-full gap-4 text-[var(--text-muted)]">
              <span>{t("fileViewer.tooLarge")}</span>
              <button
                className="px-4 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity"
                onClick={() => invoke('open_path_with_default_app', { path: currentPath })}
              >
                {t("fileViewer.openWithDefaultApp")}
              </button>
            </div>
          )}
          {!isImg && result && !result.isBinary && !result.tooLarge && editing ? (
            <div className="flex w-full h-full overflow-hidden font-mono text-sm leading-6">
              <div ref={editLineNumbersRef} className="w-12 h-full overflow-hidden py-4 text-right pr-3 text-[var(--text-muted)] select-none flex-shrink-0 opacity-40">
                {editContent.split('\n').map((_line, index) => <div key={index}>{index + 1}</div>)}
              </div>
              <textarea
                value={editContent}
                onChange={(event) => setEditContent(event.target.value)}
                onScroll={(event) => {
                  if (editLineNumbersRef.current) editLineNumbersRef.current.scrollTop = event.currentTarget.scrollTop;
                }}
                autoFocus
                spellCheck={false}
                wrap="off"
                className="flex-1 h-full resize-none outline-none bg-[var(--bg-base)] text-[var(--text-primary)] leading-6 py-4 px-2"
              />
            </div>
          ) : !isImg && result && !result.isBinary && !result.tooLarge && isHtml && preview ? (
            <iframe
              srcDoc={htmlSrcDoc}
              title={fileName}
              className="w-full h-full border-0 bg-white"
              sandbox="allow-same-origin"
            />
          ) : !isImg && result && !result.isBinary && !result.tooLarge && isMd && preview ? (
            <div className="md-preview p-6 max-w-[860px] mx-auto">
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeRaw]}
                components={{
                  ...headingComponents,
                  img: ({ src, alt, ...props }) => (
                    <img src={resolveImgSrc(src)} alt={alt ?? ''} {...props} />
                  ),
                  a: ({ href, children, ...props }) => (
                    <a href={href} onClick={handleLinkClick} {...props}>{children}</a>
                  ),
                }}
              >
                {result.content}
              </ReactMarkdown>
            </div>
          ) : !isImg && result && !result.isBinary && !result.tooLarge && (
            <div className="font-mono text-sm leading-6">
              {result.content.split('\n').map((line, i) => (
                <div
                  key={i}
                  ref={i + 1 === highlightLine ? highlightRef : undefined}
                  className={`flex hover:bg-[var(--border-subtle)] ${i + 1 === highlightLine ? 'bg-[var(--accent-muted)]' : ''}`}
                >
                  <span className="w-12 text-right pr-3 text-[var(--text-muted)] select-none flex-shrink-0 opacity-40">
                    {i + 1}
                  </span>
                  <span className="flex-1 whitespace-pre px-2 text-[var(--text-primary)]">
                    {line}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
    </Modal>
  );
}
