// Reads back the older part of the open session's transcript, a page at a
// time.

import { transcript } from './dom.js';
import { toastError } from './common.js';
import { opened } from './session-view.js';
import { drawAssistant, drawBlock, drawLine, modelCallLine } from './transcript.js';

export { TAIL_MESSAGES, startEarlier };

// ---- Earlier messages ----
// A session opens at its newest messages. The stream's first frame says where
// that replay starts and whether older events exist; the reader gets them a
// page at a time, by scrolling near the top or by tapping the row there. The
// session's `earlier` holds where the next page ends, whether there is one,
// whether a read is in flight, and the row.

/// How many messages the stream replays when a session opens.
const TAIL_MESSAGES = 60;
/// How many messages each read-back adds above the transcript.
const EARLIER_MESSAGES = 60;
/// How close to the top, in pixels, a scroll reads the next page back.
const EARLIER_MARGIN = 400;

function startEarlier(s, history) {
  s.earlier.before = history.before;
  s.earlier.more = !!history.more && history.before != null;
  syncEarlierRow(s);
}

// The row at the top of the transcript: present while older messages exist,
// and saying so while a page is on its way.
function syncEarlierRow(s) {
  const earlier = s.earlier;
  if (!earlier.more) {
    if (earlier.row) earlier.row.remove();
    earlier.row = null;
    return;
  }
  if (!earlier.row) {
    earlier.row = document.createElement('button');
    earlier.row.type = 'button';
    earlier.row.id = 'earlier';
    earlier.row.addEventListener('click', loadEarlier);
  }
  if (transcript.firstChild !== earlier.row) transcript.insertBefore(earlier.row, transcript.firstChild);
  earlier.row.textContent = earlier.loading ? 'Loading earlier messages…' : 'Show earlier messages';
  earlier.row.disabled = earlier.loading;
}

async function loadEarlier() {
  const s = opened;
  if (!s || !s.earlier.more || s.earlier.loading) return;
  const read = s.earlier;
  read.loading = true;
  syncEarlierRow(s);
  try {
    const response = await fetch(
      '/sessions/' + encodeURIComponent(s.id) + '/history?before=' + read.before +
        '&messages=' + EARLIER_MESSAGES
    );
    // A page for a session the pane has left draws nothing.
    if (opened !== s) return;
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const page = await response.json();
    if (opened !== s) return;
    prependEvents(s, page.events);
    if (page.events.length) read.before = page.events[0].seq;
    read.more = page.more && page.events.length > 0;
  } catch (error) {
    if (opened === s) toastError('earlier messages: ' + error.message);
  } finally {
    if (opened === s) {
      read.loading = false;
      syncEarlierRow(s);
    }
  }
}

// Draws a page of older events off screen, then puts it above what is already
// drawn. The reader's distance from the bottom is kept, so the lines they are
// reading stay where they are. Only what the transcript shows is drawn: the
// state, permission, persona and activity events describe the session's
// present, which the header and the live stream already carry.
function prependEvents(s, events) {
  const page = document.createDocumentFragment();
  // The page draws with an ask record of its own, which starts empty because
  // no question in the page is open yet, and with the session's call
  // arguments, whose answers may be in the page.
  const record = { askBox: null, callArgs: s.callArgs };
  for (const frame of events) {
    const event = frame.event;
    let drawn = null;
    if (event.kind === 'message') {
      const message = event.message;
      if (message.block.kind !== 'ask') record.askBox = null;
      drawn = message.role === 'assistant' && message.block.kind === 'text'
        ? drawAssistant(message.block.text, event.at_ms)
        : drawBlock(message, event.at_ms, record);
    } else if (event.kind === 'model_call') {
      drawn = drawLine('mono', modelCallLine(event), event.at_ms);
    } else if (event.kind === 'warning') {
      drawn = drawLine('warning', event.text);
    }
    if (drawn) page.appendChild(drawn);
  }
  // A question the page leaves open may be the one whose answered copy opens
  // what is already drawn: the page cut between them. The answer joins the
  // question, and the copy goes, so the question shows once.
  const pageAsk = record.askBox;
  const drawnAsk = transcript.querySelector('.ask');
  if (pageAsk && !pageAsk.answered && drawnAsk && drawnAsk.dataset.key === pageAsk.key) {
    const answer = drawnAsk.querySelector('.answer');
    if (answer) {
      pageAsk.el.appendChild(answer);
      (drawnAsk.closest('.stamp-row') || drawnAsk).remove();
      if (s.askBox && s.askBox.el === drawnAsk) s.askBox.el = pageAsk.el;
    }
  }
  const anchor = s.earlier.row ? s.earlier.row.nextSibling : transcript.firstChild;
  const fromBottom = transcript.scrollHeight - transcript.scrollTop;
  transcript.insertBefore(page, anchor);
  transcript.scrollTop = transcript.scrollHeight - fromBottom;
}

// A scroll near the top reads the next page back.
transcript.addEventListener('scroll', () => {
  if (transcript.scrollTop < EARLIER_MARGIN) loadEarlier();
});
