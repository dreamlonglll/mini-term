import { useEffect, useRef } from 'react';
import { Compartment, EditorState } from '@codemirror/state';
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  drawSelection,
  dropCursor,
  highlightSpecialChars,
  rectangularSelection,
  crosshairCursor,
} from '@codemirror/view';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import {
  bracketMatching,
  foldGutter,
  foldKeymap,
  indentOnInput,
  syntaxHighlighting,
  HighlightStyle,
  LanguageDescription,
} from '@codemirror/language';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import {
  search,
  searchKeymap,
  highlightSelectionMatches,
  searchPanelOpen,
  closeSearchPanel,
} from '@codemirror/search';
import { languages } from '@codemirror/language-data';
import { tags as tg } from '@lezer/highlight';
import { useI18nStore } from '../i18n';

/**
 * CodeMirror 6 编辑器封装。
 *
 * 主题不写死颜色：全部引用 styles.css 的 CSS 变量（含 `--syn-*` 语法色板，
 * 见 styles.css 的定义——它们又引用应用现有色板），因此自动跟随
 * dark / light / blueprint / fluent2 四套皮肤，切主题无需重建编辑器。
 *
 * 语言支持走 @codemirror/language-data 的按需加载：按文件名匹配到语言描述后
 * 动态 import 对应语言包，Vite 自动分包，首屏零语言包开销。
 */

/** 宿主可通过 apiRef 调用的编辑器操作 */
export interface CodeEditorApi {
  /** 搜索面板开着就关掉并返回 true；否则返回 false。
   *  宿主 Modal 的 Esc 处理用它实现「先关面板、再关弹窗」的两段式退出 */
  closeSearchIfOpen: () => boolean;
}

export interface CodeEditorProps {
  /** 初始文档内容。值变化（重载 / 换文件）时整个编辑器按新内容重建 */
  value: string;
  /** 文件名（含扩展名），用于语言探测与折行策略 */
  fileName: string;
  /** 光标定位并居中滚动到该行（1-based） */
  highlightLine?: number;
  readOnly?: boolean;
  /** 挂载后聚焦编辑器，默认 true（仅创建时读取一次，中途变化不生效） */
  autoFocus?: boolean;
  /** 每次编辑触发，参数为全文 */
  onDocChange?: (doc: string) => void;
  /** Ctrl/Cmd+S 触发，参数为全文 */
  onSave?: (doc: string) => void;
  /** 编辑器操作句柄，挂载后写入、卸载时置 null */
  apiRef?: React.MutableRefObject<CodeEditorApi | null>;
  className?: string;
}

/** 语法高亮：颜色全部指向 --syn-* 变量，token 分组尽量贴合各语言通感 */
const appHighlight = HighlightStyle.define([
  {
    tag: [tg.keyword, tg.controlKeyword, tg.moduleKeyword, tg.operatorKeyword, tg.definitionKeyword],
    color: 'var(--syn-keyword)',
  },
  { tag: [tg.self, tg.atom, tg.bool, tg.null, tg.special(tg.variableName)], color: 'var(--syn-number)' },
  { tag: tg.number, color: 'var(--syn-number)' },
  { tag: [tg.string, tg.special(tg.string), tg.regexp, tg.escape], color: 'var(--syn-string)' },
  {
    tag: [tg.comment, tg.lineComment, tg.blockComment, tg.docComment],
    color: 'var(--syn-comment)',
    fontStyle: 'italic',
  },
  { tag: [tg.function(tg.variableName), tg.function(tg.propertyName), tg.macroName], color: 'var(--syn-function)' },
  { tag: [tg.typeName, tg.className, tg.namespace, tg.annotation], color: 'var(--syn-type)' },
  { tag: [tg.propertyName, tg.attributeName, tg.labelName], color: 'var(--syn-property)' },
  { tag: tg.tagName, color: 'var(--syn-tag)' },
  { tag: [tg.operator, tg.punctuation, tg.separator, tg.bracket], color: 'var(--syn-operator)' },
  { tag: [tg.meta, tg.processingInstruction], color: 'var(--syn-comment)' },
  { tag: tg.invalid, color: 'var(--color-error)' },
  // Markdown / 文档类
  { tag: tg.heading, color: 'var(--accent)', fontWeight: '600' },
  { tag: [tg.link, tg.url], color: 'var(--syn-property)', textDecoration: 'underline' },
  { tag: tg.emphasis, fontStyle: 'italic' },
  { tag: tg.strong, fontWeight: '600' },
  { tag: tg.strikethrough, textDecoration: 'line-through' },
  { tag: tg.inserted, color: 'var(--color-success)' },
  { tag: tg.deleted, color: 'var(--color-error)' },
]);

