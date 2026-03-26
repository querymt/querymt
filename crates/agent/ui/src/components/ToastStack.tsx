/**
 * ToastStack - Renders session action notices and connection error toasts.
 * Extracted from AppShell to reduce its size.
 */

interface NoticeItem {
  id: number;
  kind: 'success' | 'error';
  message: string;
}

interface ErrorItem {
  id: number;
  message: string;
}

interface ToastStackProps {
  sessionActionNotices: NoticeItem[];
  connectionErrors: ErrorItem[];
  onDismissNotice: (id: number) => void;
  onDismissError: (id: number) => void;
}

export function ToastStack({
  sessionActionNotices,
  connectionErrors,
  onDismissNotice,
  onDismissError,
}: ToastStackProps) {
  if (sessionActionNotices.length === 0 && connectionErrors.length === 0) {
    return null;
  }

  return (
    <div className="fixed bottom-4 right-4 z-50 flex flex-col gap-2 max-w-md">
      {sessionActionNotices.map((notice) => (
        <div
          key={notice.id}
          className={`flex items-start gap-2 px-4 py-3 rounded-lg border bg-surface-elevated shadow-lg animate-fade-in ${
            notice.kind === 'success'
              ? 'border-status-success/40'
              : 'border-status-warning/40'
          }`}
        >
          <span
            className={`text-xs flex-1 break-words ${
              notice.kind === 'success' ? 'text-status-success' : 'text-status-warning'
            }`}
          >
            {notice.message}
          </span>
          <button
            type="button"
            onClick={() => onDismissNotice(notice.id)}
            className="text-ui-muted hover:text-ui-primary transition-colors flex-shrink-0"
            aria-label="Dismiss"
          >
            &times;
          </button>
        </div>
      ))}
      {connectionErrors.map((err) => (
        <div
          key={err.id}
          className="flex items-start gap-2 px-4 py-3 rounded-lg border border-status-warning/40 bg-surface-elevated shadow-lg animate-fade-in"
        >
          <span className="text-xs text-status-warning flex-1 break-words">{err.message}</span>
          <button
            type="button"
            onClick={() => onDismissError(err.id)}
            className="text-ui-muted hover:text-ui-primary transition-colors flex-shrink-0"
            aria-label="Dismiss"
          >
            &times;
          </button>
        </div>
      ))}
    </div>
  );
}
