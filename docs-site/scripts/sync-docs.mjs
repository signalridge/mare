// Sync + transform the repo's localized `docs/**/*.md` user documentation into
// the 100%-generated Starlight content collection at `src/content/docs/`.
// Never edit generated output by hand. The home pages are not part of it: they
// are `src/pages/[lang]/index.astro`.
//
// Transforms applied per page:
//   - inject Starlight frontmatter (title from the first H1, description from
//     the first paragraph), stripping the now-duplicate H1 from the body;
//   - rewrite relative `*.md` links to base-aware Starlight routes;
//   - rewrite `assets/**` image references to the copied public asset URLs;
//   - wrap diagram images in a link to the standalone asset, so the full-size
//     original is reachable from the downscaled figure in the prose column;
//   - map each edit link back to its repository source instead of generated output;
//   - drop mkdocs-only `markdown` HTML attributes.
//
// Also emits:
//   - `public/raw/<locale>/<slug>.md` for the in-page "Copy Markdown" action;
//   - `public/llms.txt` (structured multi-locale index for AI tools);
//   - `public/llms-full.txt` (concatenated English documentation with resolved links).

import { promises as fs } from 'node:fs';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPOSITORY_ROOT = path.resolve(SCRIPT_DIR, '../..');
const execFileAsync = promisify(execFile);
const DOCS_DIR = path.resolve(SCRIPT_DIR, '../../docs');
const OUT_DIR = path.resolve(SCRIPT_DIR, '../src/content/docs');
const PUBLIC_ASSETS = path.resolve(SCRIPT_DIR, '../public/assets');
const PUBLIC_RAW = path.resolve(SCRIPT_DIR, '../public/raw');
const PUBLIC_LLMS = path.resolve(SCRIPT_DIR, '../public/llms.txt');
const PUBLIC_LLMS_FULL = path.resolve(SCRIPT_DIR, '../public/llms-full.txt');

// Must match `base` and `site` in astro.config.mjs (project Pages path).
const BASE = '/mare';
const SITE = 'https://signalridge.github.io';
const SITE_ROOT = `${SITE}${BASE}`;
const REPOSITORY_BLOB = 'https://github.com/signalridge/mare/blob/main';
const REPOSITORY_EDIT = 'https://github.com/signalridge/mare/edit/main/docs';

const LOCALES = ['en', 'zh', 'ja'];
const IMAGE_EXT = /\.(svg|png|jpe?g|gif|webp|avif)$/i;

const SECTION_ORDER = ['get-started', 'use', 'configure', 'extend', 'integrate', 'reference'];
const SECTION_LABELS = {
  'get-started': 'Get started',
  use: 'Use mare',
  configure: 'Configure',
  extend: 'Extend',
  integrate: 'Integrate',
  reference: 'Reference',
};
const LOCALE_LABELS = {
  en: 'English',
  zh: '简体中文',
  ja: '日本語',
};

async function walk(dir, rootRel = '') {
  const out = [];
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const abs = path.join(dir, entry.name);
    const rel = path.posix.join(rootRel, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'assets') continue; // copied wholesale, not parsed
      out.push(...(await walk(abs, rel)));
    } else if (entry.isFile() && entry.name.endsWith('.md')) {
      out.push(rel);
    }
  }
  return out;
}

function routeFor(docsRelNoExt, sitePrefix = '') {
  if (docsRelNoExt === 'index') return `${sitePrefix}${BASE}/`;
  return `${sitePrefix}${BASE}/${docsRelNoExt}/`;
}

