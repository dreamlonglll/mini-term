import { memo, useLayoutEffect, useRef, useState } from 'react';
import { answerQuestion, useRelayStore } from './relay';
import { useT } from './i18n';
import { Markdown } from './MirrorMarkdown';
import type { MirrorMessage } from './protocol';

function sourceKey(source: string): string {
  switch (source) {
    case 'assistant':
      return 'mirror.source.assistant';
    case 'mobile':
      return 'mirror.source.mobile';
    default:
      return 'mirror.source.desktop';
  }
}

/**
 * ISO 8601 → 本地时钟「月-日 时:分:秒」。
 * 与桌面端会话查看同一口径(`session_panel::format_message_time`)。
 */
export function formatMessageTime(raw: string): string | null {
  if (!raw) return null;
  const d = new Date(raw);
  if (Number.isNaN(d.getTime())) return null;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 距底部多近算「贴着底」:留点余量,手指轻微回弹不至于脱离跟随 */
export function isAtBottom(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

/** 输入内容超过这个高度才折叠;折叠后的高度由样式 `.input-clip.is-collapsed` 定 */
const COLLAPSE_THRESHOLD = 320;

function CollapsibleInput({
  content,
  rowKey,
  onUserResize,
}: {
  content: string;
  rowKey: number;
  onUserResize?: (rowKey: number) => void;
}) {
  const t = useT();
  const preRef = useRef<HTMLPreElement>(null);
  const toggleRef = useRef<HTMLButtonElement>(null);
  const userToggled = useRef(false);
  const [tall, setTall] = useState(false);
  const [expanded, setExpanded] = useState(false);

  useLayoutEffect(() => {
    const el = preRef.current;
    if (!el) return;
    const measure = () => setTall(el.offsetHeight > COLLAPSE_THRESHOLD);
    measure();
    if (typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useLayoutEffect(() => {
    if (!userToggled.current) return;
    userToggled.current = false;
    if (!expanded) toggleRef.current?.scrollIntoView({ block: 'nearest' });
    onUserResize?.(rowKey);
  }, [expanded, rowKey, onUserResize]);

  const collapsed = tall && !expanded;
  return (
    <>
      <div className={`input-clip${collapsed ? ' is-collapsed' : ''}`}>
        <pre ref={preRef} className="plain-input">
          {content}
        </pre>
      </div>
      {tall && (
        <button
          ref={toggleRef}
          type="button"
          className="input-toggle"
          aria-expanded={expanded}
          onClick={() => {
            userToggled.current = true;
            setExpanded((v) => !v);
          }}
        >
          {t(expanded ? 'mirror.input.collapse' : 'mirror.input.expand')}
        </button>
      )}
    </>
  );
}

export const MessageRow = memo(function MessageRow({
  msg,
  onUserResize,
}: {
  msg: MirrorMessage;
  onUserResize?: (rowKey: number) => void;
}) {
  const t = useT();
  const isAssistant = msg.source === 'assistant';
  const time = formatMessageTime(msg.timestamp);
  return (
    <div className={`mirror-msg ${isAssistant ? 'from-assistant' : 'from-input'}`}>
      <div className="mirror-msg-source">
        <span>{t(sourceKey(msg.source))}</span>
        {time && <time className="mirror-msg-time">{time}</time>}
      </div>
      <div className="mirror-msg-body">
        {isAssistant ? (
          <div className="markdown">
            <Markdown content={msg.content} />
          </div>
        ) : (
          <CollapsibleInput content={msg.content} rowKey={msg.seq} onUserResize={onUserResize} />
        )}
      </div>
    </div>
  );
});

/** 打断标记:居中的灰色小提示 */
export const InterruptRow = memo(function InterruptRow({ msg }: { msg: MirrorMessage }) {
  const t = useT();
  const time = formatMessageTime(msg.timestamp);
  return (
    <div className="mirror-interrupt" role="note">
      <span>{t('mirror.interrupted')}</span>
      {time && <time className="mirror-msg-time">{time}</time>}
    </div>
  );
});

/**
 * agent 提问卡片:实时镜像里可点选作答,`readOnly` 模式(历史会话预览)只做只读展示。
 */
export const QuestionCard = memo(function QuestionCard({
  msg,
  active,
  marker,
  readOnly = false,
}: {
  msg: MirrorMessage;
  active: boolean;
  marker: MirrorMessage | null;
  readOnly?: boolean;
}) {
  const t = useT();
  const mirrorOpen = useRelayStore((s) => s.mirror !== null && !s.mirror.closed);
  const sending = useRelayStore((s) => s.mirror?.pendingCommandId != null);
  const progress = useRelayStore((s) => s.mirror?.answeredProgress[msg.seq] ?? 0);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const answered = marker != null;
  const chosen = marker?.labels ?? [];
  const canAnswer = !readOnly && active && !answered && mirrorOpen && desktopOnline !== false;
  const time = formatMessageTime(msg.timestamp);
  const items = msg.questions ?? [];
  const questionId = msg.questionId ?? '';
  return (
    <div className="mirror-msg from-assistant">
      <div className="mirror-msg-source">
        <span>{t('mirror.source.question')}</span>
        {time && <time className="mirror-msg-time">{time}</time>}
      </div>
      <div className={`question-card${canAnswer ? '' : ' inactive'}`}>
        {items.length === 0 ? (
          <pre className="plain-input">{msg.content}</pre>
        ) : (
          items.map((item, qi) => (
            <div key={qi} className="question-item">
              {item.header && <div className="question-header">{item.header}</div>}
              <div className="question-text">{item.question}</div>
              <div className="question-options">
                {item.options.map((opt, oi) => (
                  <button
                    key={oi}
                    className={`question-option${chosen.includes(opt.label) ? ' chosen' : ''}`}
                    disabled={
                      !canAnswer ||
                      sending ||
                      item.multiSelect === true ||
                      !questionId ||
                      qi !== progress
                    }
                    onClick={() => answerQuestion(msg.seq, questionId, qi, oi)}
                  >
                    <span className="question-option-label">{opt.label}</span>
                    {opt.description && (
                      <span className="question-option-desc">{opt.description}</span>
                    )}
                  </button>
                ))}
              </div>
              {!readOnly && item.multiSelect === true && (
                <div className="question-hint">{t('mirror.question.multiSelectHint')}</div>
              )}
            </div>
          ))
        )}
        {answered ? (
          <div className="question-state answered">
            {chosen.length > 0
              ? `${t('mirror.question.answered')}: ${chosen.join(', ')}`
              : t('mirror.question.handled')}
          </div>
        ) : !readOnly && !active ? (
          <div className="question-state stale">{t('mirror.question.expired')}</div>
        ) : null}
      </div>
    </div>
  );
});

function toolLabel(m: MirrorMessage): string {
  if (!m.toolName) return m.content;
  return m.toolTarget ? `${m.toolName} ${m.toolTarget}` : m.toolName;
}

export const ToolGroupRow = memo(function ToolGroupRow({
  firstSeq,
  tools,
  expanded,
  onToggle,
}: {
  firstSeq: number;
  tools: readonly MirrorMessage[];
  expanded: boolean;
  onToggle: (firstSeq: number) => void;
}) {
  const t = useT();
  const latest = tools[tools.length - 1];
  return (
    <div className={`tool-group${expanded ? ' is-expanded' : ''}`}>
      <button
        type="button"
        className="tool-group-head"
        aria-expanded={expanded}
        onClick={() => onToggle(firstSeq)}
      >
        <span className="tool-group-chevron" aria-hidden="true">
          ›
        </span>
        <span className="tool-group-count">
          {t('mirror.tools.summary', { count: tools.length })}
        </span>
        <span className="tool-group-latest">
          · {t('mirror.tools.latest', { tool: toolLabel(latest) })}
        </span>
      </button>
      {expanded && (
        <ol className="tool-group-list">
          {tools.map((m) => (
            <li key={m.seq} className="tool-item">
              {m.toolName ? (
                <>
                  <span className="tool-name">{m.toolName}</span>
                  {m.toolTarget && <span className="tool-target">{m.toolTarget}</span>}
                </>
              ) : (
                <span className="tool-target">{m.content}</span>
              )}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
});

export function MirrorSkeleton({ label }: { label: string }) {
  return (
    <div className="mirror-skeleton" aria-label={label} aria-busy="true">
      {[0, 1, 2].map((i) => (
        <div key={i} className={`skeleton-msg ${i % 2 ? 'from-input' : 'from-assistant'}`}>
          <div className="skeleton-line w-40" />
          <div className="skeleton-line w-full" />
          <div className="skeleton-line w-75" />
        </div>
      ))}
    </div>
  );
}
