import { useState, useEffect, useMemo, useRef, useCallback } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeRaw from 'rehype-raw';
import type { FileContentResult } from '../types';
import { handleExternalLinkClick } from '../utils/externalLink';

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

export function FileViewerModal({ open, onClose, filePath, projectRoot, highlightLine }: FileViewerModalProps) {
  const [result, setResult] = useState<FileContentResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const isMd = useMemo(() => isMarkdownFile(filePath), [filePath]);
  const isImg = useMemo(() => isImageFile(filePath), [filePath]);
  const isHtml = useMemo(() => isHtmlFile(filePath), [filePath]);
  const [preview, setPreview] = useState(true);
  const highlightRef = useRef<HTMLDivElement>(null);

  const htmlSrcDoc = useMemo(() => {
    if (!isHtml || !result?.content) return '';
    const normalized = filePath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return result.content.replace(
      /((?:src|href|poster)\s*=\s*["'])(?!https?:|data:|blob:|mailto:|tel:|#|javascript:)([^"']+)(["'])/gi,
      (_match, prefix, url, suffix) => prefix + convertFileSrc(fileDir + '/' + url) + suffix
    );
  }, [isHtml, result?.content, filePath]);

  const resolveImgSrc = useCallback((src: string | undefined) => {
    if (!src || /^(https?:|data:|blob:)/i.test(src)) return src;
    const normalized = filePath.replace(/\\/g, '/');
    const fileDir = normalized.substring(0, normalized.lastIndexOf('/'));
    return convertFileSrc(fileDir + '/' + src);
  }, [filePath]);

  useEffect(() => {
    if (!open || isImg) return;
    setLoading(true);
    setError('');
    setResult(null);

    invoke<FileContentResult>('read_file_content', { projectRoot, path: filePath })
      .then(setResult)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, filePath, projectRoot, isImg]);

  useEffect(() => {
    if (!open) return;
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [open, onClose]);

  useEffect(() => {
    if (result && highlightLine && highlightRef.current) {
      highlightRef.current.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  }, [result, highlightLine]);

  if (!open) return null;

  const fileName = filePath.replace(/\\/g, '/').split('/').pop() ?? filePath;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center select-text" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative flex flex-col overflow-hidden bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] animate-slide-in"
        style={{ width: '90vw', height: '80vh' }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* 工具栏 */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2">
            <span className="text-base font-medium text-[var(--accent)]">{fileName}</span>
            <span className="text-sm text-[var(--text-muted)] truncate max-w-[400px]">
              {filePath}
            </span>
          </div>
          <div className="flex items-center gap-2">
            {(isMd || isHtml) && result && !result.isBinary && !result.tooLarge && (
              <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs">
                <button
                  className={`px-2.5 py-1 transition-colors ${preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                  onClick={() => setPreview(true)}
                >
                  预览
                </button>
                <button
                  className={`px-2.5 py-1 transition-colors ${!preview ? 'bg-[var(--accent)] text-[var(--bg-base)]' : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'}`}
                  onClick={() => setPreview(false)}
                >
                  源码
                </button>
              </div>
            )}
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 内容区 */}
        <div className="flex-1 overflow-auto bg-[var(--bg-base)]">
          {loading && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              加载中...
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
                src={convertFileSrc(filePath)}
                alt={fileName}
                className="max-w-full max-h-full object-contain"
                draggable={false}
              />
            </div>
          )}
          {!isImg && result && result.isBinary && (
            <div className="flex flex-col items-center justify-center h-full gap-4 text-[var(--text-muted)]">
              <span>二进制文件，不支持预览</span>
              <button
                className="px-4 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity"
                onClick={() => invoke('open_path_with_default_app', { path: filePath })}
              >
                使用默认工具打开
              </button>
            </div>
          )}
          {!isImg && result && result.tooLarge && (
            <div className="flex flex-col items-center justify-center h-full gap-4 text-[var(--text-muted)]">
              <span>文件过大（&gt;1MB），不支持预览</span>
              <button
                className="px-4 py-1.5 text-sm rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity"
                onClick={() => invoke('open_path_with_default_app', { path: filePath })}
              >
                使用默认工具打开
              </button>
            </div>
          )}
          {!isImg && result && !result.isBinary && !result.tooLarge && isHtml && preview ? (
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
                  img: ({ src, alt, ...props }) => (
                    <img src={resolveImgSrc(src)} alt={alt ?? ''} {...props} />
                  ),
                  a: ({ href, children, ...props }) => (
                    <a href={href} onClick={handleExternalLinkClick} {...props}>{children}</a>
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
      </div>
    </div>
  );
}
