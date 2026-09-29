// Reads back the older part of the open session's transcript, a page at a
// time.

import { transcript } from './dom.js';
import { toastError } from './common.js';
import { current } from './session-view.js';
import {
  appendAssistant,
  appendLine,
  modelCallLine,
  openAskBox,
  out,
  renderBlock,
  setOpenAskBox,
  setOut,
} from './transcript.js';

export { TAIL_MESSAGES, setEarlier, startEarlier };

// The older part of the open session's transcript, which the pane reads back a
// page at a time: the seq the next page ends before, whether the session has
// more, and whether a read is in flight. A new object per opened session, so a
// read that lands after the session changed can tell it is stale.
let earlier = { before: null, more: false, loading: false };

// Another module cannot assign an imported binding, so it writes `earlier`
// through this.
function setEarlier(value) {
  earlier = value;
}

// ---- Earlier messages ----
// A session opens at its newest messages. The stream's first frame says where
// that replay starts and whether older events exist; the reader gets them a
// page at a time, by scrolling near the top or by tapping the row there.

/// How many messages the stream replays when a session opens.
const TAIL_MESSAGES = 60;
/// How many messages each read-back adds above the transcript.
const EARLIER_MESSAGES = 60;
/// How close to the top, in pixels, a scroll reads the next page back.
const EARLIER_MARGIN = 400;

let earlierRow = null;

function startEarlier(history) {
  earlier.before = history.before;
  earlier.more = !!history.more && history.before != null;
  syncEarlierRow();
}

// The row at the top of the transcript: present while older messages exist,
// and saying so while a page is on its way.
function syncEarlierRow() {
  if (!earlier.more) {
    if (earlierRow) earlierRow.remove();
    earlierRow = null;
    return;
  }
  if (!earlierRow) {
    earlierRow = document.createElement('button');
    earlierRow.type = 'button';
    earlierRow.id = 'earlier';
    earlierRow.addEventListener('click', loadEarlier);
  }
  if (transcript.firstChild !== earlierRow) transcript.insertBefore(earlierRow, transcript.firstChild);
  earlierRow.textContent = earlier.loading ? 'Loading earlier messages…' : 'Show earlier messages';
  earlierRow.disabled = earlier.loading;
}

async function loadEarlier() {
  if (!current || !earlier.more || earlier.loading) return;
  const id = current;
  const read = earlier;
  read.loading = true;
  syncEarlierRow();
  try {
    const response = await fetch(
      '/sessions/' + encodeURIComponent(id) + '/history?before=' + read.before +
        '&messages=' + EARLIER_MESSAGES
    );
    if (earlier !== read) return;
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const page = await response.json();
    if (earlier !== read) return;
    prependEvents(page.events);
    if (page.events.length) read.before = page.events[0].seq;
    read.more = page.more && page.events.length > 0;
  } catch (error) {
    if (earlier === read) toastError('earlier messages: ' + error.message);
  } finally {
    if (earlier === read) {
      read.loading = false;
      syncEarlierRow();
    }
  }
}

// Draws a page of older events off screen, then puts it above what is already
// drawn. The reader's distance from the bottom is kept, so the lines they are
// reading stay where they are. Only what the transcript shows is drawn: the
// state, permission, persona and activity events describe the session's
// present, which the header and the live stream already carry.
function prependEvents(events) {
  const page = document.createElement('div');
  const previousOut = out;
  const previousAsk = openAskBox;
  setOut(page);
  setOpenAskBox(null);
  let pageAsk = null;
  try {
    for (const frame of events) {
      const event = frame.event;
      if (event.kind === 'message') {
        const message = event.message;
        if (message.block.kind !== 'ask') setOpenAskBox(null);
        if (message.role === 'assistant' && message.block.kind === 'text') {
          appendAssistant(message.block.text, event.at_ms);
        } else {
          renderBlock(message, event.at_ms);
        }
      } else if (event.kind === 'model_call') {
        appendLine('mono', modelCallLine(event), event.at_ms);
      } else if (event.kind === 'warning') {
        appendLine('warning', event.text);
      }
    }
    pageAsk = openAskBox;
  } finally {
    setOut(previousOut);
    setOpenAskBox(previousAsk);
  }
  // A question the page leaves open may be the one whose answered copy opens
  // what is already drawn: the page cut between them. The answer joins the
  // question, and the copy goes, so the question shows once.
  const drawnAsk = transcript.querySelector('.ask');
  if (pageAsk && !pageAsk.answered && drawnAsk && drawnAsk.dataset.key === pageAsk.key) {
    const answer = drawnAsk.querySelector('.answer');
    if (answer) {
      pageAsk.el.appendChild(answer);
      (drawnAsk.closest('.stamp-row') || drawnAsk).remove();
      if (openAskBox && openAskBox.el === drawnAsk) openAskBox.el = pageAsk.el;
    }
  }
  const anchor = earlierRow ? earlierRow.nextSibling : transcript.firstChild;
  const fromBottom = transcript.scrollHeight - transcript.scrollTop;
  while (page.firstChild) transcript.insertBefore(page.firstChild, anchor);
  transcript.scrollTop = transcript.scrollHeight - fromBottom;
}

// A scroll near the top reads the next page back.
transcript.addEventListener('scroll', () => {
  if (transcript.scrollTop < EARLIER_MARGIN) loadEarlier();
});
