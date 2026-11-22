import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  clearCommandReceipt,
  closeMirror,
  loadOlderMirror,
  sendMobileCommand,
  useRelayStore,
} from './relay';
import { useT } from './i18n';
import { RenameSheet } from './RenameSheet';
import { isAtBottom, MirrorRowList, MirrorSkeleton } from './MirrorContent';
import { buildMirrorRows, hasActiveQuestion, type MirrorRow } from './mirrorRows';
import type { MirrorMessage } from './protocol';

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
const CommandComposer = memo(function CommandComposer() {
  const t = useT();
  // 细粒度订阅:只关心开关与发送回执,AI 每秒推新消息时不打扰正在输入的输入框
  const mirrorAvailable = useRelayStore((s) => s.mirror !== null && !s.mirror.closed);
  const receipt = useRelayStore((s) => s.mirror?.receipt ?? null);
  const sending = useRelayStore((s) => s.mirror?.pendingCommandId != null);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const [text, setText] = useState('');
  const inputRef = useRef<HTMLTextAreaElement>(null);

  const disabled = !mirrorAvailable || desktopOnline === false;

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
});

const EMPTY_MESSAGES: MirrorMessage[] = [];

/**
 * 对话镜像页:按时间混排的桌面输入 / AI 回复 / 工具活动,上拉加载更早,实时追加,
 * 尾部挂 AI 工作中 / 等你处理的状态行。
 */
export function MirrorView() {
  const t = useT();
  // 细粒度订阅 mirror 渲染实际用到的字段:发指令回执 / answeredProgress 等变动不再驱动整页重渲染
  const hasMirror = useRelayStore((s) => s.mirror !== null);
  const paneId = useRelayStore((s) => s.mirror?.paneId ?? '');
  const title = useRelayStore((s) => s.mirror?.title ?? '');
  const closed = useRelayStore((s) => s.mirror?.closed ?? false);
  const hasMore = useRelayStore((s) => s.mirror?.hasMore ?? false);
  const loaded = useRelayStore((s) => s.mirror?.loaded ?? false);
  const loadingOlder = useRelayStore((s) => s.mirror?.loadingOlder ?? false);
  const messages = useRelayStore((s) => s.mirror?.messages ?? EMPTY_MESSAGES);
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

  // 最近一次被点开/收起的工具组(首条 seq),供展开后的贴底判定认出是哪一行
  const lastToggledTools = useRef<number | null>(null);
  const toggleTools = useCallback((firstSeq: number) => {
    lastToggledTools.current = firstSeq;
    setExpandedTools((prev) => {
      const next = new Set(prev);
      if (!next.delete(firstSeq)) next.add(firstSeq);
      return next;
    });
  }, []);

  // 用户主动展开/收起某一行后按实际几何重算贴底。内容在视口下方长出来不会触发
  // scroll 事件,不重算的话 stickToBottom 还停在 true,下一条消息一来就把正在读的
  // 展开内容甩走。
  //
  // 例外:此前贴着底、动的又是最后一行(最常见的是展开正在跑的那组工具)。内容
  // 往视口下方长,按几何重算会判成离底,之后新到的工具与状态行就不再跟随,用户
  // 得自己往下翻 —— 这种情况继续贴底。此刻 stickToBottom 仍是展开前的值:下方长
  // 内容不改 scrollTop,不会有 scroll 事件先把它改掉。
  const syncStick = useCallback((rowKey: number) => {
    const el = scrollRef.current;
    if (!el) return;
    const committed = rowsCache.current;
    const isLastRow = committed.length > 0 && committed[committed.length - 1].key === rowKey;
    if (stickToBottom.current && isLastRow) {
      el.scrollTop = el.scrollHeight;
      return;
    }
    stickToBottom.current = isAtBottom(el);
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

  useLayoutEffect(() => {
    const key = lastToggledTools.current;
    if (key === null) return;
    lastToggledTools.current = null;
    syncStick(key);
  }, [expandedTools, syncStick]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottom.current = isAtBottom(el);
    // 滚到顶部附近自动加载更早
    if (el.scrollTop < 30) requestOlder();
  }, [requestOlder]);

  if (!hasMirror) return null;

  return (
    <div className="mirror-view">
      <div className="mirror-header">
        <button className="mirror-back" onClick={closeMirror}>
          ‹ {t('mirror.back')}
        </button>
        {/* 标题即改名入口。会话已结束/桌面端离线时改不动:pane 没了或消息送不到 */}
        <button
          className="mirror-title"
          disabled={closed || desktopOnline === false}
          title={t('sessions.rename.action')}
          onClick={() => setRenaming(true)}
        >
          {title}
        </button>
      </div>

      {renaming && (
        <RenameSheet paneId={paneId} current={title} onClose={() => setRenaming(false)} />
      )}

      {desktopOnline === false && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
        </div>
      )}

      {closed && (
        <div className="mirror-closed">
          <div className="mirror-closed-text">{t('mirror.paneClosed')}</div>
          <button className="mirror-closed-btn" onClick={closeMirror}>
            {t('mirror.backToList')}
          </button>
        </div>
      )}

      <div className="mirror-scroll" ref={scrollRef} onScroll={onScroll}>
        {hasMore && (
          <button
            className="mirror-load-older"
            onClick={requestOlder}
            disabled={loadingOlder}
          >
            {loadingOlder ? t('mirror.loading') : t('mirror.loadOlder')}
          </button>
        )}
        {!loaded ? (
          // 骨架屏而不是一行「加载中…」：首屏要拉整段会话记录，纯文字会让人以为卡住了
          <MirrorSkeleton label={t('mirror.loading')} />
        ) : rows.length === 0 ? (
          <div className="mirror-empty">{t('mirror.empty')}</div>
        ) : (
          <MirrorRowList
            rows={rows}
            expandedTools={expandedTools}
            onToggleTools={toggleTools}
            onUserResize={syncStick}
          />
        )}
        {indicator && <ActivityIndicator key={indicator} kind={indicator} />}
      </div>

      <CommandComposer />
    </div>
  );
}