function rewriteTarget(target, currentDir, sitePrefix = '', currentRelNoExt = '') {
  const trimmed = target.trim();
  if (/^(https?:|mailto:|tel:|\/\/)/i.test(trimmed)) return null; // external
  if (trimmed.startsWith('#')) {
    return sitePrefix && currentRelNoExt ? `${routeFor(currentRelNoExt, sitePrefix)}${trimmed}` : null;
  }
  const [pathPart, anchor] = trimmed.split('#');
  const suffix = anchor ? `#${anchor}` : '';

  if (pathPart.endsWith('.md')) {
    const resolved = path.posix.normalize(path.posix.join(currentDir, pathPart));
    if (resolved.startsWith('../')) {
      const repositoryRelative = resolved.slice('../'.length);
      if (repositoryRelative.startsWith('../')) {
        throw new Error(`documentation link escapes the repository: ${trimmed}`);
      }
      return `${REPOSITORY_BLOB}/${repositoryRelative}${suffix}`;
    }
    return routeFor(resolved.replace(/\.md$/, ''), sitePrefix) + suffix;
  }
  if (IMAGE_EXT.test(pathPart) || pathPart.includes('assets/')) {
    const resolved = path.posix.normalize(path.posix.join(currentDir, pathPart));
    // docs/assets/... is copied to public/assets/..., served at `${BASE}/assets/...`
    const idx = resolved.indexOf('assets/');
    if (idx >= 0) return `${sitePrefix}${BASE}/${resolved.slice(idx)}${suffix}`;
  }

  return null;
}

function rewriteLinks(body, currentDir, sitePrefix = '', currentRelNoExt = '') {
  return body.replace(/(\]\()([^)]+)(\))/g, (whole, open, target, close) => {
    const next = rewriteTarget(target, currentDir, sitePrefix, currentRelNoExt);
    return next === null ? whole : `${open}${next}${close}`;
  });
}

function escapeAttr(value) {
  return value
    .replace(/&/g, '&amp;')
    .replace(/"/g, '&quot;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

async function loadDiagramSvgs() {
  const dir = path.join(DOCS_DIR, 'assets', 'diagrams');
  const map = new Map();
  let entries = [];
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch (err) {
    if (err?.code === 'ENOENT') return map;
    throw err;
  }
  for (const entry of entries) {
    if (!entry.isFile() || !entry.name.endsWith('.svg')) continue;
    const raw = await fs.readFile(path.join(dir, entry.name), 'utf8');
    // Collapse blank lines so CommonMark parses the enclosing <figure> as a single HTML block.
    const compact = raw
      .replace(/^<\?xml[^>]*>\s*/i, '')
      .replace(/\r\n/g, '\n')
      .replace(/^\s*\n/gm, '')
      .trim();
    map.set(entry.name, compact);
  }
  return map;
}

// Inline hand-written diagram SVGs so their text stays selectable, theme tokens
// follow Starlight's `data-theme` switch immediately, and the full-size lightbox
// stays one click away. Non-SVG diagrams fall back to linked images.
function linkDiagrams(body, diagramSvgs) {
  return body.replace(
    /(?<!\[)!\[([^\]]*)\]\(([^)\s]*\/assets\/diagrams\/([^)\s/]+))\)/g,
    (_whole, alt, src, filename) => {
      const svg = diagramSvgs.get(filename);
      if (!svg) return `[![${alt}](${src})](${src})`;
      const safeAlt = escapeAttr(alt || filename);
      return [
        `<figure class="mare-diagram not-content">`,
        `<div class="mare-diagram-scroll">${svg}</div>`,
        `<a class="mare-diagram-zoom" href="${escapeAttr(src)}" data-lightbox-link data-diagram-alt="${safeAlt}" aria-label="Expand diagram: ${safeAlt}" title="Expand diagram">⤢</a>`,
        `</figure>`,
      ].join('\n');
    },
  );
}

