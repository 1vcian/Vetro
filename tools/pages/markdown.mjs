// A small Markdown to HTML converter for the user guide on the site
// (docs/user/*.md -> docs/*.html, tools/pages/build.sh). No npm
// dependencies, like the rest of the site. It covers the subset the guide
// uses, and nothing else:
//
//   # headings (with ids for anchors), paragraphs, **bold**, _italic_ and
//   *italic*, `code`, [links](x.md#anchor) (.md becomes .html, README.md
//   becomes index.html), ![images](images/x.jpg), fenced code blocks, "> "
//   quotes, "-"/"*" and "1." lists with one level of nesting, tables with a
//   header row, "---" rules.
//
//   node tools/pages/markdown.mjs SRC_DIR OUT_DIR   # every .md in SRC_DIR
//
// Tested by tests/web/unit.mjs ("user guide Markdown").

import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const escapeHtml = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** The anchor id of a heading, like GitHub's: lowercase, punctuation dropped, spaces to dashes. */
export const slug = (text) => text.toLowerCase().replace(/<[^>]+>/g, '').replace(/[^\p{L}\p{N} _-]/gu, '').trim().replace(/ /g, '-');

/** A link target: `x.md` -> `x.html`, `README.md` -> `index.html` (anchors kept); other URLs as they are. */
export function linkTarget(href) {
  if (/^[a-z]+:/i.test(href) || href.startsWith('#')) return href;
  return href.replace(/(^|\/)README\.md(#|$)/, '$1index.html$2').replace(/\.md(#|$)/, '.html$1');
}

/** Inline Markdown of one paragraph or cell. */
export function inline(text) {
  const codes = [];
  let s = text.replace(/`([^`]+)`/g, (_, c) => {
    codes.push(`<code>${escapeHtml(c)}</code>`);
    return `\u0000${codes.length - 1}\u0000`;
  });
  s = escapeHtml(s);
  s = s.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_, alt, src) => `<img src="${src}" alt="${alt}" loading="lazy">`);
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, href) => `<a href="${linkTarget(href.replace(/&amp;/g, '&')).replace(/&/g, '&amp;')}">${label}</a>`);
  s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/(^|[^\w*])\*([^*\s][^*]*)\*(?!\w)/g, '$1<em>$2</em>');
  s = s.replace(/(^|[^\w])_([^_\s][^_]*)_(?!\w)/g, '$1<em>$2</em>');
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => codes[Number(i)]);
}

/** The cells of a table row: `|` inside `code` does not split. */
function cells(line) {
  const s = line.trim().replace(/^\|/, '').replace(/\|$/, '');
  const out = [''];
  let code = false;
  for (const ch of s) {
    if (ch === '`') code = !code;
    if (ch === '|' && !code) out.push('');
    else out[out.length - 1] += ch;
  }
  return out.map((c) => c.trim());
}

