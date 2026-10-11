import { isValidElement, memo, useEffect, useRef, useState, type ReactNode } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { useT } from './i18n';

/**
 * 复制文本到剪贴板。返回是否成功。
 *
 * 优先 `navigator.clipboard`,但它只在安全上下文里存在:PWA 走内网 `ws://` 中转、
 * 用 http 打开时根本拿不到,这时退回隐藏 textarea + `execCommand('copy')`。
 * 有 API 时同步发起 writeText,保证调用还落在点按手势里(iOS 只认手势内的复制)。
 */
async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      /* 权限被拒等:落到下面的老办法再试一次 */
    }
  }
  return legacyCopy(text);
}

function legacyCopy(text: string): boolean {
  const prevFocus = document.activeElement;
  const ta = document.createElement('textarea');
  ta.value = text;
  // readonly:iOS 聚焦时不弹键盘;16px:iOS 不因聚焦小字号输入框而放大页面;
  // 移出视口而不是 display:none——后者选不中,复制出来是空的
  ta.setAttribute('readonly', '');
  ta.style.position = 'fixed';
  ta.style.top = '0';
  ta.style.left = '-9999px';
  ta.style.fontSize = '16px';
  document.body.appendChild(ta);
  ta.select();
  ta.setSelectionRange(0, text.length); // iOS 上 select() 不全选,要再框一次
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  document.body.removeChild(ta);
  // 还焦点:用户可能正在指令框里打字,复制一下不该把输入状态打断
  if (prevFocus instanceof HTMLElement) prevFocus.focus();
  return ok;
}

/** 从 react-markdown 交给 `pre` 的 `<code class="language-xx">` 子元素上取语言名 */
function codeLanguage(children: ReactNode): string | null {
  if (!isValidElement<{ className?: string }>(children)) return null;
  const m = /language-(\S+)/.exec(children.props.className ?? '');
  return m ? m[1] : null;
}

type CopyState = 'idle' | 'copied' | 'failed';

/**
 * 代码块:顶栏左边语言名、右边复制按钮。
 *
 * 按钮放顶栏而不是浮在代码上:浮层会盖住首行右端,长行横向滚动时更是一直挡着。
 * 复制取渲染后 `<pre>` 的 textContent,不必在 hast 里自己拼文本。
 */
function CodeBlock({ children }: { children?: ReactNode }) {
  const t = useT();
  const preRef = useRef<HTMLPreElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [state, setState] = useState<CopyState>('idle');
  const lang = codeLanguage(children);

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  const onCopy = async () => {
    // 围栏代码块的文本带一个结尾换行,粘贴进终端会多回车一次
    const text = (preRef.current?.textContent ?? '').replace(/\n$/, '');
    const ok = await copyText(text);
    setState(ok ? 'copied' : 'failed');
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setState('idle'), 1500);
  };

  return (
    <div className="code-block">
      <div className="code-block-bar">
        <span className="code-block-lang">{lang ?? ''}</span>
        <button
          type="button"
          className={`code-copy${state === 'idle' ? '' : ` is-${state}`}`}
          onClick={onCopy}
        >
          {t(`mirror.code.${state === 'idle' ? 'copy' : state}`)}
        </button>
      </div>
      <pre ref={preRef}>{children}</pre>
    </div>
  );
}

/** GFM 表格:外包一层横向滚动容器,宽表在窄屏上自己滚,不把整条气泡撑出屏幕 */
function MarkdownTable({ children }: { children?: ReactNode }) {
  return (
    <div className="md-table-wrap">
      <table>{children}</table>
    </div>
  );
}

/**
 * 链接一律新开页。PWA 独立窗口里原地跳转会把整个应用换掉——中转连接断开、
 * 镜像状态清空,回来还得重进会话。GFM 自动链接让这种点击变得很常见。
 * 页内锚点(脚注)除外。
 */
function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }) {
  if (href?.startsWith('#')) return <a href={href}>{children}</a>;
  return (
    <a href={href} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}

// 插件与组件表提成模块级常量:每次渲染新建的话 react-markdown 认为配置变了,
// memo 掉的行也挡不住重建
const REMARK_PLUGINS = [remarkGfm];
const MARKDOWN_COMPONENTS: Components = {
  pre: CodeBlock,
  table: MarkdownTable,
  a: MarkdownLink,
};

/**
 * AI 回复的 Markdown 正文(GFM:表格 / 删除线 / 任务列表 / 自动链接)。
 * 单独 memo:切换语言时外层行会重渲染,正文内容没变就不必重新解析。
 */
export const Markdown = memo(function Markdown({ content }: { content: string }) {
  return (
    <ReactMarkdown remarkPlugins={REMARK_PLUGINS} components={MARKDOWN_COMPONENTS}>
      {content}
    </ReactMarkdown>
  );
});