/** 编辑器 chrome：底色透明融入弹窗，选区/光标/面板全部走应用色板 */
const appTheme = EditorView.theme({
  '&': {
    height: '100%',
    fontSize: '13px',
    backgroundColor: 'transparent',
    color: 'var(--text-primary)',
  },
  '.cm-scroller': {
    fontFamily: 'var(--app-font-mono)',
    lineHeight: '1.6',
  },
  '&.cm-focused': { outline: 'none' },
  '.cm-content': { caretColor: 'var(--accent)', padding: '8px 0' },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--accent)', borderLeftWidth: '2px' },
  '&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection':
    { backgroundColor: 'var(--accent-muted)' },
  '.cm-activeLine': { backgroundColor: 'var(--border-subtle)' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text-secondary)' },
  '.cm-gutters': {
    backgroundColor: 'transparent',
    color: 'var(--text-muted)',
    borderRight: '1px solid var(--border-subtle)',
  },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 8px 0 16px' },
  '.cm-foldPlaceholder': {
    backgroundColor: 'var(--accent-subtle)',
    border: '1px solid var(--accent-muted)',
    color: 'var(--accent)',
    borderRadius: 'var(--radius-sm)',
    padding: '0 6px',
    margin: '0 2px',
  },
  '&.cm-focused .cm-matchingBracket': { backgroundColor: 'var(--accent-muted)' },
  '&.cm-focused .cm-nonmatchingBracket': { backgroundColor: 'var(--color-error-muted)' },
  '.cm-searchMatch': {
    backgroundColor: 'var(--accent-subtle)',
    outline: '1px solid var(--accent-muted)',
  },
  '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: 'var(--accent-muted)', outline: 'none' },
  '.cm-selectionMatch': { backgroundColor: 'var(--border-default)' },
  '.cm-panels': { backgroundColor: 'var(--bg-elevated)', color: 'var(--text-primary)' },
  '.cm-panels.cm-panels-top': { borderBottom: '1px solid var(--border-default)' },
  '.cm-panel.cm-search': { padding: '6px 10px', fontFamily: 'var(--app-font-family)', fontSize: '12px' },
  '.cm-panel.cm-search label': { color: 'var(--text-secondary)', fontSize: '12px' },
  '.cm-panel.cm-search input[type=checkbox]': { accentColor: 'var(--accent)', marginRight: '3px' },
  '.cm-panel.cm-search [name=close]': {
    color: 'var(--text-muted)',
    fontSize: '18px',
    padding: '0 6px',
    cursor: 'pointer',
  },
  '.cm-textfield': {
    backgroundColor: 'var(--bg-base)',
    border: '1px solid var(--border-default)',
    borderRadius: 'var(--radius-sm)',
    color: 'var(--text-primary)',
    outline: 'none',
  },
  '.cm-textfield:focus': { borderColor: 'var(--accent)' },
  '.cm-button': {
    backgroundColor: 'var(--bg-overlay)',
    backgroundImage: 'none',
    border: '1px solid var(--border-default)',
    borderRadius: 'var(--radius-sm)',
    color: 'var(--text-primary)',
    cursor: 'pointer',
  },
  '.cm-button:active': { backgroundImage: 'none', backgroundColor: 'var(--border-strong)' },
  '.cm-tooltip': {
    backgroundColor: 'var(--bg-overlay)',
    border: '1px solid var(--border-default)',
    color: 'var(--text-primary)',
  },
});

/** CodeMirror 内置 UI（搜索面板等）的中文文案 */
const zhPhrases: Record<string, string> = {
  'Find': '查找',
  'Replace': '替换',
  'next': '下一个',
  'previous': '上一个',
  'all': '全部',
  'match case': '区分大小写',
  'by word': '全词匹配',
  'regexp': '正则',
  'replace': '替换',
  'replace all': '全部替换',
  'close': '关闭',
  'current match': '当前匹配',
  'replaced $ matches': '已替换 $ 处',
  'replaced match on line $': '已替换第 $ 行的匹配',
  'on line': '所在行',
  'Go to line': '跳转到行',
  'go': '跳转',
  'Control character': '控制字符',
};

/** 散文类文件（Markdown / 纯文本）折行，代码不折 */
function shouldWrap(fileName: string) {
  return /\.(md|markdown|mkd|mdx|txt)$/i.test(fileName);
}

