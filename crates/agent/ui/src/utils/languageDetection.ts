/**
 * Language detection utilities for syntax highlighting
 */

export interface LanguageInfo {
  language: string;
  isMarkdown: boolean;
  displayName: string;
}

const LANGUAGE_MAP: Record<string, string> = {
  // JavaScript/TypeScript
  ts: 'typescript',
  tsx: 'tsx',
  js: 'javascript',
  jsx: 'jsx',
  mjs: 'javascript',
  cjs: 'javascript',
  
  // Python
  py: 'python',
  pyw: 'python',
  pyi: 'python',
  
  // Rust
  rs: 'rust',
  
  // Go
  go: 'go',
  
  // Ruby
  rb: 'ruby',
  rake: 'ruby',
  
  // Java/Kotlin
  java: 'java',
  kt: 'kotlin',
  kts: 'kotlin',
  
  // C/C++
  c: 'c',
  h: 'c',
  cpp: 'cpp',
  cc: 'cpp',
  cxx: 'cpp',
  hpp: 'cpp',
  hh: 'cpp',
  hxx: 'cpp',
  
  // C#
  cs: 'csharp',
  
  // Web
  html: 'html',
  htm: 'html',
  css: 'css',
  scss: 'scss',
  sass: 'sass',
  less: 'less',
  
  // Data formats
  json: 'json',
  jsonc: 'jsonc',
  json5: 'json5',
  yaml: 'yaml',
  yml: 'yaml',
  toml: 'toml',
  xml: 'xml',
  
  // Markdown
  md: 'markdown',
  mdx: 'mdx',
  markdown: 'markdown',
  
  // Shell
  sh: 'bash',
  bash: 'bash',
  zsh: 'zsh',
  fish: 'fish',
  
  // SQL
  sql: 'sql',
  
  // PHP
  php: 'php',
  
  // Lua
  lua: 'lua',
  
  // Swift
  swift: 'swift',
  
  // Objective-C
  m: 'objective-c',
  mm: 'objective-cpp',
  
  // Dart
  dart: 'dart',
  
  // Elixir
  ex: 'elixir',
  exs: 'elixir',
  
  // Haskell
  hs: 'haskell',
  
  // Scala
  scala: 'scala',
  
  // Clojure
  clj: 'clojure',
  cljs: 'clojurescript',
  
  // R
  r: 'r',
  
  // Julia
  jl: 'julia',
  
  // Nim
  nim: 'nim',
  
  // Zig
  zig: 'zig',
  
  // V
  v: 'v',
  
  // Perl
  pl: 'perl',
  pm: 'perl',
  
  // GraphQL
  graphql: 'graphql',
  gql: 'graphql',
  
  // Prisma
  prisma: 'prisma',
  
  // Protobuf
  proto: 'protobuf',
  
  // Dockerfile
  dockerfile: 'dockerfile',
  
  // Makefile
  makefile: 'makefile',
  make: 'makefile',
  
  // Terraform
  tf: 'terraform',
  tfvars: 'terraform',
  
  // Config files
  conf: 'ini',
  config: 'ini',
  ini: 'ini',
  cfg: 'ini',
  properties: 'properties',
  
  // Misc
  txt: 'plaintext',
  text: 'plaintext',
  log: 'log',
  diff: 'diff',
  patch: 'diff',
};

/**
 * Detect language from file path
 */
export function detectLanguage(filePath: string): LanguageInfo {
  // Extract extension
  const ext = filePath.split('.').pop()?.toLowerCase() || '';
  
  // Special case: Dockerfile (no extension)
  if (filePath.endsWith('Dockerfile') || filePath.includes('Dockerfile.')) {
    return {
      language: 'dockerfile',
      isMarkdown: false,
      displayName: 'Dockerfile',
    };
  }
  
  // Special case: Makefile (no extension)
  if (filePath.endsWith('Makefile') || filePath.includes('Makefile.')) {
    return {
      language: 'makefile',
      isMarkdown: false,
      displayName: 'Makefile',
    };
  }
  
  // Look up in map
  const language = LANGUAGE_MAP[ext] || 'plaintext';
  const isMarkdown = language === 'markdown' || language === 'mdx';
  
  return {
    language,
    isMarkdown,
    displayName: language.charAt(0).toUpperCase() + language.slice(1),
  };
}

/**
 * Check if a file should be treated as markdown
 */
export function isMarkdownFile(filePath: string): boolean {
  const ext = filePath.split('.').pop()?.toLowerCase() || '';
  return ext === 'md' || ext === 'mdx' || ext === 'markdown';
}

/**
 * Check if a file should be syntax highlighted
 */
export function shouldHighlight(filePath: string): boolean {
  const info = detectLanguage(filePath);
  // Highlight everything except plaintext and very large files
  return info.language !== 'plaintext';
}

/**
 * Get display name for language
 */
export function getLanguageDisplayName(filePath: string): string {
  return detectLanguage(filePath).displayName;
}
