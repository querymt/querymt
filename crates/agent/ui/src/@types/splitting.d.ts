declare module 'splitting' {
  interface SplittingOptions {
    target?: HTMLElement | string;
    by?: string;
  }
  
  function Splitting(options?: SplittingOptions): void;
  
  export default Splitting;
}

declare global {
  interface Window {
    Splitting: (options?: { target?: HTMLElement; by?: string }) => void;
  }
}

export {};
