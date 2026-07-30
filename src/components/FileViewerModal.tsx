import { useState, useEffect, useMemo, useRef, useCallback } from 'react';
import type { MouseEvent as ReactMouseEvent, ReactNode } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeRaw from 'rehype-raw';
import type { FileContentResult, FsChangePayload } from '../types';
import { openExternalUrl } from '../utils/externalLink';
import { showConfirm } from '../utils/prompt';
import { useOverlayPresence } from '../hooks/useOverlayMotion';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { Modal } from './Modal';
import { CodeEditor, type CodeEditorApi } from './CodeEditor';
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
  // 当前正在查看的文件，可随 Markdown 内的本地链接跳转；初始为传入的 filePath
  const [currentPath, setCurrentPath] = useState(filePath);
  const [history, setHistory] = useState<string[]>([]);
  const contentRef = useRef<HTMLDivElement>(null);
  const present = useOverlayPresence(open);

  // === 编辑状态 ===
  // draftRef 是编辑器当前全文、savedRef 是磁盘上最后一次已知内容；
  // dirty 只是二者不等的 UI 投影，真值始终以两个 ref 的比较为准
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState('');
  /** 文件在外部被改、而本地有未保存修改时置真（干净时直接静默重载） */
  const [extChanged, setExtChanged] = useState(false);
  /** 切到预览那一刻的草稿快照；null = 无未保存修改，预览直接用磁盘内容 */
  const [previewDraft, setPreviewDraft] = useState<string | null>(null);
  /** 磁盘上的当前内容（载入或保存成功时更新）。预览渲染用它而不是
   *  result.content——后者是「打开时」的内容，保存后就旧了；也不能直接改
   *  result，那会换掉编辑器的 value 触发重建、丢撤销栈 */
  const [diskContent, setDiskContent] = useState('');
  const [reloadNonce, setReloadNonce] = useState(0);
  const draftRef = useRef('');
  const savedRef = useRef('');
  const lastSaveAtRef = useRef(0);
  const savingRef = useRef(false);
  const editorApiRef = useRef<CodeEditorApi | null>(null);

  const isMd = useMemo(() => isMarkdownFile(currentPath), [currentPath]);
  const isImg = useMemo(() => isImageFile(currentPath), [currentPath]);
  const isHtml = useMemo(() => isHtmlFile(currentPath), [currentPath]);

  const htmlSrcDoc = useMemo(() => {
    const source = previewDraft ?? diskContent;
    if (!isHtml || !source) return '';
    const normalized = currentPath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return source.replace(
      /((?:src|href|poster)\s*=\s*["'])(?!https?:|data:|blob:|mailto:|tel:|#|javascript:)([^"']+)(["'])/gi,
      (_match, prefix, url, suffix) => prefix + convertFileSrc(fileDir + '/' + url) + suffix
    );
  }, [isHtml, diskContent, previewDraft, currentPath]);

  const resolveImgSrc = useCallback((src: string | undefined) => {
    if (!src || /^(https?:|data:|blob:)/i.test(src)) return src;
    const normalized = currentPath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return convertFileSrc(fileDir + '/' + src);
  }, [currentPath]);

  // 有未保存修改时，任何会丢弃草稿的动作（关闭/跳转/返回）先过这道确认
  const confirmDiscard = useCallback(async () => {
    if (draftRef.current === savedRef.current) return true;
    return showConfirm(t('fileViewer.unsavedTitle'), t('fileViewer.unsavedMessage'));
  }, [t]);

  const requestClose = useCallback(() => {
    // 两段式退出：编辑器搜索面板开着时，Esc/遮罩点击先只关面板
    if (editorApiRef.current?.closeSearchIfOpen()) return;
    void confirmDiscard().then((ok) => {
      if (ok) onClose();
    });
  }, [confirmDiscard, onClose]);

  // 跳转到链接目标文件，记录历史以支持返回
  // 用两次独立 setState（而非在 updater 里嵌套 setState），避免 StrictMode 下
  // updater 被二次调用导致 history 重复入栈。
  const navigateTo = useCallback((absPath: string) => {
    if (absPath === currentPath) return;
    void confirmDiscard().then((ok) => {
      if (!ok) return;
      setHistory((h) => [...h, currentPath]);
      setCurrentPath(absPath);
    });
  }, [currentPath, confirmDiscard]);

  const goBack = useCallback(() => {
    if (!history.length) return;
    void confirmDiscard().then((ok) => {
      if (!ok) return;
      setCurrentPath(history[history.length - 1]);
      setHistory((h) => h.slice(0, -1));
    });
  }, [history, confirmDiscard]);

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
      .then((res) => {
        // 编辑基线与内容一起落位，避免出现「内容已换、基线还是旧文件」的窗口
        savedRef.current = res.content;
        draftRef.current = res.content;
        setDirty(false);
        setExtChanged(false);
        setPreviewDraft(null);
        setDiskContent(res.content);
        setSaveError('');
        setResult(res);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, currentPath, projectRoot, isImg, reloadNonce]);

  // 外部传入的 filePath 变化（或重新打开）时，重置到该文件并清空跳转历史
  useEffect(() => {
    setCurrentPath(filePath);
    setHistory([]);
  }, [filePath, open]);

  const canEdit = !!result && !result.isBinary && !result.tooLarge && !isImg;

  const handleDocChange = useCallback((doc: string) => {
    draftRef.current = doc;
    setDirty(doc !== savedRef.current);
  }, []);

  const handleSave = useCallback(async () => {
    // 干净或已在保存中时静默返回：Ctrl+S 是肌肉记忆，不该弹任何东西
    if (savingRef.current || draftRef.current === savedRef.current) return;
    const text = draftRef.current;
    savingRef.current = true;
    setSaving(true);
    setSaveError('');
    try {
      await invoke('write_file_content', { projectRoot, path: currentPath, content: text });
      savedRef.current = text;
      lastSaveAtRef.current = Date.now();
      setDiskContent(text);
      // 保存期间用户可能又敲了字：脏态按最新草稿重新比对，而不是直接置 false
      setDirty(draftRef.current !== text);
      setExtChanged(false);
    } catch (e) {
      setSaveError(String(e));
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  }, [projectRoot, currentPath]);

  // 文件被外部修改：干净时静默重载跟上磁盘，脏时挂提示条让用户自己决定
  useTauriEvent<FsChangePayload>('fs-change', (payload) => {
    if (!open || isImg || !result) return;
    const norm = (s: string) => s.replace(/\\/g, '/').toLowerCase();
    if (norm(payload.path) !== norm(currentPath)) return;
    // 自己 write_file_content 落盘触发的回声事件，不算「外部」修改
    if (Date.now() - lastSaveAtRef.current < 2000) return;
    if (draftRef.current !== savedRef.current) setExtChanged(true);
    else setReloadNonce((n) => n + 1);
  });

  // Ctrl/Cmd+S 全局兜底：焦点在工具栏/预览时也能保存。capture 抢在浏览器
  // 默认「保存网页」与编辑器自身 Mod-s 之前，stopPropagation 保证只保存一次
  useEffect(() => {
    if (!open) return;
    const h = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 's') {
        e.preventDefault();
        e.stopPropagation();
        void handleSave();
      }
    };
    window.addEventListener('keydown', h, true);
    return () => window.removeEventListener('keydown', h, true);
  }, [open, handleSave]);

  // 跳转后内容区滚回顶部
  useEffect(() => {
    if (contentRef.current) contentRef.current.scrollTop = 0;
  }, [currentPath]);

  // 关闭后不立刻塌掉子树，留给 Modal 播退场动画
  if (!present) return null;

  const fileName = currentPath.replace(/\\/g, '/').split('/').pop() ?? currentPath;

  return (
    <Modal open={open} onClose={requestClose} align="center" ariaLabel={fileName}
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
            {dirty && (
              <span
                className="w-1.5 h-1.5 rounded-full bg-[var(--accent)] flex-shrink-0"
                title={t('fileViewer.unsaved')}
              />
            )}
            <span className="text-sm text-[var(--text-muted)] truncate">
              {currentPath}
            </span>
          </div>
          <div className="flex items-center gap-2 flex-shrink-0">
            {canEdit && (
              <button
                className={`px-2.5 py-1 text-xs rounded-[var(--radius-sm)] transition-colors ${
                  dirty
                    ? 'bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90'
                    : 'border border-[var(--border-default)] text-[var(--text-muted)] cursor-default'
                }`}
                onClick={() => void handleSave()}
                disabled={!dirty || saving}
                title="Ctrl+S"
              >
                {saving ? t('fileViewer.saving') : t('fileViewer.save')}
              </button>
            )}
            {(isMd || isHtml) && canEdit && (
              <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
                <button
                  className={`px-2.5 py-1 transition-colors ${preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                  onClick={() => {
                    // 快照当下草稿：预览渲染的是「正在编辑的内容」，不是磁盘旧文
                    setPreviewDraft(draftRef.current !== savedRef.current ? draftRef.current : null);
                    setPreview(true);
                  }}
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
              onClick={requestClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 保存失败 / 外部修改提示条 */}
        {saveError && (
          <div
            className="px-4 py-1.5 text-xs text-[var(--color-error)] bg-[var(--color-error-muted)] border-b border-[var(--border-subtle)] flex-shrink-0 truncate"
            title={saveError}
          >
            {t('fileViewer.saveFailed')}: {saveError}
          </div>
        )}
        {extChanged && (
          <div className="px-4 py-1.5 text-xs text-[var(--color-warning)] bg-[var(--accent-subtle)] border-b border-[var(--border-subtle)] flex-shrink-0 flex items-center gap-3">
            <span>{t('fileViewer.externallyChanged')}</span>
            <button
              className="underline hover:text-[var(--text-primary)] transition-colors"
              onClick={() => {
                setExtChanged(false);
                setReloadNonce((n) => n + 1);
              }}
            >
              {t('fileViewer.reloadDiscard')}
            </button>
          </div>
        )}

        {/* 内容区 */}
        <div ref={contentRef} className="flex-1 overflow-auto bg-[var(--bg-base)]">
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
          {canEdit && result && (
            <>
              {isHtml && preview && (
                <iframe
                  srcDoc={htmlSrcDoc}
                  title={fileName}
                  className="w-full h-full border-0 bg-white"
                  sandbox="allow-same-origin"
                />
              )}
              {isMd && preview && (
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
                    {previewDraft ?? diskContent}
                  </ReactMarkdown>
                </div>
              )}
              {/* 编辑器在预览时只隐藏不卸载：保住未保存的草稿与撤销栈 */}
              <div className={(isMd || isHtml) && preview ? 'hidden' : 'h-full'}>
                <CodeEditor
                  value={result.content}
                  fileName={fileName}
                  highlightLine={currentPath === filePath ? highlightLine : undefined}
                  autoFocus={!(isMd || isHtml) || !preview}
                  onDocChange={handleDocChange}
                  onSave={() => void handleSave()}
                  apiRef={editorApiRef}
                />
              </div>
            </>
          )}
        </div>
    </Modal>
  );
}
