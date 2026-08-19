import { useRef } from 'react';
import { Allotment } from 'allotment';
import { PaneGroup } from './PaneGroup';
import type { SplitNode } from '../types';

interface Props {
  projectId: string;
  node: SplitNode;
  projectPath: string;
  /** 分隔条拖动导致的 sizes 变化回写；pane 的增删由 paneActions 直接改全局布局 */
  onLayoutChange?: (updatedNode: SplitNode) => void;
}

// 稳定 key：递归取第一个叶节点里最小的 pane ID。
// 当 leaf 被 insertSplit 变为 split 时，原 leaf 始终是 children[0]，
// key 不变 → 父级 Allotment 不会重新分配尺寸。
// 取最小值而不是 panes[0]：tab 拖拽重排（尤其插到首位）会换掉 panes[0]，
// 用它当 key 会让整个 PaneGroup 卸载重挂，pane-enter 动画整屏重播像新开了分屏。
function getNodeKey(node: SplitNode): string {
  if (node.type === 'leaf') {
    return node.panes.reduce<string | null>(
      (min, p) => (min === null || p.id < min ? p.id : min),
      null,
    ) ?? 'empty';
  }
  return getNodeKey(node.children[0]);
}

export function SplitLayout({ projectId, node, projectPath, onLayoutChange }: Props) {
  const rafRef = useRef<number>(0);
  const nodeRef = useRef(node);
  nodeRef.current = node;

  if (node.type === 'leaf') {
    return <PaneGroup projectId={projectId} node={node} projectPath={projectPath} />;
  }

  const handleSizesChange = (sizes: number[]) => {
    if (!onLayoutChange) return;
    cancelAnimationFrame(rafRef.current);
    rafRef.current = requestAnimationFrame(() => {
      const currentNode = nodeRef.current;
      if (currentNode.type !== 'split' || sizes.length !== currentNode.children.length) return;
      const total = sizes.reduce((a, b) => a + b, 0);
      const proportional = total > 0 ? sizes.map((s) => (s / total) * 100) : sizes;
      onLayoutChange({ ...currentNode, sizes: proportional });
    });
  };

  const handleChildLayoutChange = (index: number, updatedChild: SplitNode) => {
    if (!onLayoutChange) return;
    const currentNode = nodeRef.current;
    if (currentNode.type !== 'split') return;
    const newChildren = [...currentNode.children];
    newChildren[index] = updatedChild;
    onLayoutChange({ ...currentNode, children: newChildren });
  };

  return (
    <Allotment
      vertical={node.direction === 'vertical'}
      defaultSizes={node.sizes}
      onChange={handleSizesChange}
    >
      {node.children.map((child, index) => (
        <Allotment.Pane key={getNodeKey(child)}>
          <SplitLayout
            projectId={projectId}
            node={child}
            projectPath={projectPath}
            onLayoutChange={(updated) => handleChildLayoutChange(index, updated)}
          />
        </Allotment.Pane>
      ))}
    </Allotment>
  );
}
