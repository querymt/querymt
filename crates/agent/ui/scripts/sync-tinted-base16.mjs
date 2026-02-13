#!/usr/bin/env node

import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const BASE16_KEYS = [
  'base00',
  'base01',
  'base02',
  'base03',
  'base04',
  'base05',
  'base06',
  'base07',
  'base08',
  'base09',
  'base0A',
  'base0B',
  'base0C',
  'base0D',
  'base0E',
  'base0F',
];

const SOURCE_REPOSITORY = 'tinted-theming/schemes';
const SOURCE_REF = 'spec-0.11';
const SOURCE_CONTENTS_URL = `https://api.github.com/repos/${SOURCE_REPOSITORY}/contents/base16?ref=${SOURCE_REF}`;
const SOURCE_COMMIT_URL = `https://api.github.com/repos/${SOURCE_REPOSITORY}/commits/${SOURCE_REF}`;
const USER_AGENT = 'querymt-dashboard-theme-sync';

const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = resolve(dirname(scriptPath), '..');
const outputPath = resolve(projectRoot, 'src/utils/generated/tintedBase16Themes.ts');

async function fetchJson(url) {
  const response = await fetch(url, {
    headers: {
      'User-Agent': USER_AGENT,
      Accept: 'application/vnd.github+json',
    },
  });

  if (!response.ok) {
    throw new Error(`Failed to fetch JSON from ${url}: ${response.status} ${response.statusText}`);
  }

  return response.json();
}

async function fetchText(url) {
  const response = await fetch(url, {
    headers: {
      'User-Agent': USER_AGENT,
    },
  });

  if (!response.ok) {
    throw new Error(`Failed to fetch text from ${url}: ${response.status} ${response.statusText}`);
  }

  return response.text();
}

function stripInlineComment(value) {
  let inSingleQuotes = false;
  let inDoubleQuotes = false;

  for (let index = 0; index < value.length; index += 1) {
    const char = value[index];

    if (char === "'" && !inDoubleQuotes) {
      inSingleQuotes = !inSingleQuotes;
      continue;
    }

    if (char === '"' && !inSingleQuotes) {
      inDoubleQuotes = !inDoubleQuotes;
      continue;
    }

    if (char === '#' && !inSingleQuotes && !inDoubleQuotes) {
      return value.slice(0, index).trim();
    }
  }

  return value.trim();
}

function parseYamlScalar(rawValue) {
  const valueWithoutComment = stripInlineComment(rawValue);

  if (!valueWithoutComment) {
    return '';
  }

  const quotedWithDouble = valueWithoutComment.startsWith('"') && valueWithoutComment.endsWith('"');
  const quotedWithSingle = valueWithoutComment.startsWith("'") && valueWithoutComment.endsWith("'");

  if (quotedWithDouble || quotedWithSingle) {
    return valueWithoutComment.slice(1, -1).trim();
  }

  return valueWithoutComment.trim();
}

function normalizeHexColor(value) {
  const normalized = value.replace('#', '').trim();

  if (!/^[0-9a-fA-F]{6}$/.test(normalized)) {
    throw new Error(`Invalid hex color: "${value}"`);
  }

  return `#${normalized.toLowerCase()}`;
}

function titleCaseFromSlug(slug) {
  return slug
    .split('-')
    .filter(Boolean)
    .map((chunk) => chunk.charAt(0).toUpperCase() + chunk.slice(1))
    .join(' ');
}

function inferVariant(slug, parsedVariant) {
  if (parsedVariant === 'light' || parsedVariant === 'dark') {
    return parsedVariant;
  }

  if (slug.includes('light')) {
    return 'light';
  }

  return 'dark';
}

