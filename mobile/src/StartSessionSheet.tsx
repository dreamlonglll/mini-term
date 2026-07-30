import { Fragment, useMemo, useState } from 'react';
import { startAiSession, useRelayStore } from './relay';
import { useT } from './i18n';
import type { MobileProject } from './protocol';

/**
 * 发起新 AI 会话的弹层:选项目 → 选启动器。
 *
 * 只有一条启动器时跳过第二步(少点一步)。SSH 远程项目与 WSL 根项目置灰并标注
 * "对话镜像不可用"——在那里开会话只能盲发指令、看不到回复。
 * 手机侧永远不自拟命令,传的只是启动器 id。
 *
 * 项目按桌面端的「分组」层级展示:快照里的项目已经是桌面端项目树的深度优先序,
 * 每项带 `groupPath`(祖先分组名链),据此重建成可折叠的树。折叠状态是**手机本地**的
 * (默认全展开)——桌面端折叠了什么与这里无关,小屏上跟着折叠只会让人找不到项目。
 */

interface GroupNode {
  kind: 'group';
  /** 全路径 key:同名但不同父的组不会被并到一起 */
  key: string;
  name: string;
  children: SheetNode[];
}

type SheetNode = GroupNode | { kind: 'project'; project: MobileProject };

/**
 * 把带 groupPath 的扁平清单重建成分组树。
 *
 * 组按整条路径索引复用,**不**要求同组项目在输入里连续:增量是原位替换的,项目在桌面端
 * 换组后位置不动、只有 groupPath 变,照样能落进正确的组(组自身的排位则要等下次全量快照
 * 才完全对齐桌面端)。没有任何项目的空组不会出现——选项目时它也没有意义。
 * groupPath 缺省(旧桌面端 / 旧中转把字段吃掉)时整棵树退化成平铺列表。
 */
function buildTree(projects: MobileProject[]): SheetNode[] {
  const roots: SheetNode[] = [];
  const groups = new Map<string, GroupNode>();

  for (const project of projects) {
    let siblings = roots;
    const path: string[] = [];
    for (const name of project.groupPath ?? []) {
      path.push(name);
      // 整条路径序列化当 key:没有分隔符歧义,同名但不同父的组不会被并到一起
      const key = JSON.stringify(path);
      let group = groups.get(key);
      if (!group) {
        group = { kind: 'group', key, name, children: [] };
        groups.set(key, group);
        siblings.push(group);
      }
      siblings = group.children;
    }
    siblings.push({ kind: 'project', project });
  }
  return roots;
}

/** 组内项目总数(含子组),对齐桌面端组名后面的计数 */
function countProjects(group: GroupNode): number {
  let total = 0;
  for (const child of group.children) {
    total += child.kind === 'group' ? countProjects(child) : 1;
  }
  return total;
}

export function StartSessionSheet({ onClose }: { onClose: () => void }) {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const launchers = useRelayStore((s) => s.launchers);
  const [picked, setPicked] = useState<MobileProject | null>(null);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set<string>());

  const tree = useMemo(() => buildTree(projects), [projects]);

  const launch = (project: MobileProject, launcherId: string) => {
    if (startAiSession(project.projectId, project.name, launcherId)) onClose();
  };

  const pickProject = (project: MobileProject) => {
    if (launchers.length === 1) {
      launch(project, launchers[0].id);
      return;
    }
    setPicked(project);
  };

  const toggleGroup = (key: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  };

  const renderNode = (node: SheetNode, depth: number) => {
    const indent = { paddingLeft: `${16 + depth * 16}px` };

    if (node.kind === 'project') {
      const { project } = node;
      return (
        <button
          key={project.projectId}
          className="sheet-row"
          style={indent}
          disabled={!project.canStartSession}
          onClick={() => pickProject(project)}
        >
          <span className="sheet-row-name">{project.name}</span>
          {project.canStartSession ? (
            <span className="pane-chevron">›</span>
          ) : (
            <span className="sheet-row-note">{t('start.notSupported')}</span>
          )}
        </button>
      );
    }

    const isCollapsed = collapsed.has(node.key);
    return (
      <Fragment key={node.key}>
        <button
          className="sheet-group"
          style={indent}
          aria-expanded={!isCollapsed}
          onClick={() => toggleGroup(node.key)}
        >
          <span className={`sheet-group-arrow ${isCollapsed ? 'is-collapsed' : ''}`}>▾</span>
          <span className="sheet-group-name">{node.name}</span>
          <span className="sheet-group-count">({countProjects(node)})</span>
        </button>
        {!isCollapsed && node.children.map((child) => renderNode(child, depth + 1))}
      </Fragment>
    );
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="sheet-header">
          {picked ? (
            <button className="sheet-back" onClick={() => setPicked(null)}>
              ‹ {t('start.back')}
            </button>
          ) : (
            <span className="sheet-title">{t('start.pickProject')}</span>
          )}
          <button className="sheet-close" onClick={onClose}>
            {t('start.cancel')}
          </button>
        </div>

        {picked ? (
          <div className="sheet-body">
            <div className="sheet-subtitle">
              {picked.name} · {t('start.pickLauncher')}
            </div>
            {launchers.map((launcher) => (
              <button
                key={launcher.id}
                className="sheet-row"
                onClick={() => launch(picked, launcher.id)}
              >
                <span className="sheet-row-name">{launcher.name}</span>
                <span className="pane-chevron">›</span>
              </button>
            ))}
          </div>
        ) : (
          <div className="sheet-body">
            {projects.length === 0 ? (
              <div className="sheet-empty">{t('start.noProjects')}</div>
            ) : (
              tree.map((node) => renderNode(node, 0))
            )}
          </div>
        )}
      </div>
    </div>
  );
}
