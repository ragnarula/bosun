// Renders message markdown as DOM nodes.

import { mdPre, renderMermaid } from './diagram.js';

export { renderMarkdown };

// The nodes that `text` draws as, in order, for the caller to place.
function renderMarkdown(text) {
  const nodes = document.createDocumentFragment();
  const lines = text.split('\n');
  let inCode = false;
  let fenceLanguage = '';
  let code = [];
  let para = [];
  const paraOut = () => {
    if (para.length === 0) return;
    const paragraph = document.createElement('p');
    appendInline(paragraph, para.join('\n'));
    nodes.appendChild(paragraph);
    para = [];
  };
  const codeOut = (closed) => {
    if (code.length === 0) return;
    const source = code.join('\n');
    const language = fenceLanguage;
    code = [];
    fenceLanguage = '';
    if (closed && language === 'mermaid') {
      nodes.appendChild(renderMermaid(source));
      return;
    }
    nodes.appendChild(mdPre(source));
  };
  let i = 0;
  while (i < lines.length) {
    const raw = lines[i];
    if (inCode) {
      if (raw.trimStart().startsWith('```')) {
        inCode = false;
        codeOut(true);
      } else {
        code.push(raw);
      }
      i += 1;
      continue;
    }
    const line = raw.trim();
    if (line.startsWith('```')) {
      paraOut();
      inCode = true;
      fenceLanguage = fenceInfo(line.slice(3));
      i += 1;
      continue;
    }
    if (line === '') {
      paraOut();
      i += 1;
      continue;
    }
    if (isTableStart(line)) {
      const header = tableCells(line);
      const aligns = tableAligns(lines[i + 1] || '');
      if (aligns && aligns.length === header.length) {
        paraOut();
        // The delimiter row decides each column's alignment; it is never a
        // row of its own.
        const body = [];
        i += 2;
        while (i < lines.length && lines[i].trimStart().startsWith('|')) {
          body.push(tableCells(lines[i]));
          i += 1;
        }
        nodes.appendChild(tableNode(header, body, aligns));
        continue;
      }
    }
    const heading = headingLevel(line);
    if (heading > 0) {
      paraOut();
      const el = document.createElement('h' + heading);
      appendInline(el, line.slice(heading).trim());
      nodes.appendChild(el);
      i += 1;
      continue;
    }
    if (isRule(line)) {
      paraOut();
      nodes.appendChild(document.createElement('hr'));
      i += 1;
      continue;
    }
    if (line.startsWith('>')) {
      paraOut();
      const quote = document.createElement('blockquote');
      while (i < lines.length && lines[i].trimStart().startsWith('>')) {
        const paragraph = document.createElement('p');
        appendInline(paragraph, lines[i].trimStart().slice(1).trim());
        quote.appendChild(paragraph);
        i += 1;
      }
      nodes.appendChild(quote);
      continue;
    }
    const marker = listMarker(line);
    if (marker !== null) {
      paraOut();
      const item = document.createElement('div');
      item.className = 'md-li';
      const bullet = document.createElement('span');
      bullet.className = 'md-marker';
      bullet.textContent = marker;
      item.appendChild(bullet);
      appendInline(item, line.slice(marker.length));
      nodes.appendChild(item);
      i += 1;
      continue;
    }
    para.push(line);
    i += 1;
  }
  paraOut();
  // Text that ends inside a fence is still streaming: it shows its source
  // rather than a diagram of the part that arrived.
  if (inCode && code.length > 0) codeOut(false);
  return nodes;
}

function headingLevel(line) {
  let hashes = 0;
  while (hashes < line.length && line[hashes] === '#') hashes += 1;
  if (hashes < 1 || hashes > 6) return 0;
  const rest = line[hashes];
  return rest === ' ' || rest === '\t' ? hashes : 0;
}

function isRule(line) {
  if (line.length < 3) return false;
  for (const char of line) {
    if (char !== '-' && char !== '*' && char !== '_') return false;
  }
  return true;
}

function listMarker(line) {
  if (/^[-*+]\s/.test(line)) return '• ';
  const numbered = line.match(/^\d+([.)])\s/);
  return numbered ? numbered[0].slice(0, -1) + ' ' : null;
}

// The cells of one table row: the text between its pipes, trimmed. A pipe at
// either end is the table's edge, not a cell.
function tableCells(line) {
  const text = line.trim();
  const inner = text.startsWith('|') ? text.slice(1) : text;
  const body = inner.endsWith('|') ? inner.slice(0, -1) : inner;
  return body.split('|').map((cell) => cell.trim());
}

// Whether a line opens a table: a trimmed line starting with `|` and holding
// at least two of them. A lone `|` in prose is not one.
function isTableStart(line) {
  const text = line.trim();
  return text.startsWith('|') && (text.match(/\|/g) || []).length >= 2;
}

// The alignment each cell of a delimiter row asks for, or null when the line
// is not a delimiter row: only pipes, dashes, colons and spaces, with a dash.
// `---:` is right, `:---:` centre, and anything else left.
function tableAligns(line) {
  const text = line.trim();
  if (!text.includes('-')) return null;
  for (const char of text) {
    if (char !== '|' && char !== '-' && char !== ':' && char !== ' ') return null;
  }
  return tableCells(text).map((cell) =>
    cell.startsWith(':') && cell.endsWith(':') ? 'center'
      : cell.endsWith(':') ? 'right'
      : 'left');
}

