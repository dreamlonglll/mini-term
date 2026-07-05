import { useState, useEffect, useMemo, useRef } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeRaw from 'rehype-raw';
import { MOD_LABEL } from '../utils/platform';
import { handleExternalLinkClick } from '../utils/externalLink';
import { useT } from '../i18n';
import type { AiSession, AiSessionMessage, RemoteSessionContent } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
  session: AiSession | null;
  projectPath: string;
}

function formatTime(iso: string): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return '';
  return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`;
}

function escapeRegex(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function HighlightText({ text, query }: { text: string; query: string }) {
  if (!query) return <>{text}</>;
  const parts = text.split(new RegExp(`(${escapeRegex(query)})`, 'gi'));
  return (
    <>
      {parts.map((part, i) =>
        part.toLowerCase() === query.toLowerCase() ? (
          <mark key={i} className="bg-[var(--color-warning,#f59e0b)]/40 text-inherit rounded-[2px] px-[1px]">{part}</mark>
        ) : (
          <span key={i}>{part}</span>
        ),
      )}
    </>
  );
}

export function SessionViewerModal({ open, onClose, session, projectPath }: Props) {
  const t = useT();
  const [messages, setMessages] = useState<AiSessionMessage[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [search, setSearch] = useState('');
  const [matchIdx, setMatchIdx] = useState(0);
  const [userIdx, setUserIdx] = useState(-1);

  const msgRefs = useRef<(HTMLDivElement | null)[]>([]);
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!open || !session) return;
    setLoading(true);
    setError('');
    setMessages([]);
    setSearch('');
    setMatchIdx(0);
    setUserIdx(-1);

    // SSH 远程会话:走 SFTP 读取链路。后端支持增量 offset(返回 nextOffset 供下次续读),
    // 与本地查看链路对齐:打开时一次性全量加载(offset=0),modal 内无刷新入口,
    // nextOffset 留给后续需要增量刷新的调用方。
    const contentPromise: Promise<AiSessionMessage[]> = session.sshConnectionId
      ? invoke<RemoteSessionContent>('ssh_remote_ai_session_content', {
          connectionId: session.sshConnectionId,
          sessionType: session.sessionType,
          sessionId: session.id,
          projectPath,
          offset: 0,
        }).then((r) => r.messages)
      : invoke<AiSessionMessage[]>('get_ai_session_content', {
          sessionType: session.sessionType,
          sessionId: session.id,
          projectPath,
          // WSL 会话:回传来源发行版,后端从对应 UNC 位置读正文
          wslDistro: session.wslDistro,
        });

    contentPromise
      .then((msgs) => {
        setMessages(msgs);
        msgRefs.current = new Array(msgs.length).fill(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, session, projectPath]);

  const userIndices = useMemo(
    () => messages.reduce<number[]>((acc, m, i) => { if (m.role === 'user') acc.push(i); return acc; }, []),
    [messages],
  );

  const q = search.trim().toLowerCase();

  const matchIndices = useMemo(() => {
    if (!q) return [];
    return messages.reduce<number[]>((acc, m, i) => {
      if (m.content.toLowerCase().includes(q)) acc.push(i);
      return acc;
    }, []);
  }, [messages, q]);

  useEffect(() => {
    setMatchIdx(0);
    if (matchIndices.length > 0) {
      msgRefs.current[matchIndices[0]]?.scrollIntoView({ behavior: 'smooth', block: 'center' });
    }
  }, [matchIndices]);

  useEffect(() => {
    if (!open) return;
    const h = (e: KeyboardEvent) => {
      if (e.key === 'f' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        searchRef.current?.focus();
      } else if (e.key === 'Escape') {
        if (search) {
          setSearch('');
        } else {
          onClose();
        }
      } else if (e.key === 'Enter' && document.activeElement === searchRef.current) {
        e.preventDefault();
        if (matchIndices.length === 0) return;
        const dir = e.shiftKey ? -1 : 1;
        setMatchIdx((prev) => {
          const next = (prev + dir + matchIndices.length) % matchIndices.length;
          msgRefs.current[matchIndices[next]]?.scrollIntoView({ behavior: 'smooth', block: 'center' });
          return next;
        });
      }
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [open, onClose, search, matchIndices]);

  const goMatch = (dir: 1 | -1) => {
    if (matchIndices.length === 0) return;
    const next = (matchIdx + dir + matchIndices.length) % matchIndices.length;
    setMatchIdx(next);
    msgRefs.current[matchIndices[next]]?.scrollIntoView({ behavior: 'smooth', block: 'center' });
  };

  const goUser = (dir: 1 | -1) => {
    if (userIndices.length === 0) return;
    let next: number;
    if (userIdx < 0) {
      next = dir === 1 ? 0 : userIndices.length - 1;
    } else {
      next = (userIdx + dir + userIndices.length) % userIndices.length;
    }
    setUserIdx(next);
    msgRefs.current[userIndices[next]]?.scrollIntoView({ behavior: 'smooth', block: 'center' });
  };

  if (!open || !session) return null;

  const typeName = session.sessionType === 'claude' ? 'Claude' : 'Codex';
  const typeColor = session.sessionType === 'claude' ? 'var(--color-ai)' : 'var(--color-success)';
  const isMatch = (i: number) => q && matchIndices.includes(i);
  const isCurrentMatch = (i: number) => q && matchIndices[matchIdx] === i;

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-center justify-center select-text" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative flex flex-col overflow-hidden bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] animate-slide-in"
        style={{ width: '90vw', height: '80vh' }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* 标题栏 */}
        <div className="flex items-center justify-between px-4 py-2.5 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2 min-w-0">
            <span
              className="flex-shrink-0 text-xs font-bold px-1.5 py-0.5 rounded"
              style={{ backgroundColor: typeColor + '22', color: typeColor }}
            >
              {typeName}
            </span>
            <span className="text-base font-medium text-[var(--text-primary)] truncate">{session.title}</span>
            {messages.length > 0 && (
              <span className="text-xs text-[var(--text-muted)] flex-shrink-0">{t("sessionViewer.messageCount", { count: messages.length })}</span>
            )}
          </div>

          <div className="flex items-center gap-3 flex-shrink-0 ml-2">
            {/* User 消息快速导航 */}
            {userIndices.length > 0 && (
              <div className="flex items-center gap-1 text-xs text-[var(--text-muted)]">
                <button className="hover:text-[var(--text-primary)] transition-colors px-0.5" onClick={() => goUser(-1)} title={t("sessionViewer.prevUserMessage")}>
                  ▲
                </button>
                <span className="min-w-[4em] text-center">
                  User {userIdx >= 0 ? userIdx + 1 : '-'}/{userIndices.length}
                </span>
                <button className="hover:text-[var(--text-primary)] transition-colors px-0.5" onClick={() => goUser(1)} title={t("sessionViewer.nextUserMessage")}>
                  ▼
                </button>
              </div>
            )}
            <button className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none" onClick={onClose}>
              ✕
            </button>
          </div>
        </div>

        {/* 搜索栏 */}
        {messages.length > 0 && (
          <div className="flex items-center gap-2 px-4 py-1.5 border-b border-[var(--border-subtle)] flex-shrink-0 bg-[var(--bg-surface)]">
            <span className="text-xs text-[var(--text-muted)]">🔍</span>
            <input
              ref={searchRef}
              type="text"
              className="flex-1 bg-transparent text-sm text-[var(--text-primary)] outline-none placeholder:text-[var(--text-muted)]"
              placeholder={t("sessionViewer.searchPlaceholder", { mod: MOD_LABEL })}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
            {q && (
              <div className="flex items-center gap-1 text-xs text-[var(--text-muted)]">
                {matchIndices.length > 0 ? (
                  <>
                    <button className="hover:text-[var(--text-primary)] px-0.5" onClick={() => goMatch(-1)}>◀</button>
                    <span>{matchIdx + 1}/{matchIndices.length}</span>
                    <button className="hover:text-[var(--text-primary)] px-0.5" onClick={() => goMatch(1)}>▶</button>
                  </>
                ) : (
                  <span>{t("sessionViewer.noMatch")}</span>
                )}
                <button className="hover:text-[var(--text-primary)] ml-1 px-0.5" onClick={() => setSearch('')}>✕</button>
              </div>
            )}
          </div>
        )}

        {/* 消息列表 */}
        <div className="flex-1 overflow-auto bg-[var(--bg-base)] p-4 space-y-4">
          {loading && <div className="flex items-center justify-center h-full text-[var(--text-muted)]">{t("sessionViewer.loading")}</div>}
          {error && <div className="flex items-center justify-center h-full text-[var(--color-error)]">{error}</div>}
          {!loading && !error && messages.length === 0 && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">{t("sessionViewer.emptyContent")}</div>
          )}

          {messages.map((msg, i) => (
            <div
              key={i}
              ref={(el) => { msgRefs.current[i] = el; }}
              className={isCurrentMatch(i) ? 'ring-2 ring-[var(--color-warning,#f59e0b)] rounded-[var(--radius-sm)]' : ''}
              style={q && !isMatch(i) ? { opacity: 0.35 } : undefined}
            >
              <div className="flex items-center gap-2 mb-1">
                <span className="text-xs font-semibold" style={{ color: msg.role === 'user' ? 'var(--text-secondary)' : typeColor }}>
                  {msg.role === 'user' ? 'User' : 'Assistant'}
                </span>
                {msg.timestamp && <span className="text-[10px] text-[var(--text-muted)]">{formatTime(msg.timestamp)}</span>}
              </div>
              <div
                className={`rounded-[var(--radius-sm)] px-3 py-2 text-sm ${
                  msg.role === 'user'
                    ? 'bg-[var(--border-subtle)] text-[var(--text-primary)]'
                    : 'bg-[var(--bg-surface)] text-[var(--text-primary)] border border-[var(--border-default)]'
                }`}
              >
                {msg.role === 'assistant' ? (
                  <div className="md-preview">
                    <ReactMarkdown
                      remarkPlugins={[remarkGfm]}
                      rehypePlugins={[rehypeRaw]}
                      components={{
                        a: ({ href, children, ...props }) => (
                          <a href={href} onClick={handleExternalLinkClick} {...props}>{children}</a>
                        ),
                      }}
                    >{msg.content}</ReactMarkdown>
                  </div>
                ) : (
                  <div style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>
                    {q ? <HighlightText text={msg.content} query={search.trim()} /> : msg.content}
                  </div>
                )}
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}
