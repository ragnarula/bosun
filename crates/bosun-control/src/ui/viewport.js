// Keeps the session view on the visual viewport while the keyboard is open.

import { view } from './dom.js';
import { syncStick } from './scroll.js';

export { scheduleVisualViewportSync };

// iOS Safari does not shrink the layout viewport when the keyboard opens, so a
// session view pinned to that height keeps the composer exactly where the
// keyboard is drawn, and a tap at the very bottom edge can fall in the Home
// indicator's strip instead of the field. The visual viewport is the part of the
// page the reader can actually see, so the view rides it: its height always,
// and its offset whenever the visible viewport is pushed or shortened. Those
// two numbers do not say what moved it — the keyboard, or a pinch zoom — and
// both can leave the visible window somewhere below the document's top.
//
// The visual viewport exists on modern desktops as well, where it is the whole
// page and the numbers never change; this runs there harmlessly, and a window
// resize costs one read of the transcript's geometry on the frame it asks for
// and one more when the numbers settle. A browser without a visual viewport
// attaches no listeners at all.
// Whether a text field inside the session view holds the focus. Every write to
// the view's height is tied to it: iOS reports the visual viewport wrong on load
// and after stray events, and only a field that really has the focus means a
// keyboard is really up.
let composerFocused = false;

function syncVisualViewport() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  // Nothing is being typed into, so nothing here is a keyboard: the view keeps
  // the height its own rules give it. This is what a fresh page needs — iOS can
  // report the visual viewport wrong on load, and writing that number left the
  // session view a sliver with the home column showing beneath it — and it is
  // also the restore: leaving a field gives the view its whole height back and
  // zeroes the strip, without waiting for a resize that may report a stale
  // number.
  if (!composerFocused) {
    view.style.height = '';
    view.style.top = '0px';
    // Safari can leave the document scrolled after the keyboard closes, and the
    // view, which sits at the document's top, would stay moved up with it.
    if (window.scrollY !== 0) window.scrollTo(0, 0);
    document.documentElement.style.setProperty('--keyboard-inset', '0px');
    syncStick();
    return;
  }
  // Only a plausible keyboard shrink is honoured: the visible viewport is never
  // taller than the layout one, and an empty report is not a keyboard.
  if (!(viewport.height > 0 && viewport.height <= window.innerHeight)) return;
  // Belt and braces: a value that would leave no room for the composer is not a
  // keyboard either, so the view never shrinks past this floor.
  const floor = Math.round(window.innerHeight * 0.4);
  view.style.height = Math.max(viewport.height, floor) + 'px';
  // The offset says how far the page has moved under the visible part, and the
  // strip below it is what the keyboard leaves of the layout viewport; both are
  // clamped. The offset is written whenever the visible viewport is pushed or
  // shortened, and parks at zero only when it is the full height again: on some
  // iOS versions the offset stays stale after the keyboard closes, and writing
  // it then left the whole view offset.
  //
  // The strip is not what tells this code a keyboard is up. Safari can push the
  // page far enough that the visible viewport reaches the layout viewport's
  // bottom, and the strip is then zero with the keyboard still up; reading that
  // as "no keyboard" wrote `top: 0`, which put the view's bottom edge a
  // keyboard-height above the keyboard, and the home column's session list
  // showed in the gap. So the offset decides.
  //
  // The offset the view takes is clamped to the short viewport's own height: a
  // page the keyboard has pushed and a value left stale by an older keyboard
  // look alike in these two numbers, and riding a stale one that stands past
  // the keyboard would put the composer under it. The clamp costs nothing in a
  // reading that is consistent — a pushed page's offset is exactly what its
  // short viewport leaves of the layout viewport — and holds the view's bottom
  // edge at the keyboard either way.
  //
  // Safari has two ways to bring the field above the keyboard: it moves the
  // visible viewport inside the layout viewport, which is `offsetTop`, or it
  // scrolls the document itself, which is `scrollY` and leaves `offsetTop` at
  // zero. The view is placed in the document, so it moves up with a document
  // scroll: its top has to be the sum of the two, or the view sits a
  // keyboard-height above the visible part and the transcript leaves the
  // screen at the top. The strip is measured against the layout viewport, which
  // a document scroll does not move, so it takes `offsetTop` alone.
  const offset = Math.min(Math.max(0, viewport.offsetTop), window.innerHeight);
  const pushed = Math.min(Math.max(0, window.scrollY) + offset, window.innerHeight);
  const covered = Math.max(0, window.innerHeight - viewport.height - offset);
  view.style.top = (viewport.height < window.innerHeight && pushed > 0)
    ? Math.min(pushed, window.innerHeight - viewport.height) + 'px'
    : '0px';
  document.documentElement.style.setProperty('--keyboard-inset', covered + 'px');
  // The keyboard changes the transcript's height, and no scroll event comes
  // with it: this is the resize #19 named as stale, and the control catches up
  // here.
  syncStick();
}

// A resize arrives in bursts — iOS sends one per frame while the keyboard
// animates — and one read and one write per frame is all this needs. The burst
// can end while the keyboard is still moving, so the sync runs a second time
// after the movement has settled: a sample taken mid-animation leaves the view
// short or long by whatever the keyboard had left to travel, and that much of
// the visible area shows the page beneath it.
let visualViewportFrame = null;
let visualViewportSettle = null;
const isTextField = (target) =>
  target instanceof Element && view.contains(target) && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName);

document.addEventListener('focusin', (event) => {
  if (!isTextField(event.target)) return;
  composerFocused = true;
  scheduleVisualViewportSync();
});

document.addEventListener('focusout', (event) => {
  if (!isTextField(event.target)) return;
  composerFocused = false;
  // The field is left: restore now, rather than waiting for the keyboard's own
  // resize, whose numbers can arrive late or stale.
  syncVisualViewport();
});

function scheduleVisualViewportSync() {
  if (visualViewportFrame !== null) return;
  visualViewportFrame = window.requestAnimationFrame(() => {
    visualViewportFrame = null;
    syncVisualViewport();
  });
  window.clearTimeout(visualViewportSettle);
  visualViewportSettle = window.setTimeout(syncVisualViewport, 300);
}

if (window.visualViewport) {
  window.visualViewport.addEventListener('resize', scheduleVisualViewportSync);
  window.visualViewport.addEventListener('scroll', scheduleVisualViewportSync);
  // Leaving a field is when the numbers settle: the keyboard's close arrives as
  // a resize, but not always before the next paint. Taking a field asks for the
  // same sync, because the keyboard it raises is what moves the numbers; the
  // window's own resize covers the frames the visual viewport's numbers miss;
  // and `showSession` asks too, for a session opened with the keyboard up.
  document.addEventListener('focusout', scheduleVisualViewportSync);
  document.addEventListener('focusin', scheduleVisualViewportSync);
  window.addEventListener('resize', scheduleVisualViewportSync);
  // A document scroll moves the view and is not a visual viewport event.
  window.addEventListener('scroll', scheduleVisualViewportSync);
}
