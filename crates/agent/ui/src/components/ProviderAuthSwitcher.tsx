import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { Command } from 'cmdk';
import { Copy, ExternalLink, Eye, EyeOff, KeyRound, ShieldCheck, X } from 'lucide-react';
import { useUiStore } from '../store/uiStore';
import { AuthMethod } from '../types';
import type { AuthProviderEntry, OAuthFlowState, OAuthResultState } from '../types';

interface ProviderAuthSwitcherProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providers: AuthProviderEntry[];
  oauthFlow: OAuthFlowState | null;
  oauthResult: OAuthResultState | null;
  apiTokenResult: { provider: string; success: boolean; message: string } | null;
  onRequestProviders: () => void;
  onStartOAuthLogin: (provider: string) => void;
  onCompleteOAuthLogin: (flowId: string, response: string) => void;
  onClearOAuthState: () => void;
  onDisconnectOAuth: (provider: string) => void;
  onSetApiToken: (provider: string, apiKey: string) => void;
  onClearApiToken: (provider: string) => void;
  onSetAuthMethod: (provider: string, method: AuthMethod) => void;
  onClearApiTokenResult: () => void;
}

// ── Exported helpers (tested independently) ──

/** Provider supports only OAuth (no API key env var). */
export function isOAuthOnly(provider: AuthProviderEntry): boolean {
  return provider.supports_oauth && provider.env_var_name == null;
}

/** Provider supports only API key (no OAuth). */
export function isApiKeyOnly(provider: AuthProviderEntry): boolean {
  return !provider.supports_oauth && provider.env_var_name != null;
}

/** Provider supports multiple auth methods (both OAuth and API key). */
export function hasMultipleAuthMethods(provider: AuthProviderEntry): boolean {
  return provider.supports_oauth && provider.env_var_name != null;
}

/** Determine the badge label and styling for a provider's current auth state. */
export function activeAuthLabel(provider: AuthProviderEntry): { label: string; classes: string } {
  const successClasses = 'border-status-success/40 bg-status-success/10 text-status-success';
  const warningClasses = 'border-status-warning/45 bg-status-warning/10 text-status-warning';

  // Resolve the effective auth source: try the preferred method first, then
  // fall back through all available sources in default order.
  const effective = resolveEffectiveAuth(provider);

  if (!effective) {
    return { label: 'Not configured', classes: 'border-surface-border/60 bg-surface-canvas/60 text-ui-muted' };
  }
  if (effective === 'expired') {
    return { label: 'Expired', classes: warningClasses };
  }

  const labelMap: Record<AuthMethod, string> = {
    [AuthMethod.OAuth]: 'OAuth',
    [AuthMethod.ApiKey]: 'API Key',
    [AuthMethod.EnvVar]: 'Env',
  };
  return { label: labelMap[effective], classes: successClasses };
}

/** Check whether a specific auth method is active for a provider. */
function isMethodAvailable(provider: AuthProviderEntry, method: AuthMethod): boolean | 'expired' {
  switch (method) {
    case AuthMethod.OAuth:
      if (provider.oauth_status === 'connected') return true;
      if (provider.oauth_status === 'expired') return 'expired';
      return false;
    case AuthMethod.ApiKey:
      return provider.has_stored_api_key;
    case AuthMethod.EnvVar:
      return provider.has_env_api_key;
  }
}

const DEFAULT_ORDER: AuthMethod[] = [AuthMethod.OAuth, AuthMethod.ApiKey, AuthMethod.EnvVar];

/** Resolve which auth method is effectively active, respecting preference. */
function resolveEffectiveAuth(provider: AuthProviderEntry): AuthMethod | 'expired' | null {
  // Build resolution order: preferred method first (if set), then the rest.
  const pref = provider.preferred_method;
  let order: AuthMethod[];
  if (pref) {
    order = [pref, ...DEFAULT_ORDER.filter(m => m !== pref)];
  } else if (provider.supports_oauth) {
    order = DEFAULT_ORDER;
  } else {
    // No OAuth support — skip it in the default order
    order = DEFAULT_ORDER.filter(m => m !== AuthMethod.OAuth);
  }

  for (const method of order) {
    const available = isMethodAvailable(provider, method);
    if (available === true) return method;
    if (available === 'expired') return 'expired';
  }
  return null;
}

// ── Component ──

