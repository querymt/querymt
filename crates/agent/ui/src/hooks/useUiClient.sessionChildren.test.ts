import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useUiClient } from './useUiClient';

class MockWebSocket {
  static instance: MockWebSocket | null = null;
  sentMessages: any[] = [];

  onopen: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;

  readyState = WebSocket.OPEN;

  constructor(public url: string) {
    MockWebSocket.instance = this;
    Promise.resolve().then(() => {
      this.onopen?.(new Event('open'));
    });
  }

  send(data: string) {
    this.sentMessages.push(JSON.parse(data));
  }

  close() {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.(new CloseEvent('close'));
  }

  simulateMessage(data: any) {
    this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(data) }));
  }
}

const OriginalWebSocket = globalThis.WebSocket;

async function renderConnectedHook() {
  const hook = renderHook(() => useUiClient());

  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });

  expect(hook.result.current.connected).toBe(true);
  return hook;
}

describe('useUiClient - session children loading', () => {
  beforeEach(() => {
    MockWebSocket.instance = null;
    (globalThis as any).WebSocket = MockWebSocket;
    Object.defineProperty(window, 'location', {
      value: { protocol: 'http:', host: 'localhost:3000' },
      writable: true,
    });
  });

  afterEach(() => {
    MockWebSocket.instance = null;
    (globalThis as any).WebSocket = OriginalWebSocket;
  });

  it('clears loading state when listing session children fails', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.loadSessionChildren('parent-1');
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(true);

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'error',
        data: { message: 'Failed to list session children: boom' },
      });
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(false);
  });

  it('clears loading state when listing session children uses an invalid scope', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.loadSessionChildren('parent-1');
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(true);

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'error',
        data: { message: 'Session children list only supports user forks' },
      });
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(false);
  });

  it('paginates session children and appends subsequent pages', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_list',
        data: {
          groups: [
            {
              cwd: '/workspace/test',
              sessions: [
                {
                  session_id: 'parent-1',
                  title: 'Parent',
                  has_children: true,
                  fork_count: 11,
                },
              ],
            },
          ],
          next_cursor: null,
          total_count: 1,
        },
      });
    });

    await act(async () => {
      result.current.loadSessionChildren('parent-1');
    });

    const firstRequest = MockWebSocket.instance?.sentMessages.findLast(
      (message) => message.type === 'list_session_children'
    );
    expect(firstRequest.data).toMatchObject({ parent_session_id: 'parent-1', limit: 10 });
    expect(firstRequest.data.cursor).toBeUndefined();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_children',
        data: {
          parent_session_id: 'parent-1',
          sessions: [
            {
              session_id: 'child-1',
              title: 'First child',
              parent_session_id: 'parent-1',
              has_children: false,
              fork_count: 0,
            },
          ],
          next_cursor: '10',
          total_count: 11,
        },
      });
    });

    let parent = result.current.sessionGroups[0].sessions[0] as any;
    expect(parent.children.map((child: any) => child.session_id)).toEqual(['child-1']);
    expect(parent.childrenNextCursor).toBe('10');
    expect(parent.childrenTotalCount).toBe(11);

    await act(async () => {
      result.current.loadSessionChildren('parent-1', '10');
    });

    const secondRequest = MockWebSocket.instance?.sentMessages.findLast(
      (message) => message.type === 'list_session_children'
    );
    expect(secondRequest.data).toMatchObject({ parent_session_id: 'parent-1', limit: 10, cursor: '10' });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_children',
        data: {
          parent_session_id: 'parent-1',
          sessions: [
            {
              session_id: 'child-2',
              title: 'Second child',
              parent_session_id: 'parent-1',
              has_children: false,
              fork_count: 0,
            },
          ],
          next_cursor: null,
          total_count: 11,
        },
      });
    });

    parent = result.current.sessionGroups[0].sessions[0] as any;
    expect(parent.children.map((child: any) => child.session_id)).toEqual(['child-1', 'child-2']);
    expect(parent.childrenNextCursor).toBeNull();
    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(false);
  });

  it('clears only the successful parent from pending child loads', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.loadSessionChildren('parent-1');
      result.current.loadSessionChildren('parent-2');
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(true);
    expect(result.current.sessionChildrenLoading.has('parent-2')).toBe(true);

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_children',
        data: {
          parent_session_id: 'parent-1',
          sessions: [],
          total_count: 0,
        },
      });
    });

    expect(result.current.sessionChildrenLoading.has('parent-1')).toBe(false);
    expect(result.current.sessionChildrenLoading.has('parent-2')).toBe(true);
  });
});
