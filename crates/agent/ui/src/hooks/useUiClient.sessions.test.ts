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

function sentListSessions() {
  return MockWebSocket.instance?.sentMessages.filter((message) => message.type === 'list_sessions') ?? [];
}

function sentScheduleRequests() {
  return MockWebSocket.instance?.sentMessages.filter((message) => message.type === 'list_schedules') ?? [];
}

describe('useUiClient - session listing', () => {
  beforeEach(() => {
    MockWebSocket.instance = null;
    (globalThis as any).WebSocket = MockWebSocket;
    Object.defineProperty(window, 'location', {
      value: { protocol: 'http:', host: 'localhost:3000' },
      writable: true,
    });
  });

  afterEach(() => {
    MockWebSocket.instance?.close();
    MockWebSocket.instance = null;
    (globalThis as any).WebSocket = OriginalWebSocket;
  });

  it('requests remote sessions in the initial browse list', async () => {
    await renderConnectedHook();

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'browse', include_remote: true });
  });

  it('keeps generic browse pagination local-only by default', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_list',
        data: {
          groups: [],
          next_cursor: 'next-page',
          total_count: 25,
        },
      });
    });

    await act(async () => {
      result.current.loadMoreSessions();
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({
      mode: 'browse',
      cursor: 'next-page',
    });
    expect(request?.data.include_remote).toBeUndefined();
  });

  it('requests remote sessions when browse pagination opts in', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_list',
        data: {
          groups: [],
          next_cursor: 'next-page',
          total_count: 25,
        },
      });
    });

    await act(async () => {
      result.current.loadMoreSessions(50, { includeRemote: true });
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({
      mode: 'browse',
      cursor: 'next-page',
      include_remote: true,
    });
  });

  it('keeps generic search local-only by default', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.searchSessions('foo');
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'search', query: 'foo' });
    expect(request?.data.include_remote).toBeUndefined();
  });

  it('requests remote sessions when search opts in', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.searchSessions('foo', 30, { includeRemote: true });
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'search', query: 'foo', include_remote: true });
  });

  it('keeps clearing generic search local-only by default', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.searchSessions('');
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'browse' });
    expect(request?.data.include_remote).toBeUndefined();
  });

  it('requests remote sessions when clearing opted-in search reloads browse results', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.searchSessions('', 30, { includeRemote: true });
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'browse', include_remote: true });
  });

  it('keeps grouped pagination local-only', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_list',
        data: {
          groups: [
            {
              cwd: '/workspace/project',
              sessions: [],
              next_cursor: 'group-next',
            },
          ],
          next_cursor: null,
          total_count: 0,
        },
      });
    });

    await act(async () => {
      result.current.loadMoreGroupSessions('/workspace/project');
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'group', cwd: '/workspace/project' });
    expect(request?.data.include_remote).toBeUndefined();
  });

  it('requests remote sessions after delete error recovery', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.deleteSession('s1', 'Session One');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'error',
        data: { message: 'Failed to delete session: denied' },
      });
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'browse', include_remote: true });
  });

  it('requests remote sessions after load error recovery', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      result.current.loadSession('s1', 'Session One');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'error',
        data: { message: 'Failed to load session: missing' },
      });
    });

    const request = sentListSessions().at(-1);
    expect(request?.data).toMatchObject({ mode: 'browse', include_remote: true });
  });

  it('uses loaded remote node ids when listing schedules after session load', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        data: {
          session_id: 'remote-session',
          agent_id: 'primary',
          node_id: 'stable-remote-node',
          audit: { session_id: 'remote-session', events: [], tasks: [], intent_snapshots: [], decisions: [], progress_entries: [], artifacts: [], delegations: [], generated_at: '2026-01-01T00:00:00Z' },
          undo_stack: [],
          cursor: { local_seq: 0, remote_seq_by_source: {} },
        },
      });
    });

    await act(async () => {
      result.current.listSchedules('remote-session');
    });

    const request = sentScheduleRequests().at(-1);
    expect(request?.data).toMatchObject({ session_id: 'remote-session', node_id: 'stable-remote-node' });
  });

  it('stores schedule lists by response key without dropping remote refresh results', async () => {
    const { result } = await renderConnectedHook();

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'session_loaded',
        data: {
          session_id: 'remote-session',
          agent_id: 'primary',
          node_id: 'stable-remote-node',
          audit: { session_id: 'remote-session', events: [], tasks: [], intent_snapshots: [], decisions: [], progress_entries: [], artifacts: [], delegations: [], generated_at: '2026-01-01T00:00:00Z' },
          undo_stack: [],
          cursor: { local_seq: 0, remote_seq_by_source: {} },
        },
      });
    });

    await act(async () => {
      result.current.listSchedules('remote-session');
    });

    await act(async () => {
      MockWebSocket.instance?.simulateMessage({
        type: 'schedule_list',
        data: {
          session_id: 'remote-session',
          node_id: undefined,
          schedules: [],
        },
      });
      MockWebSocket.instance?.simulateMessage({
        type: 'schedule_list',
        data: {
          session_id: 'remote-session',
          node_id: 'stable-remote-node',
          schedules: [
            {
              public_id: 'sched-1',
              task_public_id: 'task-1',
              session_public_id: 'remote-session',
              node_id: 'stable-remote-node',
              trigger: { Interval: { every_seconds: 60 } },
              state: 'armed',
              run_count: 1,
              consecutive_failures: 0,
              created_at: '2026-01-01T00:00:00Z',
              updated_at: '2026-01-01T00:00:00Z',
            },
          ],
        },
      });
    });

    expect(result.current.schedules).toHaveLength(1);
    expect(result.current.schedules[0]?.public_id).toBe('sched-1');
  });
});
