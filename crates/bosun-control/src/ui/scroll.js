// The transcript's auto-follow flag and the control that returns to the
// newest line.

import { btnBottom, childTranscript, transcript } from './dom.js';
import { out } from './transcript.js';
import { childStick } from './subagents.js';

export { jumpToBottom, scrollToBottom, setStick, syncBtnBottom, syncStick };

let stick = true;     // auto-scroll the transcript when true

// Another module cannot assign an imported binding, so it writes `stick`
// through this.
function setStick(value) {
  stick = value;
}

// The transcript a render just drew into is the one that follows: the session's
// own, on `stick`, or the panel's, on its own flag. A child's frame must never
// move the session's transcript.
function scrollToBottom() {
  if (out === childTranscript) {
    if (childStick) childTranscript.scrollTop = childTranscript.scrollHeight;
    return;
  }
  // A page of older messages is drawn off screen before it goes in above the
  // rest, and must not move the transcript while it is drawn.
  if (out !== transcript) return;
  if (stick) transcript.scrollTop = transcript.scrollHeight;
}

// A keystroke in the composer, a sent message and a press of the control all
// ask for the newest lines, so this one moves the transcript whatever `stick`
// said before.
function jumpToBottom() {
  stick = true;
  transcript.scrollTop = transcript.scrollHeight;
  syncBtnBottom();
}

// Auto-follow off means the reader is off the newest line, so the control and
// the flag are the same state.
function syncBtnBottom() {
  btnBottom.hidden = stick;
}

btnBottom.addEventListener('click', jumpToBottom);

// Whether the transcript is at its end, and the control that shows when it is
// not. The scroll listener asks on every scroll; the visual viewport asks too,
// because the keyboard changes the transcript's height without a scroll.
function syncStick() {
  stick = transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 40;
  syncBtnBottom();
}

transcript.addEventListener('scroll', syncStick);
