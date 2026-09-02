import { useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
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

function MessageRow({ msg }: { msg: MirrorMessage }) {
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
            <ReactMarkdown>{msg.content}</ReactMarkdown>
          </div>
        ) : (
          <pre className="plain-input">{msg.content}</pre>
        )}
      </div>
    </div>
  );
}

/**
 * agent 提问卡片:题目 + 选项按钮,点按即作答(桌面端向终端注入按键完成选择)。
 *
 * - `active` = 该提问仍是最新消息且未见作答标记;其后出现任何消息都视为已在
 *   桌面端处理(作答或打断),按钮失效只留展示;
 * - `marker` = 回流的 questionAnswered 标记,选中项取其结构化 labels
 *   (labels 为空 = 打断/旧版记录给不出选中项,显示中性「已处理」);
 * - 逐题推进:只放行本地作答进度指向的下一道题(与桌面端校验同一口径),
 *   回执 ok 即推进,不等标记回流——堵住 1s 轮询间隙里按钮复活可再点的毛刺;
 * - 多选题 v1 只展示不可点选(注入按键无法可靠表达多选);
 * - 结构化题目被旧链路丢掉时退化为纯文本兜底(content)。
 */
function QuestionCard({
  msg,
  active,
  marker,
}: {
  msg: MirrorMessage;
  active: boolean;
  marker: MirrorMessage | null;
}) {
  const t = useT();
  const mirror = useRelayStore((s) => s.mirror);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const sending = mirror?.pendingCommandId != null;
  const progress = mirror?.answeredProgress[msg.seq] ?? 0;
  const answered = marker != null;
  const chosen = marker?.labels ?? [];
  const canAnswer =
    active && !answered && !!mirror && !mirror.closed && desktopOnline !== false;
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
}

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

/** 对话镜像页:按时间混排的桌面输入 / AI 回复,上拉加载更早,实时追加。 */
export function MirrorView() {
  const t = useT();
  const mirror = useRelayStore((s) => s.mirror);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const [renaming, setRenaming] = useState(false);

  const messageCount = mirror?.messages.length ?? 0;
  const lastSeq = messageCount > 0 ? mirror!.messages[messageCount - 1].seq : -1;

  // 作答标记按 refSeq 建索引:渲染期逐卡片 find 是 O(n²),长对话可感
  const answeredMarkers = new Map<number, MirrorMessage>();
  for (const m of mirror?.messages ?? []) {
    if (m.kind === 'questionAnswered' && m.refSeq !== undefined) {
      answeredMarkers.set(m.refSeq, m);
    }
  }

  // 新消息到达时,若此前贴着底部则自动滚到底(阅读历史时不打扰)
  useEffect(() => {
    const el = scrollRef.current;
    if (el && stickToBottom.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [lastSeq, mirror?.loaded]);

  if (!mirror) return null;

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    // 滚到顶部附近自动加载更早
    if (el.scrollTop < 30 && mirror.hasMore && !mirror.loadingOlder) {
      loadOlderMirror();
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
            onClick={loadOlderMirror}
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
        ) : mirror.messages.length === 0 ? (
          <div className="mirror-empty">{t('mirror.empty')}</div>
        ) : (
          mirror.messages.map((m) => {
            if (m.kind === 'question') {
              // 作答状态由后续的 questionAnswered 标记回推;标记本身不单独成行
              const marker = answeredMarkers.get(m.seq) ?? null;
              return (
                <QuestionCard
                  key={m.seq}
                  msg={m}
                  active={m.seq === lastSeq && !marker}
                  marker={marker}
                />
              );
            }
            if (m.kind === 'questionAnswered') return null;
            return <MessageRow key={m.seq} msg={m} />;
          })
        )}
      </div>

      <CommandComposer />
    </div>
  );
}
