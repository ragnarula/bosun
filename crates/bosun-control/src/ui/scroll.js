// The transcript's auto-follow and the control that returns to the newest
// line. The follow flag is the open session's `stick`.

import { btnBottom, transcript } from './dom.js';
import { opened } from './session-view.js';

export { follow, jumpToBottom, syncBtnBottom, syncStick };

// Moves the session's transcript to its newest line, if the reader is there.
// A renderer's caller asks for this after it adds a line.
function follow() {
  if (opened && opened.stick) transcript.scrollTop = transcript.scrollHeight;
}

// A keystroke in the composer, a sent message and a press of the control all
// ask for the newest lines, so this one moves the transcript whatever `stick`
// said before.
function jumpToBottom() {
  if (opened) opened.stick = true;
  transcript.scrollTop = transcript.scrollHeight;
  syncBtnBottom();
}

// Auto-follow off means the reader is off the newest line, so the control and
// the flag are the same state. With no session open there is no line to return
// to.
function syncBtnBottom() {
  btnBottom.hidden = !opened || opened.stick;
}

btnBottom.addEventListener('click', jumpToBottom);

// Whether the transcript is at its end, and the control that shows when it is
// not. The scroll listener asks on every scroll; the visual viewport asks too,
// because the keyboard changes the transcript's height without a scroll.
function syncStick() {
  if (opened) {
    opened.stick = transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 40;
  }
  syncBtnBottom();
}

transcript.addEventListener('scroll', syncStick);
// A tap that opens a line makes the transcript taller, and a reader at the
// newest line stays there.
transcript.addEventListener('click', follow);
