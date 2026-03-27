import { Allotment } from 'allotment';
import { TerminalInstance } from './TerminalInstance';
import type { SplitNode } from '../types';

interface Props {
  node: SplitNode;
  onSplit?: (paneId: string, direction: 'horizontal' | 'vertical') => void;
}

// 为 SplitNode 生成稳定的 key
function getNodeKey(node: SplitNode): string {
  if (node.type === 'leaf') return node.pane.id;
  return node.children.map(getNodeKey).join('-');
}

export function SplitLayout({ node, onSplit }: Props) {
  if (node.type === 'leaf') {
    return (
      <TerminalInstance
        ptyId={node.pane.ptyId}
        paneId={node.pane.id}
        onSplit={onSplit}
      />
    );
  }

  return (
    <Allotment
      vertical={node.direction === 'vertical'}
      defaultSizes={node.sizes}
    >
      {node.children.map((child) => (
        <Allotment.Pane key={getNodeKey(child)}>
          <SplitLayout node={child} onSplit={onSplit} />
        </Allotment.Pane>
      ))}
    </Allotment>
  );
}