function deriveDescription(body) {
  const lines = body.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trim();
    if (!line) continue;
    if (/^[#>|`]|^!\[|^<|^-{3,}|^\d+\.\s|^[-*]\s/.test(line)) continue;
    const text = line
      .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
      .replace(/[`*_]/g, '')
      .replace(/\s+/g, ' ')
      .trim();
    if (text.length < 12) continue;
    return text.length > 158 ? `${text.slice(0, 155).replace(/\s+\S*$/, '')}…` : text;
  }
  return undefined;
}

function yamlQuote(value) {
  return `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

async function sourceLastUpdated(rel) {
  try {
    const { stdout } = await execFileAsync(
      'git',
      ['-C', REPOSITORY_ROOT, 'log', '-1', '--format=%cI', '--', path.posix.join('docs', rel)],
      { encoding: 'utf8' },
    );
    const value = stdout.trim();
    if (!value) return undefined;
    if (Number.isNaN(Date.parse(value))) {
      throw new Error(`git returned an invalid commit timestamp for docs/${rel}: ${value}`);
    }
    return value;
  } catch (error) {
    if (error?.code === 'ENOENT' || error?.code === 128) return undefined;
    throw error;
  }
}

function transform(raw, rel, lastUpdated, diagramSvgs) {
  const currentDir = path.posix.dirname(rel);
  let body = raw.replace(/\r\n/g, '\n');

  let title;
  body = body.replace(/^#\s+(.+?)\s*$/m, (_m, heading) => {
    if (title === undefined) {
      title = heading.trim();
      return '';
    }
    return _m;
  });
  if (!title) {
    title = path.posix
      .basename(rel, '.md')
      .replace(/[-_]/g, ' ')
      .replace(/\b\w/g, (c) => c.toUpperCase());
  }

  const currentRelNoExt = rel.replace(/\.md$/, '');
  const description = deriveDescription(body);
  const rewrittenForSite = rewriteLinks(body, currentDir);
  const rewrittenForLlms = rewriteLinks(raw.replace(/\r\n/g, '\n'), currentDir, SITE, currentRelNoExt)
    .replace(/\s+markdown(=("1"|'1'))?(?=[\s>])/g, '');

  let pageBody = linkDiagrams(rewrittenForSite, diagramSvgs);
  pageBody = pageBody.replace(/\s+markdown(=("1"|'1'))?(?=[\s>])/g, '');
  pageBody = pageBody.replace(/^\n+/, '');

  const fm = ['---', `title: ${yamlQuote(title)}`];
  if (description) fm.push(`description: ${yamlQuote(description)}`);
  if (lastUpdated) fm.push(`lastUpdated: ${lastUpdated}`);
  fm.push(`editUrl: ${yamlQuote(`${REPOSITORY_EDIT}/${rel}`)}`, '---', '');
  return {
    title,
    description,
    content: `${fm.join('\n')}\n${pageBody.replace(/\n*$/, '\n')}`,
    llmsBody: rewrittenForLlms,
  };
}

function sortPages(a, b) {
  const secA = SECTION_ORDER.indexOf(a.section);
  const secB = SECTION_ORDER.indexOf(b.section);
  if (secA !== secB) return (secA === -1 ? 999 : secA) - (secB === -1 ? 999 : secB);
  return a.rel.localeCompare(b.rel);
}

function buildLlmsTxt(pages) {
  const lines = [
    '# mare',
    '',
    '> A hyperextensible agent harness. A small Rust kernel confines processes, keeps sessions durable, and runs Lua 5.5 plugins; everything else is a plugin.',
    '',
    '## Key Resources',
    '',
    `- [Full English Documentation (llms-full.txt)](${SITE_ROOT}/llms-full.txt): Concatenated English documentation in one file.`,
    `- [mare Repository](https://github.com/signalridge/mare): Core Rust kernel, CLI, TUI, and built-in Lua plugins.`,
    `- [mare-plugins](https://github.com/signalridge/mare-plugins): Official curated plugins, hooks, and themes via \`mare.plugins.add\`.`,
    `- [cordis-lua](https://github.com/signalridge/cordis-lua): Standalone Lua 5.5 Cordis plugin kernel.`,
    '',
  ];

  for (const locale of LOCALES) {
    lines.push(`## Documentation (${LOCALE_LABELS[locale]})`, '');
    const localePages = pages.filter((p) => p.locale === locale).sort(sortPages);
    for (const section of SECTION_ORDER) {
      const group = localePages.filter((p) => p.section === section);
      if (group.length === 0) continue;
      lines.push(`### ${SECTION_LABELS[section]}`, '');
      for (const page of group) {
        const htmlUrl = `${SITE_ROOT}/${page.relNoExt}/`;
        const rawUrl = `${SITE_ROOT}/raw/${page.rel}`;
        const desc = page.description ? `: ${page.description}` : '';
        lines.push(`- [${page.title}](${htmlUrl}) ([raw](${rawUrl}))${desc}`);
      }
      lines.push('');
    }
  }

  return `${lines.join('\n').trimEnd()}\n`;
}

function buildLlmsFullTxt(pages) {
  const enPages = pages.filter((p) => p.locale === 'en').sort(sortPages);
  const header = [
    '# mare: Complete English Documentation',
    '',
    `Source: ${SITE_ROOT}/en/`,
    `Pages: ${enPages.length}`,
    '',
  ].join('\n');

  const chunks = enPages.map((page) => {
    const htmlUrl = `${SITE_ROOT}/${page.relNoExt}/`;
    const rawUrl = `${SITE_ROOT}/raw/${page.rel}`;
    const divider = '='.repeat(80);
    return [
      divider,
      `Title: ${page.title}`,
      `Section: ${SECTION_LABELS[page.section] || page.section}`,
      `URL: ${htmlUrl}`,
      `Markdown: ${rawUrl}`,
      `Source: docs/${page.rel}`,
      divider,
      '',
      page.llmsBody.trimEnd(),
      '',
    ].join('\n');
  });

  return `${header}\n${chunks.join('\n')}`;
}

async function cleanGenerated() {
  await fs.rm(OUT_DIR, { recursive: true, force: true });
  await fs.mkdir(OUT_DIR, { recursive: true });
  await fs.rm(PUBLIC_ASSETS, { recursive: true, force: true });
  await fs.rm(PUBLIC_RAW, { recursive: true, force: true });
  await fs.rm(PUBLIC_LLMS, { force: true });
  await fs.rm(PUBLIC_LLMS_FULL, { force: true });
}

async function main() {
  await cleanGenerated();
  const diagramSvgs = await loadDiagramSvgs();

  const files = (await walk(DOCS_DIR)).filter(
    (rel) => /^(en|zh|ja)\//.test(rel) && path.posix.basename(rel) !== 'index.md',
  );
  const pages = [];

  for (const rel of files) {
    const raw = await fs.readFile(path.join(DOCS_DIR, rel), 'utf8');
    const lastUpdated = await sourceLastUpdated(rel);
    const outPath = path.join(OUT_DIR, rel);
    const rawOutPath = path.join(PUBLIC_RAW, rel);
    const { title, description, content, llmsBody } = transform(raw, rel, lastUpdated, diagramSvgs);

    await fs.mkdir(path.dirname(outPath), { recursive: true });
    await fs.writeFile(outPath, content);

    await fs.mkdir(path.dirname(rawOutPath), { recursive: true });
    await fs.writeFile(rawOutPath, raw.replace(/\r\n/g, '\n'));

    const parts = rel.split('/');
    pages.push({
      rel,
      relNoExt: rel.replace(/\.md$/, ''),
      locale: parts[0],
      section: parts[1] || '',
      title,
      description,
      llmsBody,
    });
  }

  await fs.writeFile(PUBLIC_LLMS, buildLlmsTxt(pages), 'utf8');
  await fs.writeFile(PUBLIC_LLMS_FULL, buildLlmsFullTxt(pages), 'utf8');

  // Copy docs/assets wholesale -> public/assets (served at `${BASE}/assets/**`).
  const assetsSrc = path.join(DOCS_DIR, 'assets');
  if (await fs.stat(assetsSrc).then(() => true).catch(() => false)) {
    await fs.cp(assetsSrc, PUBLIC_ASSETS, { recursive: true });
  }

  console.log(
    `sync-docs: ${files.length} pages -> ${path.relative(process.cwd(), OUT_DIR)} (+ raw/, llms.txt, llms-full.txt)`,
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
