import {
  type CSSProperties,
  memo,
  type ReactNode,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { useT } from './i18n';
import {
  selectLinkDown,
  sendTerminalKey,
  type TerminalScreenData,
  useRelayStore,
} from './relay';
import type { TerminalKey, TerminalLine, TerminalSpan } from './protocol';

type TermRenderMode = 'reflow' | 'grid';

const MODE_STORAGE_KEY = 'mini-term-mobile:term-mode';
const FONT_STORAGE_KEY = 'mini-term-mobile:term-font-size';
const MIN_FONT_SIZE = 10;
const MAX_FONT_SIZE = 18;
const DEFAULT_FONT_SIZE = 12;

function loadSavedMode(): TermRenderMode {
  try {
    return localStorage.getItem(MODE_STORAGE_KEY) === 'grid' ? 'grid' : 'reflow';
  } catch {
    return 'reflow';
  }
}

function loadSavedFontSize(): number {
  try {
    const raw = Number(localStorage.getItem(FONT_STORAGE_KEY));
    if (Number.isFinite(raw) && raw >= MIN_FONT_SIZE && raw <= MAX_FONT_SIZE) {
      return Math.round(raw);
    }
  } catch {
    // 忽略存储受限环境
  }
  return DEFAULT_FONT_SIZE;
}

/** 仅允许标准 ANSI 索引或合法 6 位十六进制颜色，防异常样式注入。 */
function resolveTermColor(raw?: string): string | undefined {
  if (!raw) return undefined;
  if (raw.startsWith('ansi:')) {
    const idx = Number(raw.slice(5));
    if (Number.isInteger(idx) && idx >= 0 && idx <= 15) {
      return `var(--ansi-${idx})`;
    }
    return undefined;
  }
  if (/^#[0-9a-fA-F]{6}$/.test(raw)) {
    return raw;
  }
  return undefined;
}

function buildSpanStyle(span: TerminalSpan): CSSProperties | undefined {
  const fg = resolveTermColor(span.fg);
  const bg = resolveTermColor(span.bg);
  const style: CSSProperties = {};
  let hasAny = false;

  if (span.inverse) {
    style.color = bg ?? 'var(--term-bg)';
    style.backgroundColor = fg ?? 'var(--term-fg)';
    hasAny = true;
  } else {
    if (fg) {
      style.color = fg;
      hasAny = true;
    }
    if (bg) {
      style.backgroundColor = bg;
      hasAny = true;
    }
  }
  if (span.bold) {
    style.fontWeight = 600;
    hasAny = true;
  }
  if (span.dim) {
    style.opacity = 0.65;
    hasAny = true;
  }
  return hasAny ? style : undefined;
}

/** 估算字符在终端网格中的显示列宽（用于定位光标落在同行哪一个字符上）。 */
function charCellWidth(ch: string): number {
  const cp = ch.codePointAt(0) ?? 0;
  if (
    (cp >= 0x1100 && cp <= 0x115f) ||
    (cp >= 0x2e80 && cp <= 0xa4cf) ||
    (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) ||
    (cp >= 0xfe10 && cp <= 0xfe6f) ||
    (cp >= 0xff00 && cp <= 0xff60) ||
    (cp >= 0xffe0 && cp <= 0xffe6) ||
    (cp >= 0x1f300 && cp <= 0x1faff) ||
    (cp >= 0x20000 && cp <= 0x3fffd)
  ) {
    return 2;
  }
  return 1;
}

function renderSpansPlain(spans: TerminalSpan[], keyPrefix: string): ReactNode[] {
  return spans.map((span, idx) => {
    const style = buildSpanStyle(span);
    return style ? (
      <span key={`${keyPrefix}-${idx}`} style={style}>
        {span.text}
      </span>
    ) : (
      <span key={`${keyPrefix}-${idx}`}>{span.text}</span>
    );
  });
}

/**
 * 渲染单行 spans，并把终端光标高亮标在目标列上。
 * - `padToCursor`: grid 模式下补齐行尾空格让光标停在精确列；reflow 模式下紧接文字末尾即可。
 */
function renderSpansWithCursor(
  spans: TerminalSpan[],
  cursorCol: number,
  padToCursor: boolean,
  keyPrefix: string,
): ReactNode[] {
  const nodes: ReactNode[] = [];
  let col = 0;
  let placed = false;

  for (let i = 0; i < spans.length; i++) {
    const span = spans[i];
    const style = buildSpanStyle(span);
    if (placed) {
      nodes.push(
        style ? (
          <span key={`${keyPrefix}-${i}`} style={style}>
            {span.text}
          </span>
        ) : (
          <span key={`${keyPrefix}-${i}`}>{span.text}</span>
        ),
      );
      continue;
    }

    const chars = Array.from(span.text);
    let before = '';
    let cursorChar = '';
    let after = '';

    for (const ch of chars) {
      const w = charCellWidth(ch);
      if (!placed && cursorCol >= col && cursorCol < col + w) {
        cursorChar = ch;
        placed = true;
      } else if (!placed) {
        before += ch;
      } else {
        after += ch;
      }
      col += w;
    }

    if (before) {
      nodes.push(
        style ? (
          <span key={`${keyPrefix}-${i}-b`} style={style}>
            {before}
          </span>
        ) : (
          <span key={`${keyPrefix}-${i}-b`}>{before}</span>
        ),
      );
    }
    if (cursorChar) {
      nodes.push(
        <span key={`${keyPrefix}-${i}-c`} className="term-cursor">
          {cursorChar}
        </span>,
      );
    }
    if (after) {
      nodes.push(
        style ? (
          <span key={`${keyPrefix}-${i}-a`} style={style}>
            {after}
          </span>
        ) : (
          <span key={`${keyPrefix}-${i}-a`}>{after}</span>
        ),
      );
    }
  }

  if (!placed) {
    if (padToCursor && cursorCol > col) {
      nodes.push(<span key={`${keyPrefix}-pad`}>{' '.repeat(cursorCol - col)}</span>);
    }
    nodes.push(
      <span key={`${keyPrefix}-cursor-end`} className="term-cursor">
        {' '}
      </span>,
    );
  }

  return nodes;
}

interface ReflowParagraph {
  key: number;
  rows: Array<{ rowIdx: number; spans: TerminalSpan[] }>;
}

/** 将连续 `wrapped: true` 的软换行归并为逻辑自然段，适配手机竖屏自然折行。 */
function buildReflowParagraphs(lines: TerminalLine[]): ReflowParagraph[] {
  const result: ReflowParagraph[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const spans = line.spans ?? [];
    if (result.length === 0 || !line.wrapped) {
      result.push({ key: i, rows: [{ rowIdx: i, spans }] });
    } else {
      result[result.length - 1].rows.push({ rowIdx: i, spans });
    }
  }
  return result;
}

interface QuickKeyDef {
  key: TerminalKey;
  label: string;
  emphasis?: 'danger' | 'accent';
}

const QUICK_KEYS: QuickKeyDef[] = [
  { key: 'escape', label: 'Esc', emphasis: 'accent' },
  { key: 'ctrlC', label: 'Ctrl+C', emphasis: 'danger' },
  { key: 'tab', label: 'Tab' },
  { key: 'shiftTab', label: '⇧Tab' },
  { key: 'up', label: '↑' },
  { key: 'down', label: '↓' },
  { key: 'left', label: '←' },
  { key: 'right', label: '→' },
  { key: 'enter', label: '↵', emphasis: 'accent' },
  { key: 'space', label: 'Space' },
  { key: 'backspace', label: '⌫' },
  { key: 'ctrlD', label: 'Ctrl+D' },
  { key: 'ctrlU', label: 'Ctrl+U' },
  { key: 'home', label: 'Home' },
  { key: 'end', label: 'End' },
  { key: 'pageUp', label: 'PgUp' },
  { key: 'pageDown', label: 'PgDn' },
];

const ScreenLines = memo(function ScreenLines({
  screen,
  mode,
}: {
  screen: TerminalScreenData;
  mode: TermRenderMode;
}) {
  const paragraphs = useMemo(
    () => (mode === 'reflow' ? buildReflowParagraphs(screen.lines) : null),
    [screen.lines, mode],
  );

  if (mode === 'reflow' && paragraphs) {
    return (
      <>
        {paragraphs.map((para) => {
          const hasCursor =
            screen.cursorVisible && para.rows.some((r) => r.rowIdx === screen.cursorRow);
          const isEmpty = para.rows.every((r) => r.spans.length === 0);
          if (isEmpty && !hasCursor) {
            return (
              <div key={para.key} className="term-line">
                {'\u00a0'}
              </div>
            );
          }
          return (
            <div key={para.key} className="term-line">
              {para.rows.map((row) =>
                screen.cursorVisible && row.rowIdx === screen.cursorRow
                  ? renderSpansWithCursor(row.spans, screen.cursorCol, false, `r${row.rowIdx}`)
                  : renderSpansPlain(row.spans, `r${row.rowIdx}`),
              )}
            </div>
          );
        })}
      </>
    );
  }

  return (
    <>
      {screen.lines.map((line, rowIdx) => {
        const spans = line.spans ?? [];
        const hasCursor = screen.cursorVisible && rowIdx === screen.cursorRow;
        if (spans.length === 0 && !hasCursor) {
          return (
            <div key={rowIdx} className="term-line">
              {'\u00a0'}
            </div>
          );
        }
        return (
          <div key={rowIdx} className="term-line">
            {hasCursor
              ? renderSpansWithCursor(spans, screen.cursorCol, true, `g${rowIdx}`)
              : renderSpansPlain(spans, `g${rowIdx}`)}
          </div>
        );
      })}
    </>
  );
});

/**
 * 移动端实时终端视图：
 * - 只读呈现桌面端活跃 AI pane 的真实终端画面（桌面端保持尺寸所有权，绝不反向 resize）；
 * - 支持「文本重排」与「原屏网格」一键切换及字号调节；
 * - 底部提供高频应急特殊按键栏，应对授权确认、TUI 菜单与中断。
 */
export const TerminalView = memo(function TerminalView() {
  const t = useT();
  const screen = useRelayStore((s) => s.mirror?.terminalScreen ?? null);
  const loaded = useRelayStore((s) => s.mirror?.terminalLoaded ?? false);
  const closed = useRelayStore((s) => s.mirror?.closed ?? false);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const linkDown = useRelayStore(selectLinkDown);

  const [mode, setMode] = useState<TermRenderMode>(loadSavedMode);
  const [fontSize, setFontSize] = useState<number>(loadSavedFontSize);
  const [showJumpBottom, setShowJumpBottom] = useState(false);
  const [pressedKey, setPressedKey] = useState<TerminalKey | null>(null);

  const viewportRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const keyFlashTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // 重连中按键发不出去(sendTerminalKey 也会拒),按钮一并置灰,不给「按下去了」的闪烁反馈
  const keysDisabled = closed || desktopOnline === false || linkDown;

  const updateMode = useCallback((next: TermRenderMode) => {
    setMode(next);
    try {
      localStorage.setItem(MODE_STORAGE_KEY, next);
    } catch {
      // 忽略
    }
  }, []);

  const stepFont = useCallback((delta: number) => {
    setFontSize((prev) => {
      const next = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, prev + delta));
      try {
        localStorage.setItem(FONT_STORAGE_KEY, String(next));
      } catch {
        // 忽略
      }
      return next;
    });
  }, []);

  useEffect(() => {
    return () => {
      if (keyFlashTimer.current) clearTimeout(keyFlashTimer.current);
    };
  }, []);

  useLayoutEffect(() => {
    const el = viewportRef.current;
    if (!el || !stickToBottom.current) return;
    el.scrollTop = el.scrollHeight;
  }, [screen, mode, fontSize]);

  const onScroll = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 36;
    stickToBottom.current = atBottom;
    setShowJumpBottom(!atBottom);
  }, []);

  const jumpToBottom = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    stickToBottom.current = true;
    el.scrollTop = el.scrollHeight;
    setShowJumpBottom(false);
  }, []);

  const handleQuickKey = useCallback(
    (key: TerminalKey) => {
      if (keysDisabled) return;
      if (sendTerminalKey(key)) {
        setPressedKey(key);
        if (keyFlashTimer.current) clearTimeout(keyFlashTimer.current);
        keyFlashTimer.current = setTimeout(() => setPressedKey(null), 160);
      }
    },
    [keysDisabled],
  );

  return (
    <div className="term-panel">
      <div className="term-toolbar">
        <div className="term-mode-group" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={mode === 'reflow'}
            className={`term-mode-btn${mode === 'reflow' ? ' is-active' : ''}`}
            onClick={() => updateMode('reflow')}
          >
            {t('mirror.terminal.reflow')}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === 'grid'}
            className={`term-mode-btn${mode === 'grid' ? ' is-active' : ''}`}
            onClick={() => updateMode('grid')}
          >
            {t('mirror.terminal.grid')}
          </button>
        </div>

        <div className="term-toolbar-right">
          {screen && (
            <span className="term-meta" title={t('mirror.terminal.sizeHint')}>
              {screen.altScreen ? 'TUI · ' : ''}
              {screen.cols}×{screen.rows}
            </span>
          )}
          <div className="term-font-group">
            <button
              type="button"
              className="term-font-btn"
              disabled={fontSize <= MIN_FONT_SIZE}
              aria-label={t('mirror.terminal.fontSmaller')}
              onClick={() => stepFont(-1)}
            >
              A-
            </button>
            <button
              type="button"
              className="term-font-btn"
              disabled={fontSize >= MAX_FONT_SIZE}
              aria-label={t('mirror.terminal.fontLarger')}
              onClick={() => stepFont(1)}
            >
              A+
            </button>
          </div>
        </div>
      </div>

      <div className="term-viewport-wrap">
        <div
          ref={viewportRef}
          className={`term-viewport ${mode === 'grid' ? 'is-grid' : 'is-reflow'}`}
          style={{ fontSize: `${fontSize}px` }}
          onScroll={onScroll}
        >
          {!loaded ? (
            <div className="term-placeholder">{t('mirror.terminal.loading')}</div>
          ) : !screen || screen.lines.length === 0 ? (
            <div className="term-placeholder">{t('mirror.terminal.empty')}</div>
          ) : (
            <div className="term-content">
              <ScreenLines screen={screen} mode={mode} />
            </div>
          )}
        </div>

        {showJumpBottom && (
          <button
            type="button"
            className="term-jump-bottom"
            aria-label={t('mirror.terminal.jumpBottom')}
            onClick={jumpToBottom}
          >
            ↓
          </button>
        )}
      </div>

      <div className="term-keybar" aria-label={t('mirror.terminal.keybar')}>
        {QUICK_KEYS.map((item) => (
          <button
            key={item.key}
            type="button"
            className={`term-key-btn${item.emphasis ? ` is-${item.emphasis}` : ''}${
              pressedKey === item.key ? ' is-flash' : ''
            }`}
            disabled={keysDisabled}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => handleQuickKey(item.key)}
          >
            {item.label}
          </button>
        ))}
      </div>
    </div>
  );
});
