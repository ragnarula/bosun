// The composer: the chat box, its draft, and the ask composer that answers
// a question.

import {
  askChips,
  askFree,
  askInput,
  askSheet,
  btnAskBack,
  btnAskDismiss,
  btnAskSend,
  btnAskType,
  btnSend,
  chatRow,
  input,
  inputRow,
  view,
} from './dom.js';
import { post, toastError } from './common.js';
import { coarsePointer, current } from './session-view.js';
import { jumpToBottom } from './scroll.js';
import { lastMsg } from './transcript.js';

export { USER_REJECTED_TEXT, askSyncTimer, loadDraft, saveDraft, scheduleAskSync, setAskSyncTimer };

let askSyncTimer = null;

let sending = false;

// Another module cannot assign an imported binding, so it writes `askSyncTimer`
// through this.
function setAskSyncTimer(value) {
  askSyncTimer = value;
}

// The unsent chat draft, so closing a session and returning to it never
// loses what the user was typing.
const DRAFT_KEY = 'bosun.draft';

function saveDraft() {
  try {
    localStorage.setItem(DRAFT_KEY, input.value);
  } catch (error) {
    // Private mode or a full quota: the draft degrades to in-memory for this
    // page load.
  }
}

function clearDraft() {
  try {
    localStorage.removeItem(DRAFT_KEY);
  } catch (error) {
  }
}

function loadDraft() {
  try {
    return localStorage.getItem(DRAFT_KEY) || '';
  } catch (error) {
    return '';
  }
}

// Mirrors `bosun_control::api::USER_REJECTED_TEXT`; the pane styles the line
// as a user action, never as words the user typed.
const USER_REJECTED_TEXT = 'user rejected the question';

// ---- Ask composer ----
// The footer has two modes and shows one at a time: the chat row for ordinary
// messages, and the ask composer while a question is live. A live question is
// answered by tapping an option (or typing in the free-answer field); Dismiss
// rejects it without answering, leaving a durable note in the transcript and
// returning to the chat row. The session keeps waiting until the user sends
// its next message.

function showChatComposer() {
  askSheet.hidden = true;
  chatRow.hidden = false;
}

function renderAskComposer(ask) {
  askChips.textContent = '';
  askFree.hidden = true;
  btnAskBack.hidden = true;
  if (ask.options && ask.options.length > 0) {
    for (const option of ask.options) {
      const chip = document.createElement('button');
      chip.type = 'button';
      chip.textContent = option;
      chip.addEventListener('click', () => answer(option));
      askChips.appendChild(chip);
    }
    askChips.hidden = false;
    btnAskType.hidden = false;
  } else {
    // An open question has no options to tap: the free-answer field is the
    // composer itself.
    askChips.hidden = true;
    btnAskType.hidden = true;
    askFree.hidden = false;
    if (!coarsePointer()) askInput.focus();
  }
  // The reader is typing: leave the composer in place. Hiding it takes the
  // focus with it, and a programmatic re-focus right after is the one iOS
  // treats as "already focused" and answers with no keyboard. The question is
  // not lost: leaving the field re-runs the ask sync.
  if (document.activeElement === input) return;
  askSheet.hidden = false;
  chatRow.hidden = true;
}

// A blur is the moment the refusal above can be retried.
input.addEventListener('blur', scheduleAskSync);

function applyAskComposer() {
  if (view.hidden || inputRow.hidden) return;
  const block = lastMsg;
  const live = block && block.kind === 'ask' && !block.answer ? block : null;
  if (live) {
    renderAskComposer(live);
  } else {
    showChatComposer();
  }
}

// A replay or live burst can deliver many messages quickly; the ask composer
// only reflects the settled end of the stream, so a resolved question never
// flashes as live mid-replay.
function scheduleAskSync() {
  if (askSyncTimer) window.clearTimeout(askSyncTimer);
  askSyncTimer = window.setTimeout(applyAskComposer, 250);
}

// A tap on an ask option answers the question exactly like a typed message.
async function answer(text) {
  if (sending || !text.trim() || !current) return;
  // An answer resolves the live question; leave the ask composer immediately.
  // The durable event that confirms it re-syncs the composer on arrival.
  showChatComposer();
  sending = true;
  try {
    await post('/sessions/' + encodeURIComponent(current) + '/messages', {
      content: text,
      redirect: false,
    });
  } catch (error) {
    toastError('answer: ' + error.message);
    scheduleAskSync();
  } finally {
    sending = false;
  }
}

// Reject the question on screen: the durable note lands in the transcript,
// the binding is cleared, and the model is not woken — the session keeps
// waiting until the user types its next message.
async function dismissAsk() {
  if (sending || !current) return;
  showChatComposer();
  sending = true;
  try {
    const response = await fetch(
      '/sessions/' + encodeURIComponent(current) + '/reject',
      { method: 'POST' }
    );
    if (!response.ok) throw new Error('HTTP ' + response.status);
  } catch (error) {
    toastError('dismiss: ' + error.message);
    scheduleAskSync();
  } finally {
    sending = false;
  }
}

btnAskDismiss.addEventListener('click', dismissAsk);

btnAskType.addEventListener('click', () => {
  askChips.hidden = true;
  btnAskType.hidden = true;
  askFree.hidden = false;
  btnAskBack.hidden = false;
  if (!coarsePointer()) askInput.focus();
});

btnAskBack.addEventListener('click', () => {
  askFree.hidden = true;
  btnAskBack.hidden = true;
  if (lastMsg && lastMsg.options && lastMsg.options.length > 0) {
    askChips.hidden = false;
    btnAskType.hidden = false;
  }
});

btnAskSend.addEventListener('click', () => {
  const text = askInput.value.trim();
  askInput.value = '';
  answer(text);
});

askInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    btnAskSend.click();
  }
});

async function send() {
  if (sending || !input.value.trim()) return;
  jumpToBottom();
  sending = true;
  btnSend.disabled = true;
  try {
    await post('/sessions/' + encodeURIComponent(current) + '/messages', {
      content: input.value,
      redirect: false,
    });
    input.value = '';
    clearDraft();
  } catch (error) {
    toastError('send: ' + error.message);
  } finally {
    sending = false;
    btnSend.disabled = false;
    // Only where a focus is the reader's: on a touch screen iOS may treat this
    // re-focus as one it already has and withhold the keyboard, so the tap that
    // follows asks for it instead. This is send's own re-focus; the other gate
    // is the `answer in your own words` button's, above.
    if (!coarsePointer()) input.focus();
  }
}

btnSend.addEventListener('click', send);

// A tap anywhere on the composer's row asks for the field, not only a tap on the
// box itself: the row is a bigger target, and the tap is the reader's own
// gesture, which is what a phone needs to raise the keyboard. A control on the
// row keeps its own tap.
inputRow.addEventListener('click', (event) => {
  if (event.target.closest('button, a, input, textarea, select')) return;
  input.focus();
});
// Keep the chat draft in sync with what the user is typing.
input.addEventListener('input', () => {
  saveDraft();
  jumpToBottom();
});
input.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
    event.preventDefault();
    send();
  }
});
