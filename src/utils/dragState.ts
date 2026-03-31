export type DragPayload =
  | { type: 'tab'; tabId: string }
  | { type: 'project'; projectId: string }
  | { type: 'group'; groupId: string };

let _payload: DragPayload | null = null;

export function setDragPayload(p: DragPayload | null) {
  _payload = p;
}

export function getDragPayload() {
  return _payload;
}

// 兼容旧 API（TabBar / TerminalInstance）
export function setDraggingTabId(id: string | null) {
  _payload = id ? { type: 'tab', tabId: id } : null;
}

export function getDraggingTabId() {
  return _payload?.type === 'tab' ? _payload.tabId : null;
}
