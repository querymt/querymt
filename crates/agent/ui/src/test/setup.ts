import '@testing-library/jest-dom';

// JSDOM does not implement constructable stylesheets, but @pierre/diffs uses them.
if (!(globalThis as any).CSSStyleSheet) {
  class CSSStyleSheetMock {
    replaceSync(_css: string) {}
  }
  (globalThis as any).CSSStyleSheet = CSSStyleSheetMock;
}

if (!(ShadowRoot.prototype as any).adoptedStyleSheets) {
  Object.defineProperty(ShadowRoot.prototype, 'adoptedStyleSheets', {
    configurable: true,
    get() {
      return [];
    },
    set(_value) {},
  });
}

// Mock localStorage for tests
class LocalStorageMock implements Storage {
  private store: Record<string, string> = {};

  get length(): number {
    return Object.keys(this.store).length;
  }

  key(index: number): string | null {
    const keys = Object.keys(this.store);
    return keys[index] ?? null;
  }

  getItem(key: string): string | null {
    return this.store[key] ?? null;
  }

  setItem(key: string, value: string): void {
    this.store[key] = String(value);
  }

  removeItem(key: string): void {
    delete this.store[key];
  }

  clear(): void {
    this.store = {};
  }
}

(globalThis as any).localStorage = new LocalStorageMock();

// Mock ResizeObserver for cmdk (used in SessionSwitcher)
class ResizeObserverMock {
  observe() {}
  unobserve() {}
  disconnect() {}
}

(globalThis as any).ResizeObserver = ResizeObserverMock;

// Mock scrollIntoView for cmdk (used in SessionSwitcher)
Element.prototype.scrollIntoView = () => {};