export function ProviderAuthSwitcher({
  open,
  onOpenChange,
  providers,
  oauthFlow,
  oauthResult,
  apiTokenResult,
  onRequestProviders,
  onStartOAuthLogin,
  onCompleteOAuthLogin,
  onClearOAuthState,
  onDisconnectOAuth,
  onSetApiToken,
  onClearApiToken,
  onSetAuthMethod,
  onClearApiTokenResult,
}: ProviderAuthSwitcherProps) {
  const [search, setSearch] = useState('');
  const [responseInput, setResponseInput] = useState('');
  const [apiKeyInput, setApiKeyInput] = useState('');
  const [showApiKey, setShowApiKey] = useState(false);
  const [isCompleting, setIsCompleting] = useState(false);
  const [isDisconnecting, setIsDisconnecting] = useState(false);
  const [isSavingApiKey, setIsSavingApiKey] = useState(false);
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copied' | 'error'>('idle');
  const [oauthPending, setOauthPending] = useState(false);
  const [apiKeyPanelProvider, setApiKeyPanelProvider] = useState<AuthProviderEntry | null>(null);
  const [selectedProvider, setSelectedProvider] = useState<AuthProviderEntry | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const callbackRef = useRef<HTMLInputElement>(null);
  const apiKeyInputRef = useRef<HTMLInputElement>(null);
  const openAuthButtonRef = useRef<HTMLButtonElement>(null);
  const disconnectInFlightProviderRef = useRef<string | null>(null);
  const { focusMainInput } = useUiStore();
  const isDevicePoll = oauthFlow?.flow_kind === 'device_poll';

  const close = () => {
    onOpenChange(false);
    focusMainInput();
  };

  // Reset state when opening
  useEffect(() => {
    if (!open) {
      return;
    }
    setSearch('');
    setResponseInput('');
    setApiKeyInput('');
    setShowApiKey(false);
    setIsCompleting(false);
    setIsDisconnecting(false);
    setIsSavingApiKey(false);
    setCopyStatus('idle');
    setOauthPending(false);
    setApiKeyPanelProvider(null);
    setSelectedProvider(null);
    disconnectInFlightProviderRef.current = null;
    onRequestProviders();
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, [open, onRequestProviders]);

  // Focus auth button when OAuth flow starts
  useEffect(() => {
    if (!open || !oauthFlow) {
      return;
    }
    setOauthPending(false);
    setCopyStatus('idle');
    window.setTimeout(() => openAuthButtonRef.current?.focus(), 0);
  }, [open, oauthFlow]);

  // Handle OAuth result
  useEffect(() => {
    if (!oauthResult) {
      return;
    }

    setIsCompleting(false);

    if (
      disconnectInFlightProviderRef.current &&
      oauthResult.provider === disconnectInFlightProviderRef.current
    ) {
      setIsDisconnecting(false);
      disconnectInFlightProviderRef.current = null;
    }
  }, [oauthResult]);

  // Handle API token result
  useEffect(() => {
    if (!apiTokenResult) {
      return;
    }
    setIsSavingApiKey(false);
    if (apiTokenResult.success) {
      setApiKeyInput('');
    }
  }, [apiTokenResult]);

  // Update selected provider when providers list changes
  useEffect(() => {
    if (selectedProvider) {
      const next = providers.find((p) => p.provider === selectedProvider.provider);
      if (next) setSelectedProvider(next);
    }
    if (apiKeyPanelProvider) {
      const next = providers.find((p) => p.provider === apiKeyPanelProvider.provider);
      if (next) setApiKeyPanelProvider(next);
    }
  }, [providers, selectedProvider, apiKeyPanelProvider]);

  useEffect(() => {
    if (copyStatus === 'idle') {
      return;
    }
    const timeoutId = window.setTimeout(() => setCopyStatus('idle'), 2000);
    return () => window.clearTimeout(timeoutId);
  }, [copyStatus]);

  const copyAuthorizationUrl = async () => {
    if (!oauthFlow) {
      return;
    }
    try {
      await navigator.clipboard.writeText(oauthFlow.authorization_url);
      setCopyStatus('copied');
    } catch {
      setCopyStatus('error');
    }
  };

  if (!open) {
    return null;
  }

  const handleProviderSelect = (provider: AuthProviderEntry) => {
    onClearOAuthState();
    onClearApiTokenResult();
    setResponseInput('');
    setApiKeyInput('');
    setShowApiKey(false);
    setIsCompleting(false);
    setIsDisconnecting(false);
    setIsSavingApiKey(false);
    setApiKeyPanelProvider(null);

    if (isOAuthOnly(provider) && provider.oauth_status !== 'connected') {
      // OAuth-only, not connected → skip detail panel, start flow immediately
      setSelectedProvider(provider);
      setOauthPending(true);
      onStartOAuthLogin(provider.provider);
    } else if (isApiKeyOnly(provider)) {
      // API-key-only → show dedicated API key panel directly
      setSelectedProvider(provider);
      setApiKeyPanelProvider(provider);
    } else {
      // Multi-method or connected OAuth-only → show detail panel
      setSelectedProvider(provider);
    }
  };

  const handleDisconnect = () => {
    if (!selectedProvider || isDisconnecting) {
      return;
    }
    setIsDisconnecting(true);
    disconnectInFlightProviderRef.current = selectedProvider.provider;
    onDisconnectOAuth(selectedProvider.provider);
  };

  const handleSaveApiKey = () => {
    const target = apiKeyPanelProvider ?? selectedProvider;
    if (!target || !apiKeyInput.trim() || isSavingApiKey) {
      return;
    }
    setIsSavingApiKey(true);
    onSetApiToken(target.provider, apiKeyInput.trim());
  };

  const handleClearApiKey = () => {
    const target = apiKeyPanelProvider ?? selectedProvider;
    if (!target) {
      return;
    }
    onClearApiToken(target.provider);
  };

  const handleAuthMethodChange = (method: AuthMethod) => {
    if (!selectedProvider) {
      return;
    }
    onSetAuthMethod(selectedProvider.provider, method);

    // When switching to API Key method on a multi-method provider,
    // show the dedicated API key panel
    if (method === AuthMethod.ApiKey) {
      setApiKeyPanelProvider(selectedProvider);
    } else {
      setApiKeyPanelProvider(null);
    }
  };

  const stopCommandActivationPropagation = (e: KeyboardEvent<HTMLElement>) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.stopPropagation();
      return;
    }

    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      window.setTimeout(() => inputRef.current?.focus(), 0);
    }
  };

  const oauthActionFocusClasses =
    'focus-visible:outline-none focus-visible:border-accent-primary/70 focus-visible:ring-2 focus-visible:ring-accent-primary/50 focus-visible:shadow-[0_0_14px_rgba(var(--accent-primary-rgb),0.35)]';
  const disconnectFocusClasses =
    'focus-visible:outline-none focus-visible:border-status-warning/70 focus-visible:ring-2 focus-visible:ring-status-warning/50 focus-visible:shadow-[0_0_14px_rgba(var(--status-warning-rgb),0.35)]';

  // The provider for which the detail panel is shown (multi-method or connected OAuth-only)
  const showDetailPanel =
    selectedProvider &&
    !oauthFlow &&
    !oauthPending &&
    !apiKeyPanelProvider &&
    (hasMultipleAuthMethods(selectedProvider) ||
      (isOAuthOnly(selectedProvider) && selectedProvider.oauth_status === 'connected'));

  // Effective method for the detail panel (multi-method providers only)
  const effectiveMethod = selectedProvider?.preferred_method ?? (
    selectedProvider?.supports_oauth ? AuthMethod.OAuth : AuthMethod.ApiKey
  );

  // The active provider for API key panel (either dedicated or from detail panel switch)
  const apiKeyTarget = apiKeyPanelProvider;

  // Result message to display (from either OAuth or API token operations)
  const resultMessage = (() => {
    const activeProvider = apiKeyTarget ?? selectedProvider;
    if (oauthResult && activeProvider && oauthResult.provider === activeProvider.provider) {
      return oauthResult;
    }
    if (apiTokenResult && activeProvider && apiTokenResult.provider === activeProvider.provider) {
      return apiTokenResult;
    }
    return null;
  })();

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
              placeholder={`Manage provider authentication (${providers.length})...`}
              className="flex-1 bg-transparent text-ui-primary placeholder:text-ui-muted text-sm focus:outline-none"
            />
            <button
              type="button"
              onClick={close}
              className="sm:hidden p-1.5 rounded hover:bg-surface-canvas transition-colors text-ui-secondary hover:text-ui-primary"
              aria-label="Close"
            >
              <X className="w-5 h-5" />
            </button>
            <kbd className="hidden sm:inline-block px-2 py-1 text-[10px] font-mono bg-surface-canvas border border-surface-border rounded text-ui-muted">
              ESC
            </kbd>
          </div>

          <Command.List className="max-h-[360px] overflow-y-auto p-2 custom-scrollbar">
            <Command.Empty className="px-4 py-8 text-center text-sm text-ui-muted">
              No providers found
            </Command.Empty>

            <Command.Group className="mb-1">
              {providers.map((provider) => {
                const badge = activeAuthLabel(provider);
                return (
                  <Command.Item
                    key={provider.provider}
                    value={`${provider.provider} ${provider.display_name}`}
                    keywords={[provider.provider, provider.display_name]}
                    onSelect={() => handleProviderSelect(provider)}
                    className={`flex items-center gap-3 px-3 py-2.5 rounded-lg border cursor-pointer transition-colors data-[selected=true]:bg-accent-primary/15 data-[selected=true]:border-accent-primary/35 hover:bg-surface-elevated/60 hover:border-surface-border/40 ${
                      selectedProvider?.provider === provider.provider
                        ? 'border-accent-primary/35 bg-accent-primary/10'
                        : 'border-surface-border/20'
                    }`}
                  >
                    <div className="w-7 h-7 rounded-md border border-accent-primary/35 bg-accent-primary/10 flex items-center justify-center">
                      <KeyRound className="w-3.5 h-3.5 text-accent-primary" />
                    </div>
                    <div className="flex-1 min-w-0">
                      <div className="text-sm text-ui-primary">{provider.display_name}</div>
                      <div className="text-xs text-ui-muted">{provider.provider}</div>
                    </div>
                    <span
                      className={`inline-flex items-center rounded border px-2 py-0.5 text-[10px] uppercase tracking-wider ${badge.classes}`}
                    >
                      {badge.label}
                    </span>
                  </Command.Item>
                );
              })}
            </Command.Group>
          </Command.List>

          {/* ── Provider Detail Panel (multi-method selector OR connected OAuth-only status) ── */}
          {showDetailPanel && selectedProvider && (
            <div className="border-t border-surface-border/60 bg-surface-canvas/40 px-4 py-3 space-y-3">
              <div className="text-sm text-ui-primary">
                Configure: <span className="text-accent-primary">{selectedProvider.display_name}</span>
              </div>

              {/* Auth method selector — only for multi-method providers */}
              {hasMultipleAuthMethods(selectedProvider) && (
                <div className="flex items-center gap-1.5">
                  <span className="text-xs text-ui-muted mr-1">Method:</span>
                  <button
                    type="button"
                    onKeyDown={stopCommandActivationPropagation}
                    onClick={() => handleAuthMethodChange(AuthMethod.OAuth)}
                    className={`px-2.5 py-1 rounded-md text-xs border transition-all ${oauthActionFocusClasses} ${
                      effectiveMethod === AuthMethod.OAuth
                        ? 'border-accent-primary/60 bg-accent-primary/20 text-accent-primary'
                        : 'border-surface-border/40 bg-surface-elevated/40 text-ui-muted hover:text-ui-secondary hover:border-surface-border/60'
                    }`}
                  >
                    OAuth
                  </button>
                  <button
                    type="button"
                    onKeyDown={stopCommandActivationPropagation}
                    onClick={() => handleAuthMethodChange(AuthMethod.ApiKey)}
                    className={`px-2.5 py-1 rounded-md text-xs border transition-all ${oauthActionFocusClasses} ${
                      effectiveMethod === AuthMethod.ApiKey
                        ? 'border-accent-primary/60 bg-accent-primary/20 text-accent-primary'
                        : 'border-surface-border/40 bg-surface-elevated/40 text-ui-muted hover:text-ui-secondary hover:border-surface-border/60'
                    }`}
                  >
                    API Key
                  </button>
                </div>
              )}

              {/* OAuth section (inline in detail panel for multi-method, or for connected OAuth-only) */}
              {(effectiveMethod === AuthMethod.OAuth || isOAuthOnly(selectedProvider)) &&
                selectedProvider.supports_oauth && (
                  <div className="space-y-2">
                    <div className="flex items-center gap-2 text-xs">
                      <span className="text-ui-muted">Status:</span>
                      {selectedProvider.oauth_status === 'connected' && (
                        <span className="text-status-success">Connected</span>
                      )}
                      {selectedProvider.oauth_status === 'expired' && (
                        <span className="text-status-warning">Expired</span>
                      )}
                      {selectedProvider.oauth_status === 'not_authenticated' && (
                        <span className="text-ui-muted">Not connected</span>
                      )}
                    </div>
                    <div className="flex items-center gap-2">
                      {selectedProvider.oauth_status === 'connected' ? (
                        <button
                          type="button"
                          onKeyDown={stopCommandActivationPropagation}
                          disabled={isDisconnecting}
                          onClick={handleDisconnect}
                          className={`inline-flex items-center gap-2 px-3 py-1.5 rounded-lg text-xs border transition-all ${disconnectFocusClasses} ${
                            isDisconnecting
                              ? 'border-surface-border text-ui-muted cursor-not-allowed bg-surface-elevated/40'
                              : 'border-status-warning/45 bg-status-warning/10 text-status-warning hover:bg-status-warning/20'
                          }`}
                        >
                          {isDisconnecting ? 'Disconnecting...' : 'Disconnect'}
                        </button>
                      ) : (
                        <button
                          type="button"
                          onKeyDown={stopCommandActivationPropagation}
                          onClick={() => {
                            onClearApiTokenResult();
                            onStartOAuthLogin(selectedProvider.provider);
                          }}
                          className={`inline-flex items-center gap-2 px-3 py-1.5 rounded-lg border border-accent-primary/40 text-accent-primary text-xs hover:bg-accent-primary/10 transition-all ${oauthActionFocusClasses}`}
                        >
                          {selectedProvider.oauth_status === 'expired' ? 'Reconnect' : 'Connect'}
                        </button>
                      )}
                    </div>
                  </div>
                )}
            </div>
          )}

          {/* ── Dedicated API Key Panel ── */}
          {apiKeyTarget && !oauthFlow && (
            <div className="border-t border-surface-border/60 bg-surface-canvas/40 px-4 py-3 space-y-3">
              <div className="text-sm text-ui-primary">
                API Key for <span className="text-accent-primary">{apiKeyTarget.display_name}</span>
                {apiKeyTarget.env_var_name && (
                  <code className="ml-2 px-1.5 py-0.5 rounded bg-surface-canvas border border-surface-border text-ui-muted text-[10px] font-mono">
                    {apiKeyTarget.env_var_name}
                  </code>
                )}
              </div>

              {apiKeyTarget.has_stored_api_key && (
                <div className="flex items-center gap-2 text-xs">
                  <span className="text-status-success">API key stored in keychain</span>
                </div>
              )}

              <div className="flex items-center gap-2">
                <div className="relative flex-1">
                  <input
                    ref={apiKeyInputRef}
                    value={apiKeyInput}
                    onChange={(e) => setApiKeyInput(e.target.value)}
                    onKeyDown={(e) => {
                      e.stopPropagation();
                      if (e.key === 'Enter' && apiKeyInput.trim() && !isSavingApiKey) {
                        handleSaveApiKey();
                      }
                    }}
                    type={showApiKey ? 'text' : 'password'}
                    placeholder={apiKeyTarget.has_stored_api_key ? 'Enter new key to update...' : 'Enter API key...'}
                    className="w-full rounded-lg border border-surface-border bg-surface-elevated/70 px-3 py-2 pr-8 text-xs text-ui-primary placeholder:text-ui-muted focus:border-accent-primary focus:outline-none font-mono"
                  />
                  <button
                    type="button"
                    onClick={() => setShowApiKey(!showApiKey)}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-ui-muted hover:text-ui-secondary transition-colors"
                    tabIndex={-1}
                  >
                    {showApiKey ? <EyeOff className="w-3.5 h-3.5" /> : <Eye className="w-3.5 h-3.5" />}
                  </button>
                </div>
                <button
                  type="button"
                  onKeyDown={stopCommandActivationPropagation}
                  disabled={!apiKeyInput.trim() || isSavingApiKey}
                  onClick={handleSaveApiKey}
                  className={`px-3 py-2 rounded-lg text-xs font-medium transition-all ${oauthActionFocusClasses} ${
                    !apiKeyInput.trim() || isSavingApiKey
                      ? 'bg-surface-elevated/50 border border-surface-border text-ui-muted cursor-not-allowed'
                      : 'bg-accent-primary/20 border border-accent-primary text-accent-primary hover:bg-accent-primary/30'
                  }`}
                >
                  {isSavingApiKey ? 'Saving...' : 'Save'}
                </button>
              </div>

              {apiKeyTarget.has_stored_api_key && (
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onKeyDown={stopCommandActivationPropagation}
                    onClick={handleClearApiKey}
                    className={`inline-flex items-center gap-2 px-3 py-1.5 rounded-lg text-xs border transition-all ${disconnectFocusClasses} border-status-warning/45 bg-status-warning/10 text-status-warning hover:bg-status-warning/20`}
                  >
                    Clear Stored Key
                  </button>
                </div>
              )}
            </div>
          )}

          {/* ── OAuth Flow Panel (active flow in progress) ── */}
          {oauthFlow && (
            <div className="border-t border-surface-border/60 bg-surface-canvas/40 px-4 py-3 space-y-3">
              <div className="text-sm text-ui-primary">
                Continue OAuth for <span className="text-accent-primary">{oauthFlow.provider}</span>
              </div>
              <div className="text-xs text-ui-muted">
                {isDevicePoll
                  ? 'Open the device authorization page (URL includes your device code), approve access, then click Check Authentication.'
                  : 'Open the authorization page, approve access, then paste the callback URL or authorization code below.'}
              </div>

              <div className="flex items-center gap-2">
                <button
                  ref={openAuthButtonRef}
                  type="button"
                  onKeyDown={stopCommandActivationPropagation}
                  onClick={() => window.open(oauthFlow.authorization_url, '_blank', 'noopener,noreferrer')}
                  className={`inline-flex items-center gap-2 px-3 py-1.5 rounded-lg border border-accent-primary/40 text-accent-primary text-xs hover:bg-accent-primary/10 transition-all ${oauthActionFocusClasses}`}
                >
                  <ExternalLink className="w-3.5 h-3.5" />
                  Open Authorization Page
                </button>

                <button
                  type="button"
                  onKeyDown={stopCommandActivationPropagation}
                  onClick={() => {
                    void copyAuthorizationUrl();
                  }}
                  className={`inline-flex items-center gap-2 px-3 py-1.5 rounded-lg border border-accent-primary/40 text-accent-primary text-xs hover:bg-accent-primary/10 transition-all ${oauthActionFocusClasses}`}
                >
                  <Copy className="w-3.5 h-3.5" />
                  {copyStatus === 'copied'
                    ? 'Copied!'
                    : copyStatus === 'error'
                      ? 'Copy failed'
                      : isDevicePoll
                        ? 'Copy Device Login URL'
                        : 'Copy Authorization URL'}
                </button>

                {isDevicePoll && (
                  <button
                    type="button"
                    onKeyDown={stopCommandActivationPropagation}
                    disabled={isCompleting}
                    onClick={() => {
                      setIsCompleting(true);
                      onCompleteOAuthLogin(oauthFlow.flow_id, '');
                    }}
                    className={`px-3 py-1.5 rounded-lg text-xs font-medium transition-all ${oauthActionFocusClasses} ${
                      isCompleting
                        ? 'bg-surface-elevated/50 border border-surface-border text-ui-muted cursor-not-allowed'
                        : 'bg-accent-primary/20 border border-accent-primary text-accent-primary hover:bg-accent-primary/30'
                    }`}
                  >
                    {isCompleting ? 'Checking...' : 'Check Authentication'}
                  </button>
                )}
              </div>

              {!isDevicePoll && (
                <div className="flex items-center gap-2">
                  <input
                    ref={callbackRef}
                    value={responseInput}
                    onChange={(e) => setResponseInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') {
                        e.preventDefault();
                        e.stopPropagation();
                      }

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
                    onKeyDown={stopCommandActivationPropagation}
                    disabled={!responseInput.trim() || isCompleting}
                    onClick={() => {
                      setIsCompleting(true);
                      onCompleteOAuthLogin(oauthFlow.flow_id, responseInput.trim());
                    }}
                    className={`px-3 py-2 rounded-lg text-xs font-medium transition-all ${oauthActionFocusClasses} ${
                      !responseInput.trim() || isCompleting
                        ? 'bg-surface-elevated/50 border border-surface-border text-ui-muted cursor-not-allowed'
                        : 'bg-accent-primary/20 border border-accent-primary text-accent-primary hover:bg-accent-primary/30'
                    }`}
                  >
                    {isCompleting ? 'Completing...' : 'Complete'}
                  </button>
                </div>
              )}
            </div>
          )}

          {/* ── Result message ── */}
          {resultMessage && (
            <div
              className={`border-t px-4 py-3 text-xs ${
                resultMessage.success
                  ? 'border-status-success/40 bg-status-success/10 text-status-success'
                  : 'border-status-warning/40 bg-status-warning/10 text-status-warning'
              }`}
            >
              {resultMessage.message}
            </div>
          )}
        </Command>
      </div>
    </>
  );
}