export function CodeEditor({
  value,
  fileName,
  highlightLine,
  readOnly = false,
  autoFocus = true,
  onDocChange,
  onSave,
  apiRef,
  className = '',
}: CodeEditorProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const lang = useI18nStore((s) => s.lang);

  // 回调与语言走 ref：它们变化不该触发编辑器重建（重建会丢编辑与撤销栈）。
  // autoFocus 同理——宿主在「预览/源码」切换时会翻转它，进 deps 就等于
  // 每次切换都重建编辑器，恰好丢掉本想保住的草稿
  const onDocChangeRef = useRef(onDocChange);
  onDocChangeRef.current = onDocChange;
  const onSaveRef = useRef(onSave);
  onSaveRef.current = onSave;
  const langRef = useRef(lang);
  langRef.current = lang;
  const autoFocusRef = useRef(autoFocus);
  autoFocusRef.current = autoFocus;

  useEffect(() => {
    const parent = containerRef.current;
    if (!parent) return;

    let disposed = false;
    const langCompartment = new Compartment();

    // CRLF 文件按 CRLF 往返:不设 lineSeparator 时 CM 输入接受任意行尾、
    // toString() 一律用 \n 拼接 —— Windows 上打开 CRLF 文件改一个字保存,
    // git diff 就是整文件行尾变更。设了 facet 后 doc.toString() 原样还原 \r\n
    // (文件内偶发的孤立 \n 会以控制字符可见,恰好暴露混合行尾)
    const crlf = value.includes('\r\n');

    const view = new EditorView({
      state: EditorState.create({
        doc: value,
        extensions: [
          ...(crlf ? [EditorState.lineSeparator.of('\r\n')] : []),
          lineNumbers(),
          highlightActiveLineGutter(),
          highlightSpecialChars(),
          history(),
          foldGutter(),
          drawSelection(),
          dropCursor(),
          EditorState.allowMultipleSelections.of(true),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          rectangularSelection(),
          crosshairCursor(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          search({ top: true }),
          ...(langRef.current === 'zh' ? [EditorState.phrases.of(zhPhrases)] : []),
          keymap.of([
            {
              key: 'Mod-s',
              run: (v) => {
                onSaveRef.current?.(v.state.doc.toString());
                return true;
              },
            },
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...searchKeymap,
            ...historyKeymap,
            ...foldKeymap,
            indentWithTab,
          ]),
          langCompartment.of([]),
          appTheme,
          syntaxHighlighting(appHighlight, { fallback: true }),
          ...(shouldWrap(fileName) ? [EditorView.lineWrapping] : []),
          ...(readOnly ? [EditorState.readOnly.of(true)] : []),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) onDocChangeRef.current?.(u.state.doc.toString());
          }),
        ],
      }),
      parent,
    });
    viewRef.current = view;

    // 语言包按需加载：匹配失败（未知扩展名）就保持纯文本
    const desc = LanguageDescription.matchFilename(languages, fileName);
    if (desc) {
      desc
        .load()
        .then((support) => {
          if (!disposed) view.dispatch({ effects: langCompartment.reconfigure(support) });
        })
        .catch(() => {});
    }

    // 暴露给宿主的操作句柄：Modal 的 Esc 先问这里，「搜索面板开着」时
    // 只关面板不关弹窗（两段式退出）。不走全局键盘监听——编辑器是数据到达后
    // 才挂载的，window capture 注册必然晚于 Modal，抢不到事件
    if (apiRef) {
      apiRef.current = {
        closeSearchIfOpen: () => {
          if (!searchPanelOpen(view.state)) return false;
          closeSearchPanel(view);
          return true;
        },
      };
    }

    let raf = 0;
    if (autoFocusRef.current) raf = requestAnimationFrame(() => view.focus());

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      if (apiRef) apiRef.current = null;
      viewRef.current = null;
      view.destroy();
    };
    // value / fileName 变化 = 换文件或重载，整个编辑器重建
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value, fileName, readOnly]);

  // 行定位与编辑器生命周期解耦：同文件内换行号（搜索结果跳转)不重建编辑器
  useEffect(() => {
    const view = viewRef.current;
    if (!view || !highlightLine) return;
    if (highlightLine > view.state.doc.lines) return;
    const line = view.state.doc.line(highlightLine);
    view.dispatch({
      selection: { anchor: line.from },
      effects: EditorView.scrollIntoView(line.from, { y: 'center' }),
    });
  }, [highlightLine, value, fileName]);

  return <div ref={containerRef} className={`h-full ${className}`} />;
}