// The language of an opening fence: its info string's first token, lowercased.
// Anything after that token is a title or options, which the pane ignores; an
// unknown language keeps the fence a plain <pre>.
function fenceInfo(info) {
  const token = info.trim().split(/\s+/)[0];
  return token ? token.toLowerCase() : '';
}

// Splits inline text into styled tokens. Underscores stay literal so names
// like `file_name` are not split into italics.
function parseInline(text) {
  const chars = Array.from(text);
  const tokens = [];
  let run = '';
  let bold = false;
  let italic = false;
  let code = false;
  const flush = () => {
    if (run === '') return;
    tokens.push({ text: run, bold, italic, code });
    run = '';
  };
  let i = 0;
  while (i < chars.length) {
    const char = chars[i];
    if (char === '`') {
      flush();
      code = !code;
      i += 1;
      continue;
    }
    if (char === '*') {
      let count = 0;
      while (chars[i + count] === '*') count += 1;
      flush();
      if (count === 3) {
        bold = !bold;
        italic = !italic;
      } else if (count === 2) {
        bold = !bold;
      } else {
        italic = !italic;
      }
      i += count;
      continue;
    }
    if (char === '[') {
      const close = chars.indexOf(']', i + 1);
      if (close !== -1 && chars[close + 1] === '(') {
        const paren = chars.indexOf(')', close + 2);
        if (paren !== -1) {
          flush();
          tokens.push({
            text: chars.slice(i + 1, close).join(''),
            url: chars.slice(close + 2, paren).join(''),
          });
          i = paren + 1;
          continue;
        }
      }
      run += '[';
      i += 1;
      continue;
    }
    run += char;
    i += 1;
  }
  flush();
  return tokens;
}

// Whether a link target from a model's reply may become an href. A reply can
// quote text the model did not write — a file it read, a page it fetched, a
// tool's output — and the pane's own origin reaches the control plane's API,
// so only a target whose scheme is http, https or mailto is followed. The
// scheme is the ASCII characters before the first colon, compared without
// regard to case, the way a browser reads one, so `JaVaScRiPt:x` is refused
// with `javascript:x`. A target that opens with anything else is refused too:
// `data:text/html,...`; a scheme-relative `//host/path` or a relative path
// like `/ui` or `docs/x.md`, which a browser would resolve against the pane's
// own origin; and a target with a leading space or tab, since the check reads
// the target as it stands and does not trim. Whitespace after the scheme — a
// trailing space, say — stays in the target, because it cannot change the
// scheme, and a browser drops it when it resolves the URL.
function isSafeLinkTarget(target) {
  const scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(target);
  if (scheme === null) return false;
  return ['http', 'https', 'mailto'].includes(scheme[1].toLowerCase());
}

function appendInline(parent, text) {
  for (const token of parseInline(text)) {
    if (token.url && isSafeLinkTarget(token.url)) {
      const link = document.createElement('a');
      link.className = 'md-link';
      link.href = token.url;
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
      link.textContent = token.text;
      parent.appendChild(link);
      continue;
    }
    // A link token with no target, or one the check refuses, falls through to
    // this text path: the reader still sees the text the model wrote, without
    // a live link.
    const classes = [];
    if (token.bold) classes.push('md-bold');
    if (token.italic) classes.push('md-italic');
    if (token.code) classes.push('md-code');
    if (classes.length === 0) {
      parent.appendChild(document.createTextNode(token.text));
      continue;
    }
    const span = document.createElement('span');
    span.className = classes.join(' ');
    span.textContent = token.text;
    parent.appendChild(span);
  }
}

// The class a cell carries for its column's alignment. A left column is what
// a cell does by default, so it carries none.
function alignClass(align) {
  return align === 'left' ? '' : 'md-align-' + align;
}

// One markdown table: a header row and the body rows, inside a holder that
// scrolls sideways rather than overflowing the transcript, like the diagram
// holder. Every cell goes through appendInline, so links, emphasis and inline
// code keep working inside it, and the alignment the delimiter row asked for
// is a class on the cell.
function tableNode(header, rows, aligns) {
  const holder = document.createElement('div');
  holder.className = 'md-table-wrap';
  const table = document.createElement('table');
  table.className = 'md-table';
  const head = document.createElement('thead');
  const headRow = document.createElement('tr');
  aligns.forEach((align, index) => {
    const th = document.createElement('th');
    th.className = alignClass(align);
    appendInline(th, header[index]);
    headRow.appendChild(th);
  });
  head.appendChild(headRow);
  table.appendChild(head);
  const body = document.createElement('tbody');
  for (const row of rows) {
    const tr = document.createElement('tr');
    aligns.forEach((align, index) => {
      const td = document.createElement('td');
      td.className = alignClass(align);
      appendInline(td, row[index] === undefined ? '' : row[index]);
      tr.appendChild(td);
    });
    body.appendChild(tr);
  }
  table.appendChild(body);
  holder.appendChild(table);
  return holder;
}
