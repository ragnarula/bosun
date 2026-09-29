// Sends the viewport numbers the pane sees to the control plane, which logs
// them. It runs only when the address carries `?viewport-report`: a phone's
// keyboard cannot be reproduced anywhere else, so its numbers are read from
// the control plane's log.
//
// Each event is sampled as it arrives, before viewport.js answers it: that
// module undoes Safari's scroll, and a sample taken after it would always read
// a document at its top. main.js imports this module before viewport.js, so
// these listeners are registered, and run, first.

import { input, view } from './dom.js';

// At most one batch per frame and four per second.
const MIN_GAP_MS = 250;
// The samples one batch carries, which keeps a batch inside the control
// plane's body limit.
const MAX_BATCH = 16;
// The samples held while a batch waits. Older ones are dropped, and counted.
const MAX_QUEUED = 64;
// A keyboard's movement can end after its last event, so one more sample is
// taken this long after the last one.
const SETTLE_MS = 500;

// Tells one page load's samples from another's in the log.
const page = Math.random().toString(36).slice(2, 10);

const queue = [];
let dropped = 0;
let failed = 0;
let frame = null;
let gapTimer = null;
let settleTimer = null;
let lastSent = -Infinity;

// `checkVisibility` is missing before Safari 17.4. A box with no client rects
// is not rendered.
function rectOf(element) {
  if (element.getClientRects().length === 0) return null;
  const rect = element.getBoundingClientRect();
  return { top: rect.top, bottom: rect.bottom, height: rect.height };
}

function describe(element) {
  if (!element || element === document.body) return null;
  return element.tagName.toLowerCase() + (element.id ? '#' + element.id : '');
}

function measure(event) {
  const viewport = window.visualViewport;
  return {
    at: performance.now(),
    event,
    inner_width: window.innerWidth,
    inner_height: window.innerHeight,
    visual: viewport ? {
      width: viewport.width,
      height: viewport.height,
      offset_top: viewport.offsetTop,
      page_top: viewport.pageTop ?? null,
      scale: viewport.scale,
    } : null,
    scroll_y: window.scrollY,
    scroll_height: document.documentElement.scrollHeight,
    view: rectOf(view),
    composer: rectOf(input),
    focused: describe(document.activeElement),
  };
}

function send() {
  frame = null;
  lastSent = performance.now();
  const samples = queue.splice(0, MAX_BATCH);
  const body = JSON.stringify({ page, dropped, failed, samples });
  dropped = 0;
  failed = 0;
  fetch('/viewport-report', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body,
    keepalive: true,
  }).catch(() => {});
  if (queue.length) schedule();
}

// Sends the queue on the next frame the gap allows.
function schedule() {
  if (frame !== null || gapTimer !== null) return;
  const wait = lastSent + MIN_GAP_MS - performance.now();
  if (wait > 0) {
    gapTimer = window.setTimeout(() => {
      gapTimer = null;
      frame = window.requestAnimationFrame(send);
    }, wait);
    return;
  }
  frame = window.requestAnimationFrame(send);
}

function sample(event) {
  try {
    queue.push(measure(event));
    if (queue.length > MAX_QUEUED) {
      queue.shift();
      dropped += 1;
    }
  } catch (error) {
    // A browser missing one of the numbers must not stop the reports.
    failed += 1;
  }
  schedule();
}

function report(event) {
  sample(event);
  window.clearTimeout(settleTimer);
  settleTimer = window.setTimeout(() => sample('settled after ' + event), SETTLE_MS);
}

if (new URLSearchParams(location.search).has('viewport-report')) {
  const on = (target, name, label) =>
    target.addEventListener(name, () => report(label), { capture: true });
  if (window.visualViewport) {
    on(window.visualViewport, 'resize', 'visual resize');
    on(window.visualViewport, 'scroll', 'visual scroll');
  }
  on(window, 'resize', 'resize');
  on(window, 'scroll', 'scroll');
  on(document, 'focusin', 'focusin');
  on(document, 'focusout', 'focusout');
  report('load');
}
