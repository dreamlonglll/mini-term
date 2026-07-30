import { useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import {
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

function MessageRow({ msg }: { msg: MirrorMessage }) {
  const t = useT();
  const isAssistant = msg.source === 'assistant';
  return (
    <div className={`mirror-msg ${isAssistant ? 'from-assistant' : 'from-input'}`}>
      <div className="mirror-msg-source">{t(sourceKey(msg.source))}</div>
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
          mirror.messages.map((m) => <MessageRow key={m.seq} msg={m} />)
        )}
      </div>

      <CommandComposer />
    </div>
  );
}