/** Markdown text -> { html, title } (the title is the first level-1 heading). */
export function markdownToHtml(md) {
  const lines = md.replace(/\r\n/g, '\n').split('\n');
  const out = [];
  let title = '';
  let i = 0;
  const isBlockStart = (l) => /^(#{1,6} |```|> ?|\s*([-*]|\d+\.) |\||---\s*$)/.test(l);
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i++;
      continue;
    }
    let m;
    if ((m = /^```(\w*)/.exec(line))) {
      const body = [];
      for (i++; i < lines.length && !lines[i].startsWith('```'); i++) body.push(lines[i]);
      i++;
      out.push(`<pre><code${m[1] ? ` class="language-${m[1]}"` : ''}>${escapeHtml(body.join('\n'))}</code></pre>`);
    } else if ((m = /^(#{1,6}) (.*)$/.exec(line))) {
      const level = m[1].length;
      const html = inline(m[2].trim());
      if (level === 1 && !title) title = m[2].replace(/[`*_]/g, '').trim();
      out.push(`<h${level} id="${slug(m[2].replace(/\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/[`*]/g, ''))}">${html}</h${level}>`);
      i++;
    } else if (/^---\s*$/.test(line)) {
      out.push('<hr>');
      i++;
    } else if (/^> ?/.test(line)) {
      const body = [];
      for (; i < lines.length && /^> ?/.test(lines[i]); i++) body.push(lines[i].replace(/^> ?/, ''));
      out.push(`<blockquote>${markdownToHtml(body.join('\n')).html}</blockquote>`);
    } else if (/^\|/.test(line) && i + 1 < lines.length && /^\|?\s*:?-{3,}/.test(lines[i + 1])) {
      const head = cells(line);
      const rows = [];
      for (i += 2; i < lines.length && /^\|/.test(lines[i]); i++) rows.push(cells(lines[i]));
      out.push(`<div class="table"><table><thead><tr>${head.map((c) => `<th>${inline(c)}</th>`).join('')}</tr></thead><tbody>` +
        rows.map((r) => `<tr>${r.map((c) => `<td>${inline(c)}</td>`).join('')}</tr>`).join('') + '</tbody></table></div>');
    } else if (/^([-*]|\d+\.) /.test(line)) {
      const html = [];
      i = list(lines, i, 0, html);
      out.push(html.join(''));
    } else {
      const para = [];
      for (; i < lines.length && lines[i].trim() && !isBlockStart(lines[i]); i++) para.push(lines[i].trim());
      if (!para.length) para.push(lines[i++].trim());
      out.push(`<p>${inline(para.join(' '))}</p>`);
    }
  }
  return { html: out.join('\n'), title };
}

/** A list starting at line `i` with the given indentation; returns the next line. */
function list(lines, i, indent, out) {
  const ordered = /^\s*\d+\. /.test(lines[i]);
  out.push(ordered ? '<ol>' : '<ul>');
  const item = new RegExp(`^ {${indent}}(${ordered ? '\\d+\\.' : '[-*]'}) (.*)$`);
  while (i < lines.length) {
    const m = item.exec(lines[i]);
    if (!m) break;
    const text = [m[2]];
    const nested = [];
    for (i++; i < lines.length; i++) {
      const l = lines[i];
      if (!l.trim()) {
        // A blank line ends the item unless the list goes on after it.
        if (i + 1 < lines.length && (item.test(lines[i + 1]) || /^\s{2,}\S/.test(lines[i + 1]))) continue;
        break;
      }
      const deeper = /^(\s+)([-*]|\d+\.) /.exec(l);
      if (deeper && deeper[1].length > indent) {
        i = list(lines, i, deeper[1].length, nested) - 1;
        continue;
      }
      if (/^\s+\S/.test(l) && !item.test(l)) {
        text.push(l.trim());
        continue;
      }
      break;
    }
    out.push(`<li>${inline(text.join(' '))}${nested.join('')}</li>`);
  }
  out.push(ordered ? '</ol>' : '</ul>');
  return i;
}

/** The project's source, linked from every page. */
const SOURCE = 'https://github.com/1vcian/Vetro';

/** A complete page of the guide, with the site's look (web/site/index.html). */
export function page({ title, body, home = '../', nav = true }) {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>${escapeHtml(title.includes('user guide') ? title : `${title} - Vetro user guide`)}</title>
  <link rel="icon" type="image/png" sizes="32x32" href="${home}app/icons/icon-32.png">
  <style>
    :root { color-scheme: dark; --bg: #0a0e14; --panel: #121923; --line: #223042; --fg: #e3eaf3; --dim: #a4b2c4; --code: #151e2b; --accent: #6aa8ff; --cyan: #7fe3ff; --violet: #a58bff; }
    * { box-sizing: border-box; }
    html { scroll-padding-top: 4.5rem; }
    body { margin: 0; font: 16px/1.65 -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, system-ui, sans-serif; color: var(--fg); background: radial-gradient(70% 40% at 90% -10%, rgba(165, 139, 255, .09), transparent 60%), radial-gradient(60% 40% at 5% -10%, rgba(127, 227, 255, .08), transparent 60%), var(--bg); background-attachment: fixed; }
    a { color: var(--accent); text-underline-offset: 2px; }
    a:hover { color: var(--cyan); }
    :focus-visible { outline: 2px solid var(--cyan); outline-offset: 2px; }
    nav { position: sticky; top: 0; z-index: 2; border-bottom: 1px solid var(--line); background: rgba(10, 14, 20, .86); backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px); }
    nav > div { max-width: 62rem; margin: 0 auto; padding: .65rem 1rem; display: flex; align-items: center; gap: .5rem; }
    nav a { text-decoration: none; white-space: nowrap; }
    nav .brand { display: inline-flex; align-items: center; gap: .55rem; color: var(--fg); font-weight: 750; font-size: 1.05rem; }
    nav .brand img { width: 26px; height: 26px; border: 0; border-radius: 7px; }
    nav .crumb { color: var(--dim); font-size: .93rem; margin-right: auto; padding-left: .7rem; border-left: 1px solid var(--line); margin-left: .4rem; }
    nav .crumb:hover { color: var(--fg); }
    nav .link { color: var(--fg); font-size: .9rem; font-weight: 600; padding: .38rem .8rem; border: 1px solid var(--line); border-radius: .6rem; background: rgba(22, 31, 43, .7); }
    nav .link:hover { border-color: var(--accent); color: var(--fg); }
    nav .launch { color: #06121f; font-size: .9rem; font-weight: 700; padding: .42rem .95rem; border-radius: .6rem; background: linear-gradient(95deg, var(--cyan), var(--accent), var(--violet)); }
    nav .launch:hover { color: #06121f; filter: brightness(1.08); }
    main { max-width: 46rem; margin: 0 auto; padding: 1.6rem 1rem 3rem; }
    h1 { font-size: 2.1rem; line-height: 1.2; letter-spacing: -.015em; margin: 1rem 0 1.2rem; }
    h2 { font-size: 1.4rem; line-height: 1.3; margin: 2.4rem 0 .8rem; }
    h3 { font-size: 1.08rem; margin: 1.8rem 0 .5rem; }
    li { margin: .25rem 0; }
    code { background: var(--code); border: 1px solid var(--line); padding: .08em .35em; border-radius: .3em; font: .9em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    pre { background: var(--code); border: 1px solid var(--line); padding: .85rem 1rem; overflow-x: auto; border-radius: .6rem; }
    pre code { background: none; border: 0; padding: 0; }
    blockquote { margin: 1.2rem 0; padding: .5rem 1rem; border: 1px solid var(--line); border-left: .2rem solid var(--accent); border-radius: .5rem; background: rgba(18, 25, 35, .7); color: var(--dim); }
    .table { overflow-x: auto; }
    table { border-collapse: collapse; margin: 1rem 0; }
    th, td { border: 1px solid var(--line); padding: .4rem .7rem; text-align: left; vertical-align: top; }
    th { background: var(--panel); }
    img { max-width: 100%; border: 1px solid var(--line); border-radius: .6rem; }
    h1 img { border: 0; }
    hr { border: 0; border-top: 1px solid var(--line); margin: 2rem 0; }
    footer { border-top: 1px solid var(--line); color: var(--dim); font-size: .88rem; }
    footer > div { max-width: 46rem; margin: 0 auto; padding: 1.2rem 1rem 2rem; display: flex; flex-wrap: wrap; gap: .4rem 1.2rem; }
    footer span { margin-right: auto; }
    footer a { color: var(--dim); }
    footer a:hover { color: var(--fg); }
    @media (max-width: 560px) { nav .link { display: none; } h1 { font-size: 1.7rem; } }
  </style>
</head>
<body>
${nav ? `  <nav aria-label="Vetro"><div>
    <a class="brand" href="${home}"><img src="${home}app/icons/icon-64.png" alt="" width="26" height="26">Vetro</a>
    <a class="crumb" href="index.html">User guide</a>
    <a class="link" href="${SOURCE}">Source</a>
    <a class="link" href="${home}#support">Support</a>
    <a class="launch" href="${home}app/">Launch Vetro</a>
  </div></nav>\n` : ''}  <main>
${body}
  </main>
${nav ? `  <footer><div>
    <span>Vetro, a whole phone in a browser tab.</span>
    <a href="index.html">User guide</a>
    <a href="${SOURCE}">Source</a>
    <a href="${home}#support">Support Vetro</a>
  </div></footer>\n` : ''}</body>
</html>
`;
}

/** Converts every .md of `src` into `out` (and copies `src/images`). Returns the pages written. */
export function buildGuide(src, out) {
  mkdirSync(out, { recursive: true });
  const written = [];
  for (const f of readdirSync(src).filter((x) => x.endsWith('.md')).sort()) {
    const { html, title } = markdownToHtml(readFileSync(join(src, f), 'utf8'));
    const name = linkTarget(f);
    writeFileSync(join(out, name), page({ title: title || f, body: html }));
    written.push(name);
  }
  const images = join(src, 'images');
  if (existsSync(images)) {
    mkdirSync(join(out, 'images'), { recursive: true });
    for (const f of readdirSync(images)) if (statSync(join(images, f)).isFile()) copyFileSync(join(images, f), join(out, 'images', f));
  }
  return written;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [src, out] = process.argv.slice(2);
  if (!src || !out) {
    console.error('usage: node tools/pages/markdown.mjs SRC_DIR OUT_DIR');
    process.exit(2);
  }
  console.log(`user guide: ${buildGuide(src, out).join(', ')}`);
}
