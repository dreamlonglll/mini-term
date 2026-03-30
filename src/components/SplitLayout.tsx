import { useRef } from 'react';
import { Allotment } from 'allotment';
import { TerminalInstance } from './TerminalInstance';
import type { SplitNode } from '../types';

interface Props {
  node: SplitNode;
  onSplit?: (paneId: string, direction: 'horizontal' | 'vertical') => void;
  onClose?: (paneId: string) => void;
  onTabDrop?: (sourceTabId: string, targetPaneId: string, direction: 'horizontal' | 'vertical', position: 'before' | 'after') => void;
  onLayoutChange?: (updatedNode: SplitNode) => void;
}

// 为 SplitNode 生成稳定的 key
function getNodeKey(node: SplitNode): string {
  if (node.type === 'leaf') return node.pane.id;
  return node.children.map(getNodeKey).join('-');
}

export function SplitLayout({ node, onSplit, onClose, onTabDrop, onLayoutChange }: Props) {
  // hooks 必须在条件返回之前调用，保证调用顺序一致
  const rafRef = useRef<number>(0);
  const nodeRef = useRef(node);
  nodeRef.current = node;

  if (node.type === 'leaf') {
    return (
      <TerminalInstance
        ptyId={node.pane.ptyId}
        paneId={node.pane.id}
        shellName={node.pane.shellName}
        status={node.pane.status}
        onSplit={onSplit}
        onClose={onClose}
        onTabDrop={onTabDrop}
      />
    );
  }

  // Allotment onChange 返回像素值，防抖后转比例值更新 store
  const handleSizesChange = (sizes: number[]) => {
    if (!onLayoutChange) return;
    cancelAnimationFrame(rafRef.current);
    rafRef.current = requestAnimationFrame(() => {
      const currentNode = nodeRef.current;
      // 布局已变化（如关闭面板导致子节点数变化），丢弃过期尺寸
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
            node={child}
            onSplit={onSplit}
            onClose={onClose}
            onTabDrop={onTabDrop}
            onLayoutChange={(updated) => handleChildLayoutChange(index, updated)}
          />
        </Allotment.Pane>
      ))}
    </Allotment>
  );
}
