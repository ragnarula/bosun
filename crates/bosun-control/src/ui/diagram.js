// Renders a mermaid fence as SVG through the vendored bundle.

import { $ } from './dom.js';

export { mdPre, renderMermaid };

// A ```mermaid fence renders as SVG through the vendored bundle, which is
// deferred and 5.5 MB, so it is usually still downloading when the fence
// arrives. The diagram waits on that load, and every failure — a bundle that
// never arrived, an invalid source, an SVG the parser rejects — puts the
// fence's own text in a <pre> where the diagram would have gone, so the
// transcript never loses text or shows half a diagram.
const mermaidReady = new Promise((resolve, reject) => {
  const bundle = $('mermaid-bundle');
  bundle.addEventListener('load', () => resolve(window.mermaid));
  bundle.addEventListener('error', () => reject(new Error('mermaid did not load')));
});
// A transcript with no diagram never awaits this; the catch keeps a failed
// load out of the console as an unhandled rejection.
mermaidReady.catch(() => {});

let mermaidApi = null;
let mermaidRenders = 0;

function mdPre(source) {
  const pre = document.createElement('pre');
  pre.className = 'md-pre';
  pre.textContent = source;
  return pre;
}

async function renderMermaid(container, source) {
  // The holder is in place before the first await, so the diagram keeps its
  // place in the transcript while the bundle loads.
  const holder = document.createElement('div');
  holder.className = 'md-mermaid';
  container.appendChild(holder);

  try {
    const mermaid = await mermaidReady;
    if (!mermaidApi) {
      // Configured once per page. htmlLabels works only at the top level:
      // under `flowchart` it leaves HTML labels in <foreignObject>, which the
      // XML parse below rejects.
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: 'strict',
        htmlLabels: false,
        theme: 'dark',
        // Draw at the diagram's own size instead of shrinking it to the
        // column: a wide flowchart scaled into a phone's column leaves its
        // labels a few pixels tall, and the holder scrolls instead.
        flowchart: { useMaxWidth: false },
      });
      mermaidApi = mermaid;
    }
    mermaidRenders += 1;
    // A fresh id per diagram: mermaid keys its scratch element on it, and one
    // reused id leaves the previous render's element behind.
    const id = 'md-mermaid-' + mermaidRenders;
    let svg;
    try {
      ({ svg } = await mermaidApi.render(id, source));
    } catch (error) {
      // A render that throws leaves its scratch element in the body; mermaid
      // removes it only on a render that reaches its end.
      const scratch = document.getElementById('d' + id);
      if (scratch) scratch.remove();
      throw error;
    }
    const parsed = new DOMParser().parseFromString(svg, 'image/svg+xml');
    if (parsed.querySelector('parsererror') || parsed.documentElement.localName !== 'svg') {
      throw new Error('mermaid returned no SVG');
    }
    holder.appendChild(document.importNode(parsed.documentElement, true));
  } catch {
    holder.replaceWith(mdPre(source));
  }
}