function parseSchemeYaml(yamlText, slug) {
  const parsed = {
    name: '',
    author: '',
    variant: '',
    palette: {},
  };

  let inPalette = false;

  for (const rawLine of yamlText.split(/\r?\n/)) {
    const line = rawLine.trimEnd();
    const trimmed = line.trimStart();

    if (!trimmed || trimmed.startsWith('#')) {
      continue;
    }

    const paletteMatch = line.match(/^\s{2}(base[0-9a-fA-F]{2}):\s*(.+)$/);
    if (inPalette && paletteMatch) {
      const [, key, rawValue] = paletteMatch;
      parsed.palette[key] = normalizeHexColor(parseYamlScalar(rawValue));
      continue;
    }

    const topLevelMatch = line.match(/^([a-zA-Z0-9_-]+):\s*(.*)$/);
    if (!topLevelMatch) {
      continue;
    }

    const [, key, rawValue] = topLevelMatch;
    inPalette = key === 'palette';

    if (key === 'name') {
      parsed.name = parseYamlScalar(rawValue);
    } else if (key === 'author') {
      parsed.author = parseYamlScalar(rawValue);
    } else if (key === 'variant') {
      parsed.variant = parseYamlScalar(rawValue).toLowerCase();
    }
  }

  for (const key of BASE16_KEYS) {
    if (!parsed.palette[key]) {
      throw new Error(`Scheme "${slug}" is missing ${key}`);
    }
  }

  const variant = inferVariant(slug, parsed.variant);
  const label = parsed.name || titleCaseFromSlug(slug);
  const description = parsed.author ? `By ${parsed.author}` : `Tinted Base16 ${label}`;

  return {
    id: `base16-${slug}`,
    label,
    description,
    variant,
    palette: parsed.palette,
  };
}

async function mapWithConcurrency(items, concurrency, mapper) {
  const results = new Array(items.length);
  let index = 0;

  async function worker() {
    while (index < items.length) {
      const currentIndex = index;
      index += 1;
      results[currentIndex] = await mapper(items[currentIndex], currentIndex);
    }
  }

  const workers = Array.from({ length: Math.min(concurrency, items.length) }, () => worker());
  await Promise.all(workers);
  return results;
}

async function main() {
  const [contents, latestCommit] = await Promise.all([
    fetchJson(SOURCE_CONTENTS_URL),
    fetchJson(SOURCE_COMMIT_URL),
  ]);

  const yamlEntries = contents
    .filter((entry) => entry.type === 'file' && entry.name.endsWith('.yaml'))
    .sort((left, right) => left.name.localeCompare(right.name));

  const schemes = await mapWithConcurrency(yamlEntries, 12, async (entry) => {
    const yamlText = await fetchText(entry.download_url);
    const slug = entry.name.replace(/\.yaml$/u, '');
    return parseSchemeYaml(yamlText, slug);
  });

  const source = {
    repository: SOURCE_REPOSITORY,
    ref: SOURCE_REF,
    commitSha: latestCommit.sha,
    committedAt: latestCommit.commit?.committer?.date ?? null,
    generatedAt: new Date().toISOString(),
    totalSchemes: schemes.length,
  };

  const output = `/* eslint-disable */\n/*\n * This file is generated by scripts/sync-tinted-base16.mjs.\n * Do not edit it by hand.\n */\n\nexport type TintedBase16Theme = {\n  id: string;\n  label: string;\n  description: string;\n  variant: 'dark' | 'light';\n  palette: {\n    base00: string;\n    base01: string;\n    base02: string;\n    base03: string;\n    base04: string;\n    base05: string;\n    base06: string;\n    base07: string;\n    base08: string;\n    base09: string;\n    base0A: string;\n    base0B: string;\n    base0C: string;\n    base0D: string;\n    base0E: string;\n    base0F: string;\n  };\n};\n\nexport const TINTED_BASE16_SOURCE = ${JSON.stringify(source, null, 2)} as const;\n\nexport const TINTED_BASE16_SCHEMES: TintedBase16Theme[] = ${JSON.stringify(schemes, null, 2)};\n`;

  await mkdir(dirname(outputPath), { recursive: true });
  await writeFile(outputPath, output, 'utf8');

  console.log(
    `Wrote ${schemes.length} schemes from ${source.repository}@${source.ref} (${source.commitSha}) to ${outputPath}`,
  );
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
