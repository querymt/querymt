import { useEffect, useRef, useState } from 'react';
import { Command } from 'cmdk';
import { ExternalLink, KeyRound, ShieldCheck } from 'lucide-react';
import { useUiStore } from '../store/uiStore';
import type { AuthProviderEntry, OAuthFlowState, OAuthResultState } from '../types';

interface ProviderAuthSwitcherProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providers: AuthProviderEntry[];
  oauthFlow: OAuthFlowState | null;
  oauthResult: OAuthResultState | null;
  onRequestProviders: () => void;
  onStartOAuthLogin: (provider: string) => void;
  onCompleteOAuthLogin: (flowId: string, response: string) => void;
}

function statusClasses(status: AuthProviderEntry['status']): string {
  if (status === 'connected') {
    return 'border-status-success/40 bg-status-success/10 text-status-success';
  }
  if (status === 'expired') {
    return 'border-status-warning/45 bg-status-warning/10 text-status-warning';
  }
  return 'border-surface-border/60 bg-surface-canvas/60 text-ui-muted';
}

function statusLabel(status: AuthProviderEntry['status']): string {
  if (status === 'connected') {
    return 'Connected';
  }
  if (status === 'expired') {
    return 'Expired';
  }
  return 'Not connected';
}

export function ProviderAuthSwitcher({
  open,
  onOpenChange,
  providers,
  oauthFlow,
  oauthResult,
  onRequestProviders,
  onStartOAuthLogin,
  onCompleteOAuthLogin,
}: ProviderAuthSwitcherProps) {
  const [search, setSearch] = useState('');
  const [responseInput, setResponseInput] = useState('');
  const [isCompleting, setIsCompleting] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const callbackRef = useRef<HTMLInputElement>(null);
  const lastOpenedFlowRef = useRef<string | null>(null);
  const { focusMainInput } = useUiStore();

  const close = () => {
    onOpenChange(false);
    focusMainInput();
  };

  useEffect(() => {
    if (!open) {
      return;
    }
    setSearch('');
    setResponseInput('');
    setIsCompleting(false);
    onRequestProviders();
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, [open, onRequestProviders]);

  useEffect(() => {
    if (!open || !oauthFlow) {
      return;
    }

    if (lastOpenedFlowRef.current !== oauthFlow.flow_id) {
      window.open(oauthFlow.authorization_url, '_blank', 'noopener,noreferrer');
      lastOpenedFlowRef.current = oauthFlow.flow_id;
    }

    window.setTimeout(() => callbackRef.current?.focus(), 0);
  }, [open, oauthFlow]);

  useEffect(() => {
    if (oauthResult) {
      setIsCompleting(false);
    }
  }, [oauthResult]);

  if (!open) {
    return null;
  }

  return (
    <>
      <div
        data-testid="provider-auth-switcher-backdrop"
        className="fixed inset-0 bg-surface-canvas/70 backdrop-blur-sm z-40 animate-fade-in"
        onClick={close}
      />

      <div
        data-testid="provider-auth-switcher-container"
        className="fixed inset-0 z-50 flex items-start justify-center pt-[12vh] px-4"
        onClick={(e) => {
          if (e.target === e.currentTarget) {
            close();
          }
        }}
      >
        <Command
          label="Provider auth switcher"
          className="w-full max-w-2xl bg-surface-elevated border-2 border-accent-primary/30 rounded-xl shadow-[0_0_40px_rgba(var(--accent-primary-rgb),0.25)] overflow-hidden animate-scale-in"
        >
          <div className="flex items-center gap-3 px-4 py-3 border-b border-surface-border/60">
            <ShieldCheck className="w-4 h-4 text-accent-primary" />
            <Command.Input
              ref={inputRef}
              value={search}
              onValueChange={setSearch}
              placeholder={`Authenticate provider (${providers.length})...`}
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          <Command.List className="max-h-[260px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-ui-muted">
              No OAuth providers found
            </Command.Empty>

            <Command.Group className="mb-1">
              {providers.map((provider) => (
                <Command.Item
                  key={provider.provider}
                  value={`${provider.provider} ${provider.display_name}`}
                  keywords={[provider.provider, provider.display_name, provider.status]}
                  onSelect={() => onStartOAuthLogin(provider.provider)}
                  className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-surface-border/20 cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40"
                >
                  <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                    <KeyRound className="w-3.5 h-3.5 text-accent-primary" />
                  </div>
                  <div className="flex-1 min-w-0">
                    <div className="text-sm text-ui-primary">{provider.display_name}</div>
                    <div className="text-xs text-ui-muted">{provider.provider}</div>
                  </div>
                  <span
                    className={`inline-flex items-center rounded border px-2 py-0.5 text-[10px] uppercase tracking-wider ${statusClasses(provider.status)}`}
                  >
                    {statusLabel(provider.status)}
                  </span>
                </Command.Item>
              ))}
            </Command.Group>
          </Command.List>

          {oauthFlow && (
            <div className="border-t border-surface-border/60 bg-surface-canvas/40 px-4 py-3 space-y-3">
              <div className="text-sm text-ui-primary">
                Continue OAuth for <span className="text-accent-primary">{oauthFlow.provider}</span>
              </div>
              <div className="text-xs text-ui-muted">
                A browser tab was opened. Approve access, then paste the callback URL or authorization code below.
              </div>

              <button
                type="button"
                onClick={() => window.open(oauthFlow.authorization_url, '_blank', 'noopener,noreferrer')}
                className="inline-flex items-center gap-2 px-3 py-1.5 rounded-lg border border-accent-primary/40 text-accent-primary text-xs hover:bg-accent-primary/10 transition-colors"
              >
                <ExternalLink className="w-3.5 h-3.5" />
                Open authorization page
              </button>

              <div className="flex items-center gap-2">
                <input
                  ref={callbackRef}
                  value={responseInput}
                  onChange={(e) => setResponseInput(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && responseInput.trim() && !isCompleting) {
                      setIsCompleting(true);
                      onCompleteOAuthLogin(oauthFlow.flow_id, responseInput.trim());
                    }
                  }}
                  placeholder="Paste callback URL or code"
                  className="flex-1 rounded-lg border border-surface-border bg-surface-elevated/70 px-3 py-2 text-xs text-ui-primary placeholder:text-ui-muted focus:border-accent-primary focus:outline-none"
                />
                <button
                  type="button"
                  disabled={!responseInput.trim() || isCompleting}
                  onClick={() => {
                    setIsCompleting(true);
                    onCompleteOAuthLogin(oauthFlow.flow_id, responseInput.trim());
                  }}
                  className={`px-3 py-2 rounded-lg text-xs font-medium transition-colors ${
                    !responseInput.trim() || isCompleting
                      ? 'bg-surface-elevated/50 border border-surface-border text-ui-muted cursor-not-allowed'
                      : 'bg-accent-primary/20 border border-accent-primary text-accent-primary hover:bg-accent-primary/30'
                  }`}
                >
                  {isCompleting ? 'Completing...' : 'Complete'}
                </button>
              </div>
            </div>
          )}

          {oauthResult && (
            <div
              className={`border-t px-4 py-3 text-xs ${
                oauthResult.success
                  ? 'border-status-success/40 bg-status-success/10 text-status-success'
                  : 'border-status-warning/40 bg-status-warning/10 text-status-warning'
              }`}
            >
              {oauthResult.message}
            </div>
          )}
        </Command>
      </div>
    </>
  );
}
