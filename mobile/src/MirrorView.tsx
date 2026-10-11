import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  answerQuestion,
  clearCommandReceipt,
  closeMirror,
  loadOlderMirror,
  sendMobileCommand,
  useRelayStore,
} from './relay';
import { useT } from './i18n';
import { RenameSheet } from './RenameSheet';
import { Markdown } from './MirrorMarkdown';
import { buildMirrorRows, hasActiveQuestion, type MirrorRow } from './mirrorRows';
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
 *
 * 与桌面端会话查看**同一口径**(`session_panel::format_message_time`):同一条
 * 消息在两端读出来的时刻必须一样,否则对着两块屏幕核对时会以为丢了消息。
 * 不用 `toLocaleString`——它随系统区域在 `8/24/2026` 与 `2026/8/24` 之间跳,
 * 两端对不齐。
 *
 * 解析不出来返回 `null`,那条就不显示时间:宁可少一行灰字,也不显示
 * `1970-01-01`(桌面端同规矩)。
 */
function formatTime(raw: string): string | null {
  if (!raw) return null;
  const d = new Date(raw);
  if (Number.isNaN(d.getTime())) return null;
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 距底部多近算「贴着底」:留点余量,手指轻微回弹不至于脱离跟随 */
function isAtBottom(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

/** 输入内容超过这个高度才折叠;折叠后的高度由样式 `.input-clip.is-collapsed` 定 */
const COLLAPSE_THRESHOLD = 320;

/**
 * 桌面输入 / 移动端指令的正文:贴进来的长日志、长 prompt 默认折叠,底部渐隐 +
 * 展开按钮。AI 回复不折叠——那是用户来镜像页要看的东西。
 *
 * 首次测高放 layout effect:状态更新在绘制前同步落地,长内容不会先整段闪一帧
 * 再缩回去;之后宽度变化(转屏)交给 ResizeObserver。测的是内层 `<pre>`,
 * 它不受外层 max-height 约束,量到的始终是自然高度。
 */
function CollapsibleInput({
  content,
  onUserResize,
}: {
  content: string;
  onUserResize: () => void;
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

  // 收起一段读到一半的长内容时,按钮会随正文缩回视口上方;把它拉回视口,
  // 手指还在原处。随后按实际几何重算贴底(见 MirrorView 的 syncStick)
  useLayoutEffect(() => {
    if (!userToggled.current) return;
    userToggled.current = false;
    if (!expanded) toggleRef.current?.scrollIntoView({ block: 'nearest' });
    onUserResize();
  }, [expanded, onUserResize]);

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

const MessageRow = memo(function MessageRow({
  msg,
  onUserResize,
}: {
  msg: MirrorMessage;
  onUserResize: () => void;
}) {
  const t = useT();
  const isAssistant = msg.source === 'assistant';
  const time = formatTime(msg.timestamp);
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
          <CollapsibleInput content={msg.content} onUserResize={onUserResize} />
        )}
      </div>
    </div>
  );
});

/** 打断标记:居中的灰色小提示,不占输入气泡的位置(识别规则见 isInterruptMarker) */
const InterruptRow = memo(function InterruptRow({ msg }: { msg: MirrorMessage }) {
  const t = useT();
  const time = formatTime(msg.timestamp);
  return (
    <div className="mirror-interrupt" role="note">
      <span>{t('mirror.interrupted')}</span>
      {time && <time className="mirror-msg-time">{time}</time>}
    </div>
  );
});

/**
 * agent 提问卡片:题目 + 选项按钮,点按即作答(桌面端向终端注入按键完成选择)。
 *
 * - `active` = 该提问之后没有非工具消息且未见作答标记;其后出现任何文本消息都
 *   视为已在桌面端处理(作答或打断),按钮失效只留展示。工具消息不算——同一回合
 *   并行的工具调用会排在提问后面(判定在 mirrorRows.buildMirrorRows);
 * - `marker` = 回流的 questionAnswered 标记,选中项取其结构化 labels
 *   (labels 为空 = 打断/旧版记录给不出选中项,显示中性「已处理」);
 * - 逐题推进:只放行本地作答进度指向的下一道题(与桌面端校验同一口径),
 *   回执 ok 即推进,不等标记回流——堵住 1s 轮询间隙里按钮复活可再点的毛刺;
 * - 多选题 v1 只展示不可点选(注入按键无法可靠表达多选);
 * - 结构化题目被旧链路丢掉时退化为纯文本兜底(content)。
 *
 * store 只按需取标量:订阅整个 mirror 的话每次追加消息都会穿透 memo 重渲染。
 */
const QuestionCard = memo(function QuestionCard({
  msg,
  active,
  marker,
}: {
  msg: MirrorMessage;
  active: boolean;
  marker: MirrorMessage | null;
}) {
  const t = useT();
  const mirrorOpen = useRelayStore((s) => s.mirror !== null && !s.mirror.closed);
  const sending = useRelayStore((s) => s.mirror?.pendingCommandId != null);
  const progress = useRelayStore((s) => s.mirror?.answeredProgress[msg.seq] ?? 0);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const answered = marker != null;
  const chosen = marker?.labels ?? [];
  const canAnswer = active && !answered && mirrorOpen && desktopOnline !== false;
  const time = formatTime(msg.timestamp);
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
              {item.multiSelect === true && (
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
        ) : !active ? (
          <div className="question-state stale">{t('mirror.question.expired')}</div>
        ) : null}
      </div>
    </div>
  );
});

