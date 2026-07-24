import { useEffect, useRef } from 'react';
import ReactMarkdown from 'react-markdown';
import { closeMirror, loadOlderMirror, useRelayStore } from './relay';
import { useT } from './i18n';
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

/** 对话镜像页:按时间混排的桌面输入 / AI 回复,上拉加载更早,实时追加。 */
export function MirrorView() {
  const t = useT();
  const mirror = useRelayStore((s) => s.mirror);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

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
        <span className="mirror-title">{mirror.title}</span>
      </div>

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
          <div className="mirror-loading">{t('mirror.loading')}</div>
        ) : mirror.messages.length === 0 ? (
          <div className="mirror-empty">{t('mirror.empty')}</div>
        ) : (
          mirror.messages.map((m) => <MessageRow key={m.seq} msg={m} />)
        )}
      </div>
    </div>
  );
}
