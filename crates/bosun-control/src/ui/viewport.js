// Fits the pane to the part of the page the reader can see while a keyboard is
// up. The stylesheet sizes the body to the dynamic viewport; this module
// changes that only while a text field has the focus.

import { syncStick } from './scroll.js';

// Android Chrome shrinks the layout for the keyboard itself, through the
// viewport tag's `interactive-widget=resizes-content`. iOS Safari ignores that
// setting: the layout keeps its height, the keyboard is drawn over its bottom,
// and Safari scrolls the page to bring the focused field into view. So while a
// text field has the focus, the body takes the visual viewport's height, which
// puts the composer, the body's last row, just above the keyboard. And the
// document is always kept at its top: the body now fits inside the visible
// part, and a scroll Safari made to reveal the field would only move the
// screen's top out of sight.
//
// A browser with no visual viewport, or a desktop where it is the whole
// window, keeps the height the stylesheet gives.

// Whether a text field has the focus. Only then is a short visual viewport a
// keyboard: iOS can report the visual viewport wrong on load and after stray
// events.
let fieldFocused = false;

function syncVisualViewport() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  // A pinch zoom also shortens the visible part and moves it, and the reader
  // pans a zoomed page on purpose, so a zoomed page is left alone.
  const zoomed = Math.abs(viewport.scale - 1) > 0.01;
  if (!fieldFocused || zoomed) {
    // Clear the explicit height: on iOS the keyboard has gone and the
    // body returns to its CSS height; on Android, where the layout
    // viewport itself resizes, setting body height to the layout
    // viewport's avoids a stale dynamic-viewport (dvh) value a keyboard
    // dismissal can leave behind.
    document.body.style.height = !fieldFocused && !zoomed ? window.innerHeight + 'px' : '';
  } else if (viewport.height > 0 && viewport.height <= window.innerHeight) {
    document.body.style.height = viewport.height + 'px';
  }
  // Any other report is not a keyboard: the visible part is never taller than
  // the layout viewport, and it is never empty. The body keeps the height it
  // has.
  if (!zoomed && (window.scrollY !== 0 || viewport.offsetTop !== 0)) window.scrollTo(0, 0);
  // The keyboard changes the transcript's height without a scroll event, and
  // the control that returns to the newest line catches up here.
  syncStick();
}

const isTextField = (element) =>
  element instanceof Element && /^(INPUT|TEXTAREA|SELECT)$/.test(element.tagName);

// A resize arrives in bursts, one per frame while the keyboard moves, and one
// sync per frame is all this needs. The burst can end while the keyboard is
// still moving, so the sync runs once more after it has settled.
let frame = null;
let settle = null;

function scheduleVisualViewportSync() {
  if (frame !== null) return;
  frame = window.requestAnimationFrame(() => {
    frame = null;
    syncVisualViewport();
  });
  window.clearTimeout(settle);
  settle = window.setTimeout(syncVisualViewport, 300);
}

if (window.visualViewport) {
  document.addEventListener('focusin', (event) => {
    if (!isTextField(event.target)) return;
    fieldFocused = true;
    scheduleVisualViewportSync();
  });
  document.addEventListener('focusout', (event) => {
    if (!isTextField(event.target)) return;
    // Focus moving from one field to another keeps the keyboard up.
    if (isTextField(event.relatedTarget)) return;
    fieldFocused = false;
    // The keyboard's own resize can arrive late or stale, so the body gets
    // its full height back now.
    syncVisualViewport();
    scheduleVisualViewportSync();
  });
  window.visualViewport.addEventListener('resize', scheduleVisualViewportSync);
  window.visualViewport.addEventListener('scroll', scheduleVisualViewportSync);
  window.addEventListener('resize', scheduleVisualViewportSync);
  window.addEventListener('scroll', scheduleVisualViewportSync);
}
