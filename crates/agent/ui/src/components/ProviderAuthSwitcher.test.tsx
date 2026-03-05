/**
 * Unit tests for ProviderAuthSwitcher helper logic.
 *
 * NOTE: Component render tests are not viable in this environment because bun
 * lacks jsdom — all @testing-library/react tests fail with "document is not
 * defined" (253 pre-existing failures across the codebase).  These tests cover
 * the exported logic helpers instead.
 */

import { describe, it, expect } from 'vitest';
import {
  activeAuthLabel,
  isOAuthOnly,
  isApiKeyOnly,
  hasMultipleAuthMethods,
} from './ProviderAuthSwitcher';
import { AuthMethod } from '../types';
import type { AuthProviderEntry } from '../types';

// ── Fixtures ──

const oauthOnly: AuthProviderEntry = {
  provider: 'codex',
  display_name: 'Codex',
  oauth_status: 'not_authenticated',
  has_stored_api_key: false,
  has_env_api_key: false,
  env_var_name: null,
  supports_oauth: true,
  preferred_method: null,
};

const oauthOnlyConnected: AuthProviderEntry = {
  ...oauthOnly,
  oauth_status: 'connected',
};

const apiKeyOnly: AuthProviderEntry = {
  provider: 'groq',
  display_name: 'Groq',
  oauth_status: null,
  has_stored_api_key: false,
  has_env_api_key: false,
  env_var_name: 'GROQ_API_KEY',
  supports_oauth: false,
  preferred_method: null,
};

const apiKeyOnlyWithEnv: AuthProviderEntry = {
  ...apiKeyOnly,
  has_env_api_key: true,
};

const apiKeyOnlyWithStoredKey: AuthProviderEntry = {
  ...apiKeyOnly,
  has_stored_api_key: true,
};

const multiMethod: AuthProviderEntry = {
  provider: 'openai',
  display_name: 'OpenAI',
  oauth_status: 'not_authenticated',
  has_stored_api_key: false,
  has_env_api_key: false,
  env_var_name: 'OPENAI_API_KEY',
  supports_oauth: true,
  preferred_method: null,
};

const multiMethodWithOAuth: AuthProviderEntry = {
  ...multiMethod,
  oauth_status: 'connected',
};

const multiMethodWithApiKey: AuthProviderEntry = {
  ...multiMethod,
  preferred_method: AuthMethod.ApiKey,
  has_stored_api_key: true,
};

// ── Provider type classification ──

describe('isOAuthOnly', () => {
  it('returns true for OAuth-only providers (no env_var_name)', () => {
    expect(isOAuthOnly(oauthOnly)).toBe(true);
  });

  it('returns false for multi-method providers', () => {
    expect(isOAuthOnly(multiMethod)).toBe(false);
  });

  it('returns false for API-key-only providers', () => {
    expect(isOAuthOnly(apiKeyOnly)).toBe(false);
  });
});

describe('isApiKeyOnly', () => {
  it('returns true for API-key-only providers (no OAuth support)', () => {
    expect(isApiKeyOnly(apiKeyOnly)).toBe(true);
  });

  it('returns false for multi-method providers', () => {
    expect(isApiKeyOnly(multiMethod)).toBe(false);
  });

  it('returns false for OAuth-only providers', () => {
    expect(isApiKeyOnly(oauthOnly)).toBe(false);
  });
});

describe('hasMultipleAuthMethods', () => {
  it('returns true for providers with both OAuth and API key', () => {
    expect(hasMultipleAuthMethods(multiMethod)).toBe(true);
  });

  it('returns false for OAuth-only providers', () => {
    expect(hasMultipleAuthMethods(oauthOnly)).toBe(false);
  });

  it('returns false for API-key-only providers', () => {
    expect(hasMultipleAuthMethods(apiKeyOnly)).toBe(false);
  });
});

// ── Badge label logic ──

describe('activeAuthLabel', () => {
  // OAuth badges
  it('shows OAuth badge for connected OAuth-only provider', () => {
    const result = activeAuthLabel(oauthOnlyConnected);
    expect(result.label).toBe('OAuth');
    expect(result.classes).toContain('status-success');
  });

  it('shows Expired badge for expired OAuth-only provider', () => {
    const provider = { ...oauthOnly, oauth_status: 'expired' as const };
    const result = activeAuthLabel(provider);
    expect(result.label).toBe('Expired');
    expect(result.classes).toContain('status-warning');
  });

  it('shows Not configured for unauthenticated OAuth-only provider', () => {
    const result = activeAuthLabel(oauthOnly);
    expect(result.label).toBe('Not configured');
  });

  // API key badges
  it('shows API Key badge for provider with stored key', () => {
    const result = activeAuthLabel(apiKeyOnlyWithStoredKey);
    expect(result.label).toBe('API Key');
    expect(result.classes).toContain('status-success');
  });

  // Env var badges — env var still shows in badge even though not a selectable method
  it('shows Env badge for provider using env var', () => {
    const result = activeAuthLabel(apiKeyOnlyWithEnv);
    expect(result.label).toBe('Env');
    expect(result.classes).toContain('status-success');
  });

  // Multi-method with preference
  it('shows OAuth badge for multi-method provider with OAuth preference and connected', () => {
    const result = activeAuthLabel(multiMethodWithOAuth);
    expect(result.label).toBe('OAuth');
  });

  it('shows API Key badge for multi-method provider with ApiKey preference and stored key', () => {
    const result = activeAuthLabel(multiMethodWithApiKey);
    expect(result.label).toBe('API Key');
  });

  // EnvVar preference is no longer settable from UI, but should still display
  // correctly if the backend returns it from a previous session
  it('shows Env badge when preferred_method is EnvVar and env is set (legacy)', () => {
    const provider = {
      ...apiKeyOnly,
      preferred_method: AuthMethod.EnvVar,
      has_env_api_key: true,
    };
    const result = activeAuthLabel(provider);
    expect(result.label).toBe('Env');
  });

  it('shows Not configured for provider with no auth at all', () => {
    const result = activeAuthLabel(apiKeyOnly);
    expect(result.label).toBe('Not configured');
  });
});