/** 工具调用的一行展示文本;旧中转吃掉 toolName/toolTarget 时退回 content 兜底 */
function toolLabel(m: MirrorMessage): string {
  if (!m.toolName) return m.content;
  return m.toolTarget ? `${m.toolName} ${m.toolTarget}` : m.toolName;
}

/**
 * 工具活动折叠条:连续的工具调用并成一行「工具调用 N 次 · 最近:…」,点按展开明细。
 *
 * 一个回合里 agent 常连跑几十次 Read / Bash,逐条成行会把真正的回复挤出屏幕;
 * 折叠后既看得出它在干活、最近碰了什么,又不占地方。展开状态由 MirrorView 按组首条
 * seq 持有——新工具追加到尾组时组 key 不变,用户已展开的组不会被收起来。
 */
const ToolGroupRow = memo(function ToolGroupRow({
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

/** 镜像页尾部的状态指示:working = AI 工作中;attention = 等你处理;answer = 等你作答上方提问 */
type Indicator = 'working' | 'attention' | 'answer';

/**
 * 当前镜像 pane 的活动状态,取自会话列表的结构快照。
 *
 * 只返回标量,store 每次推送 projects 时结果没变就不触发重渲染。
 * 镜像已结束、桌面端离线(此时状态是断线前的残影)、pane 不在列表里、
 * 出错(PTY 已死,桌面端 error 优先于黄灯)都返回 null。
 * 黄灯优先于工作中,与会话列表同口径:用户此刻该做的事比 AI 在不在跑重要。
 */
function selectActivity(
  s: ReturnType<typeof useRelayStore.getState>,
): 'working' | 'attention' | null {
  const m = s.mirror;
  if (!m || m.closed || s.desktopOnline === false) return null;
  for (const project of s.projects) {
    const pane = project.panes.find((p) => p.paneId === m.paneId);
    if (!pane) continue;
    if (pane.status === 'error') return null;
    if (pane.needsAttention === true) return 'attention';
    return pane.status === 'ai-working' ? 'working' : null;
  }
  return null;
}

/**
 * 尾部状态行。按状态换 key 重新挂载,每次切换都自下而上滑入——方向性位移,
 * 不做从透明起播的淡入(深色背景上读起来像闪烁)。
 */
const ActivityIndicator = memo(function ActivityIndicator({ kind }: { kind: Indicator }) {
  const t = useT();
  if (kind === 'working') {
    return (
      <div className="mirror-activity is-working" role="status">
        <span className="activity-dots" aria-hidden="true">
          <span />
          <span />
          <span />
        </span>
        <span>{t('mirror.activity.working')}</span>
      </div>
    );
  }
  if (kind === 'answer') {
    // 已有一张可作答的卡片:卡片本身就是醒目的黄框,这里只轻声指个路,不再抢眼
    return (
      <div className="mirror-activity is-answer" role="status">
        <span aria-hidden="true">↑</span>
        <span>{t('mirror.activity.answerQuestion')}</span>
      </div>
    );
  }
  return (
    <div className="mirror-activity is-attention" role="status">
      <span className="activity-badge" aria-hidden="true">
        !
      </span>
      <span className="activity-text">
        <span className="activity-title">{t('mirror.activity.attention')}</span>
        <span className="activity-hint">{t('mirror.activity.attentionHint')}</span>
      </span>
    </div>
  );
});

/** 镜像底部的指令输入区:桌面离线/会话结束置灰;发送后展示回执。 */
function CommandComposer() {
  const t = useT();
  const mirror = useRelayStore((s) => s.mirror);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const [text, setText] = useState('');
  const inputRef = useRef<HTMLTextAreaElement>(null);

  const receipt = mirror?.receipt ?? null;
  const sending = mirror?.pendingCommandId != null;
  const disabled = !mirror || mirror.closed || desktopOnline === false;

  // 回执短暂展示后自动清除
  useEffect(() => {
    if (!receipt) return;
    const timer = setTimeout(clearCommandReceipt, receipt.ok ? 2500 : 5000);
    return () => clearTimeout(timer);
  }, [receipt]);

  // 输入框随内容自增高（最多 6 行）。rows=1 固定高度时，稍长一点的指令就只能
  // 从一条缝里往外看，改完更是无从复核。
  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.style.height = 'auto';
    const line = parseFloat(getComputedStyle(el).lineHeight) || 20;
    el.style.height = `${Math.min(el.scrollHeight, line * 6 + 16)}px`;
  }, [text]);

  const submit = () => {
    if (disabled || sending) return;
    if (sendMobileCommand(text)) setText('');
  };

  let notice: { text: string; ok: boolean } | null = null;
  if (receipt) {
    notice = receipt.ok
      ? { text: t('mirror.receiptOk'), ok: true }
      : { text: t(`mirror.receiptFail.${receipt.reason ?? 'writeFailed'}`), ok: false };
  }

  return (
    <div className="composer">
      {notice && (
        <div className={`composer-receipt ${notice.ok ? 'ok' : 'fail'}`}>{notice.text}</div>
      )}
      {disabled && desktopOnline === false && (
        <div className="composer-hint">{t('mirror.offlineCannotSend')}</div>
      )}
      <div className="composer-row">
        <textarea
          ref={inputRef}
          className="composer-input"
          value={text}
          rows={1}
          placeholder={t('mirror.inputPlaceholder')}
          disabled={disabled}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <button
          className="composer-send"
          disabled={disabled || sending || !text.trim()}
          onClick={submit}
        >
          {sending ? t('mirror.sending') : t('mirror.send')}
        </button>
      </div>
    </div>
  );
}

const EMPTY_MESSAGES: MirrorMessage[] = [];

/**
 * 对话镜像页:按时间混排的桌面输入 / AI 回复 / 工具活动,上拉加载更早,实时追加,
 * 尾部挂 AI 工作中 / 等你处理的状态行。
 */
export function MirrorView() {
  const t = useT();
  const mirror = useRelayStore((s) => s.mirror);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const activity = useRelayStore(selectActivity);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  // 加载更早历史的补偿锚点:head = 请求时的首条消息对象,height = 插入前的 scrollHeight
  const historyAnchor = useRef<{ head: MirrorMessage; height: number } | null>(null);
  // 上一轮已提交的渲染行,供 buildMirrorRows 结构共享
  const rowsCache = useRef<MirrorRow[]>([]);
  const [renaming, setRenaming] = useState(false);
  const [expandedTools, setExpandedTools] = useState<ReadonlySet<number>>(() => new Set());

  const messages = mirror?.messages ?? EMPTY_MESSAGES;
  const loaded = mirror?.loaded ?? false;
  const loadingOlder = mirror?.loadingOlder ?? false;
  const rows = useMemo(() => buildMirrorRows(messages, rowsCache.current), [messages]);
  const answerPending = useMemo(() => hasActiveQuestion(rows), [rows]);
  const indicator: Indicator | null = !loaded
    ? null
    : activity === 'attention' && answerPending
      ? 'answer'
      : activity;

  useLayoutEffect(() => {
    rowsCache.current = rows;
  }, [rows]);

  const toggleTools = useCallback((firstSeq: number) => {
    setExpandedTools((prev) => {
      const next = new Set(prev);
      if (!next.delete(firstSeq)) next.add(firstSeq);
      return next;
    });
  }, []);

  // 用户主动展开/收起后按实际几何重算贴底。内容在视口下方长出来不会触发 scroll
  // 事件,不重算的话 stickToBottom 还停在 true,下一条消息一来就把正在读的
  // 展开内容甩走
  const syncStick = useCallback(() => {
    const el = scrollRef.current;
    if (el) stickToBottom.current = isAtBottom(el);
  }, []);

  /** 上滑到顶 / 点「加载更早」:先记下锚点再发请求,插入后据此保持阅读位置 */
  const requestOlder = useCallback(() => {
    const el = scrollRef.current;
    const m = useRelayStore.getState().mirror;
    if (!el || !m || !m.hasMore || m.loadingOlder || m.messages.length === 0) return;
    historyAnchor.current = { head: m.messages[0], height: el.scrollHeight };
    loadOlderMirror();
  }, []);

  // 消息变化后的滚动位置。layout effect:在绘制前定位,不闪一帧错位。
  //
  // 头部插入了更早的历史 → 保持阅读位置:新 scrollTop = 新高 - 旧高 + 旧 scrollTop。
  // 「旧 scrollTop」直接读当前值——滚动容器设了 overflow-anchor:none,插入不改
  // scrollTop,读到的就是用户此刻的位置;请求发出后用户可能还在滑,请求时记下的
  // 值会过时。iOS Safari 的滚动锚定靠不住,所以全部自己做,也关掉浏览器那份免得
  // 重复补偿。
  //
  // 快照整体替换不补偿:旧首条对象已不在新数组里(快照是重新解析出来的新对象)。
  // 等待期间尾部追加了消息,则把基线刷新成追加后的高度,补偿量只算头部那一截。
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const anchor = historyAnchor.current;
    if (anchor) {
      if (messages[0] !== anchor.head) {
        historyAnchor.current = null;
        if (messages.includes(anchor.head)) {
          const height = el.scrollHeight;
          el.scrollTop += height - anchor.height;
          // 新插进来的长输入在它自己的 layout effect 里测高后才折叠,那次同步重渲染
          // 发生在本 effect 之后、绘制之前,头部会再缩一截;下一帧绘制前按差值补回
          requestAnimationFrame(() => {
            const node = scrollRef.current;
            if (node && node.scrollHeight !== height) {
              node.scrollTop += node.scrollHeight - height;
            }
          });
          return;
        }
      } else if (!loadingOlder) {
        historyAnchor.current = null; // 空页 / 请求作罢:没有插入,锚点作废
      } else {
        anchor.height = el.scrollHeight;
      }
    }
    // 新消息到达 / 状态行出现或消失时,若此前贴着底部则继续贴底(阅读历史时不打扰)
    if (stickToBottom.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages, loaded, loadingOlder, indicator]);

  useLayoutEffect(syncStick, [expandedTools, syncStick]);

  if (!mirror) return null;

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottom.current = isAtBottom(el);
    // 滚到顶部附近自动加载更早
    if (el.scrollTop < 30) requestOlder();
  };

  const renderRow = (row: MirrorRow) => {
    switch (row.kind) {
      case 'question':
        // 作答状态由后续的 questionAnswered 标记回推;标记本身不单独成行
        return (
          <QuestionCard key={row.key} msg={row.msg} active={row.active} marker={row.marker} />
        );
      case 'tools':
        return (
          <ToolGroupRow
            key={row.key}
            firstSeq={row.key}
            tools={row.tools}
            expanded={expandedTools.has(row.key)}
            onToggle={toggleTools}
          />
        );
      case 'interrupt':
        return <InterruptRow key={row.key} msg={row.msg} />;
      default:
        return <MessageRow key={row.key} msg={row.msg} onUserResize={syncStick} />;
    }
  };

  return (
    <div className="mirror-view">
      <div className="mirror-header">
        <button className="mirror-back" onClick={closeMirror}>
          ‹ {t('mirror.back')}
        </button>
        {/* 标题即改名入口。会话已结束/桌面端离线时改不动:pane 没了或消息送不到 */}
        <button
          className="mirror-title"
          disabled={mirror.closed || desktopOnline === false}
          title={t('sessions.rename.action')}
          onClick={() => setRenaming(true)}
        >
          {mirror.title}
        </button>
      </div>

      {renaming && (
        <RenameSheet
          paneId={mirror.paneId}
          current={mirror.title}
          onClose={() => setRenaming(false)}
        />
      )}

      {desktopOnline === false && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
        </div>
      )}

      {mirror.closed && (
        <div className="mirror-closed">
          <div className="mirror-closed-text">{t('mirror.paneClosed')}</div>
          <button className="mirror-closed-btn" onClick={closeMirror}>
            {t('mirror.backToList')}
          </button>
        </div>
      )}

      <div className="mirror-scroll" ref={scrollRef} onScroll={onScroll}>
        {mirror.hasMore && (
          <button
            className="mirror-load-older"
            onClick={requestOlder}
            disabled={mirror.loadingOlder}
          >
            {mirror.loadingOlder ? t('mirror.loading') : t('mirror.loadOlder')}
          </button>
        )}
        {!mirror.loaded ? (
          // 骨架屏而不是一行「加载中…」：首屏要拉整段会话记录，纯文字会让人以为卡住了
          <div className="mirror-skeleton" aria-label={t('mirror.loading')} aria-busy="true">
            {[0, 1, 2].map((i) => (
              <div key={i} className={`skeleton-msg ${i % 2 ? 'from-input' : 'from-assistant'}`}>
                <div className="skeleton-line w-40" />
                <div className="skeleton-line w-full" />
                <div className="skeleton-line w-75" />
              </div>
            ))}
          </div>
        ) : rows.length === 0 ? (
          <div className="mirror-empty">{t('mirror.empty')}</div>
        ) : (
          rows.map(renderRow)
        )}
        {indicator && <ActivityIndicator key={indicator} kind={indicator} />}
      </div>

      <CommandComposer />
    </div>
  );
}
