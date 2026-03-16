import { beforeEach, describe, expect, it, vi } from 'vitest';

const renderSpy = vi.fn();
const createRootSpy = vi.fn(() => ({ render: renderSpy }));
const webComponentsImportSpy = vi.fn();

vi.mock('react-dom/client', () => ({
  default: {
    createRoot: createRootSpy,
  },
  createRoot: createRootSpy,
}));

vi.mock('./App', () => ({
  default: () => null,
}));

vi.mock('./context/UiClientContext', () => ({
  UiClientProvider: ({ children }: { children: React.ReactNode }) => children,
}));

describe('main bootstrap', () => {
  beforeEach(() => {
    vi.resetModules();
    vi.doUnmock('@pierre/diffs');
    renderSpy.mockClear();
    createRootSpy.mockClear();
    webComponentsImportSpy.mockClear();
    document.body.innerHTML = '<div id="root"></div>';
  });

  it('imports pierre diffs registration module', async () => {
    vi.doMock('@pierre/diffs', () => {
      webComponentsImportSpy();
      return {};
    });

    await import('./main');

    expect(webComponentsImportSpy).toHaveBeenCalledTimes(1);
    expect(createRootSpy).toHaveBeenCalledTimes(1);
    expect(renderSpy).toHaveBeenCalledTimes(1);
  });

  it('registers the diffs-container custom element with the real package', async () => {
    await import('./main');

    expect(customElements.get('diffs-container')).toBeDefined();
    const element = document.createElement('diffs-container');
    document.body.appendChild(element);
    expect(element.shadowRoot).not.toBeNull();
    expect(createRootSpy).toHaveBeenCalledTimes(1);
    expect(renderSpy).toHaveBeenCalledTimes(1);
  });
});
