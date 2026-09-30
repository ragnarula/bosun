"""Browser checks for the web pane, on a phone-sized Chromium.

Usage: python3 pane.py <base url>

The base url serves the real control-plane router over a store that
`tests/browser.rs` seeds. This script knows the seeded session ids and their
contents, and nothing else about the server. It drives the pane only through
the DOM, the browser's history, a stubbed visual viewport and server responses
it holds back or rewrites, so the checks hold while the pane's code is
restructured. Each check prints PASS or FAIL.
The exit status is 1 when any check fails, and 2 when Chromium cannot be
started: Playwright or its Chromium is not installed, or it cannot run here.
"""

import json
import re
import sys
import traceback
from datetime import datetime
from urllib.parse import quote
from zoneinfo import ZoneInfo

try:
    from playwright.sync_api import Error as PlaywrightError
    from playwright.sync_api import TimeoutError as PlaywrightTimeout
    from playwright.sync_api import sync_playwright
except ImportError:
    PlaywrightError = None

# The exit status when Chromium cannot be started, most often because
# Playwright or its Chromium is not installed. tests/browser.rs names the same
# number.
NO_BROWSER = 2
INSTALL = """The browser tests need the playwright package and its Chromium:
    python3 -m pip install --user playwright
    python3 -m playwright install chromium
See docs/developer/workflows/running-tests.md."""

BASE = sys.argv[1].rstrip('/')

# The seeded sessions, as tests/browser.rs creates them.
LONG = 'long-session'
ASK = 'ask-session'
TURNS = 300
# Found in the list by their summaries.
LONG_ROW = 'the long session'
ASK_ROW = 'the ask session'
RUNNING_ROW = 'the running session'
PENDING_ASK_ROW = 'the pending ask session'
EMPTY_ROW = 'the empty session'
DIAGRAM_ROW = 'the diagram session'
DIAGRAM = 'diagram-session'
RUNNING = 'running-session'
EMPTY = 'empty-session'
CHILD = 'child-session'
ASK_CHILD = 'ask-child'
PENDING_ASK = 'pending-ask-session'
KINDS = 'kinds-session'
KINDS_ROW = 'the kinds session'
REUSE_ROW = 'the reuse session'
OPEN_ASK = 'open-ask-session'
# A child's name when it has no summary: the first line of its instructions.
CHILD_NAME = 'look around'
ASK_BEFORE = 100
ASK_AFTER = 59
LIST_ROWS = 40

PORTRAIT = (390, 844)
LANDSCAPE = (844, 390)

# The pane's elements these checks read. A restructure that renames one changes
# it here only.
HOME = '#home > main'
# The pane's screens. Exactly one is in the page at a time.
HOME_SCREEN = '#home'
TAB_SCREENS = ['#machines-tab', '#skills-tab', '#mcp-tab']
SESSION_ROW = '.session-row'
VIEW = '#session-view'
TRANSCRIPT = '#transcript'
TRANSCRIPT_LINE = '#transcript .msg'
# Every row the transcript draws, whatever kind: a message is `.msg`, and a
# divider, a tool strip or a warning is `.line`.
TRANSCRIPT_ROWS = '#transcript > *'
USER_LINE = '#transcript .msg.user'
REPLY_LINE = '#transcript .msg.assistant'
EARLIER = '#earlier'
ASK_BOX = '#transcript .ask'
ASK_ANSWER = '.answer'
BOTTOM_CONTROL = '#btn-bottom'
COMPOSER = '#input'
BACK = '#btn-back'
MORE = '#btn-more'
SHEET = '#view-sheet'
SHEET_CLOSE = '#btn-sheet-close'
CLEAR_ROW = '#row-clear'
CLEAR_BTN = '#btn-clear'
CLEAR_NOTE = '#view-clear'
WATCH_CHILD = '#transcript .child-watch'
CHILD_PANEL = '#child-panel'
CHILD_LINE = '#child-transcript .msg'
CHILD_CLOSE = '#btn-child-collapse'
VIEW_TITLE = '#view-title'
STATUS_LABEL = '#view-waiting'
ACTIVITY_ROW = '#activity-log .activity-row'
ASK_COMPOSER = '#ask-sheet'
CHAT_ROW = '#chat-row'
DIAGRAM_DRAWN = '#transcript .md-mermaid svg'
# The boxes meant to scroll sideways: code, tables and diagrams, which are
# wider than a phone by nature. Nothing else may.
SIDEWAYS_SCROLLERS = 'pre, .md-table-wrap, .md-mermaid'

failures = []


def check(name, ok, detail=''):
    print(('PASS ' if ok else 'FAIL ') + name + (f' -- {detail}' if detail and not ok else ''))
    if not ok:
        failures.append(name)


def until(page, condition, arg=None, timeout=5000):
    """Whether the JS `condition` becomes true within `timeout` ms."""
    try:
        page.wait_for_function(condition, arg=arg, timeout=timeout)
        return True
    except PlaywrightTimeout:
        return False


# The screen's layout, as the checks see it. The pane shows one screen at a
# time: the home column with the session list, a session, or a tab. A new
# layout changes these helpers and leaves the checks alone.

SHOWN_SCREENS = """(screens) => screens.filter(sel => getComputedStyle(document.querySelector(sel)).display !== 'none')"""


def screens_shown(page):
    """The screens in the page: home, then the session, then the tabs."""
    return page.evaluate(SHOWN_SCREENS, [HOME_SCREEN, VIEW] + TAB_SCREENS)


def list_showing(page):
    """The session list is on screen, and no other screen is in the page."""
    return page.is_visible(HOME) and screens_shown(page) == [HOME_SCREEN]


def session_showing(page):
    """A session is on screen, and no other screen is in the page."""
    return page.is_visible(VIEW) and screens_shown(page) == [VIEW]


def list_scroll(page):
    return page.eval_on_selector(HOME, 'home => home.scrollTop')


def scroll_list(page, top):
    return page.eval_on_selector(HOME, '(home, top) => { home.scrollTop = top; return home.scrollTop; }', top)


def visible_row(page):
    """A session row wholly inside the visible part of the list, so a tap on
    it does not scroll the list first. The list is its own scroll box."""
    return page.evaluate_handle(
        """([home, sel]) => {
          const box = document.querySelector(home).getBoundingClientRect();
          return [...document.querySelectorAll(sel)].find(r => {
            const rect = r.getBoundingClientRect();
            return rect.top >= box.top && rect.bottom <= box.bottom;
          });
        }""", [HOME, SESSION_ROW]).as_element()


# The session view's box, and the composer row's bottom edge, in the
# coordinates of the visible part of the page, which the viewport stub sets:
# the top is 0 when the view starts at the visible top. The stub fakes a
# document scroll and a viewport offset, and the view is placed in the
# document, so a document scroll carries it with the page: its place is moved
# by the scroll the test set, and the visible part starts at the offset.
VIEW_IN_VISIBLE_PART = f"""() => {{
  const stub = window.__viewportStub;
  const moved = stub.realScrollY() - window.scrollY - window.visualViewport.offsetTop;
  const rect = document.querySelector('{VIEW}').getBoundingClientRect();
  const composer = document.querySelector('#input-row').getBoundingClientRect();
  return {{ top: Math.round(rect.top + moved), height: Math.round(rect.height),
           composerBottom: Math.round(composer.bottom + moved),
           visible: Math.round(window.visualViewport.height) }};
}}"""


def view_covers_visible_part(page, timeout=3000):
    """Whether the view comes to start at the visible top with the visible
    height and the composer at its bottom, and the box it has."""
    ok = until(page, f"""() => {{ const b = ({VIEW_IN_VISIBLE_PART})();
      return b.top === 0 && b.height === b.visible && b.composerBottom === b.visible; }}""",
               timeout=timeout)
    return ok, page.evaluate(VIEW_IN_VISIBLE_PART)


def view_covers_screen(page, timeout=3000):
    """Whether the view comes to cover the whole layout viewport with the
    composer at its bottom, as it does with no keyboard, and the box it has."""
    box = f"""() => {{ const r = document.querySelector('{VIEW}').getBoundingClientRect();
      const c = document.querySelector('#input-row').getBoundingClientRect();
      return {{ top: Math.round(r.top), height: Math.round(r.height), composerBottom: Math.round(c.bottom),
               screen: window.innerHeight }}; }}"""
    ok = until(page, f"() => {{ const b = ({box})(); return b.top === 0 && b.height === b.screen && b.composerBottom === b.screen; }}",
               timeout=timeout)
    return ok, page.evaluate(box)


# The document's scroll and the visible part's offset, as the stub holds them.
DOCUMENT_AT_TOP = '() => window.scrollY === 0 && window.visualViewport.offsetTop === 0'


def drop_hidden_list_scroll(page):
    """Does to the hidden list what WebKit does and Chromium does not: a box
    taken out of the page loses its scroll. Chromium keeps the scroll of a
    `display: none` box, so without this a pane that forgot to restore the
    list's scroll would still pass here. Taking the element out of the
    document and putting it back in the same place drops the scroll the same
    way, and calls no pane code."""
    assert not page.is_visible(HOME), 'the list must be hidden to lose its scroll'
    page.eval_on_selector(HOME, """home => {
      const parent = home.parentNode, next = home.nextSibling;
      home.remove();
      parent.insertBefore(home, next);
    }""")


# Replaces the browser's visual viewport with one whose height and offset the
# test sets, and lets the test set the document's scrollY, which Chromium will
# not scroll here because the pane's document never overflows. A change is
# announced with a resize event, as a keyboard's is. A `window.scrollTo` moves
# the visible part to the place it names, which clears the offset as well as
# the scroll: that is how Safari's reveal scroll is expected to answer it, and
# the phone's viewport report is what confirms it. `__viewportStub` gives the
# measuring helpers the real scroll beside the one the test set.
VIEWPORT_STUB = """
(() => {
  const realScrollY = Object.getOwnPropertyDescriptor(window, 'scrollY');
  const realScrollTo = window.scrollTo.bind(window);
  const set = { height: null, offsetTop: 0, scrollY: null };
  const viewport = new EventTarget();
  Object.defineProperties(viewport, {
    height: { get: () => set.height ?? window.innerHeight },
    width: { get: () => window.innerWidth },
    offsetTop: { get: () => set.offsetTop },
    offsetLeft: { get: () => 0 },
    pageTop: { get: () => set.offsetTop },
    pageLeft: { get: () => 0 },
    scale: { get: () => 1 },
  });
  Object.defineProperty(window, 'visualViewport', { configurable: true, get: () => viewport });
  Object.defineProperty(window, 'scrollY', {
    configurable: true,
    get: () => set.scrollY ?? realScrollY.get.call(window),
  });
  window.scrollTo = (x, y) => {
    const top = typeof x === 'object' ? x.top : y;
    if (set.scrollY !== null) set.scrollY = top;
    set.offsetTop = 0;
    realScrollTo(x, y);
  };
  window.__viewportStub = {
    set(height, offsetTop, scrollY) {
      Object.assign(set, { height, offsetTop, scrollY });
      viewport.dispatchEvent(new Event('resize'));
    },
    // A change with no event, as the tail of a keyboard's movement can be.
    quiet(height, offsetTop, scrollY) {
      Object.assign(set, { height, offsetTop, scrollY });
    },
    realScrollY: () => realScrollY.get.call(window),
  };
})();
"""

# Records when the page sends each viewport report, without changing what the
# request does.
RECORD_REPORTS = """
(() => {
  const real = window.fetch.bind(window);
  window.__reportTimes = [];
  window.fetch = (url, ...rest) => {
    if (String(url).endsWith('/viewport-report')) window.__reportTimes.push(performance.now());
    return real(url, ...rest);
  };
})();
"""

# Counts the event streams the page has open, without changing what they do.
COUNT_STREAMS = """
(() => {
  const Real = window.EventSource;
  const all = [];
  window.EventSource = class extends Real {
    constructor(...args) { super(...args); all.push(this); }
  };
  window.__openStreams = () => all.filter(es => es.readyState !== Real.CLOSED).map(es => es.url);
})();
"""

# A browser with no visual viewport API. The pane then never measures the
# viewport, so nothing but its own teardown re-arms the transcript's
# auto-follow for the next session. This page is the only place the
# teardown's own re-arm is visible; no current browser lacks the API.
NO_VISUAL_VIEWPORT = """
Object.defineProperty(window, 'visualViewport', { configurable: true, get: () => undefined });
"""

# Chromium keeps the lines on screen still when content is inserted above them
# (scroll anchoring). iOS Safari does not, so the pane keeps the reader's place
# itself, and the checks turn anchoring off to see that it does.
NO_SCROLL_ANCHORING = """
document.addEventListener('DOMContentLoaded', () => {
  const style = document.createElement('style');
  style.textContent = '* { overflow-anchor: none !important; }';
  document.head.appendChild(style);
});
"""

# What makes the page wider than the screen, in two kinds:
# - escaped: an element whose right edge passes the screen's and which no
#   sideways scroller holds;
# - scrolls sideways: a box that clips or scrolls content wider than itself
#   while it is not meant to. A box that scrolls only up and down still
#   computes overflow-x to auto, so content wider than it shows here, not as
#   an escaped element. A box that ends its text with an ellipsis clips on
#   purpose.
OVERFLOW = """
(sideways) => {
  const width = document.documentElement.clientWidth;
  const escaped = [], scrolls = [];
  const name = (el) => `${el.tagName.toLowerCase()}#${el.id}.${el.className}`;
  for (const el of document.body.querySelectorAll('*')) {
    const rect = el.getBoundingClientRect();
    if (!rect.width && !rect.height) continue;
    const inScroller = el.parentElement && el.parentElement.closest(sideways);
    if (rect.right > width + 1 && !inScroller) escaped.push(`${name(el)} right=${Math.round(rect.right)}`);
    const style = getComputedStyle(el);
    const meant = el.matches(sideways) || style.textOverflow === 'ellipsis';
    if (!meant && !inScroller && style.overflowX !== 'visible' && el.scrollWidth > el.clientWidth + 1) {
      scrolls.push(`${name(el)} ${el.scrollWidth}>${el.clientWidth}`);
    }
  }
  return {
    scrollWidth: document.documentElement.scrollWidth,
    width,
    escaped: escaped.slice(0, 5),
    scrollsSideways: scrolls.slice(0, 5),
  };
}
"""

AT_BOTTOM = """
(sel) => {
  const t = document.querySelector(sel);
  return t.scrollTop + t.clientHeight >= t.scrollHeight - 2;
}
"""


def new_page(browser, errors, size=PORTRAIT, stub_viewport=False, visual_viewport=True,
             touch=True, http_errors=False, **context_options):
    """A phone page. `touch=False` gives a desktop page with a fine pointer.
    `http_errors=True` is for a check whose routes answer with an error
    status on purpose: the browser logs each such response to the console."""
    context = browser.new_context(
        viewport={'width': size[0], 'height': size[1]},
        is_mobile=touch,
        has_touch=touch,
        device_scale_factor=3,
        **context_options,
    )
    context.add_init_script(NO_SCROLL_ANCHORING)
    context.add_init_script(COUNT_STREAMS)
    if stub_viewport:
        context.add_init_script(VIEWPORT_STUB)
    if not visual_viewport:
        context.add_init_script(NO_VISUAL_VIEWPORT)
    page = context.new_page()
    page.set_default_timeout(15000)
    page.on('pageerror', lambda error: errors.append(f'page error: {error}'))

    def console(msg):
        if msg.type != 'error':
            return
        if http_errors and msg.text.startswith('Failed to load resource'):
            return
        errors.append(f'console: {msg.text}')
    page.on('console', console)
    return page


def open_session(page, session_id):
    page.goto(f'{BASE}/#s={quote(session_id)}')
    page.wait_for_selector(TRANSCRIPT_LINE)


def open_from_list(page, summary):
    """Opens the session whose row shows `summary`, by tapping the row, so the
    pane opens it in the same document as the session before it."""
    page.locator(SESSION_ROW, has_text=summary).first.tap()
    page.wait_for_selector(VIEW, state='visible')


def leave_session(page):
    page.tap(BACK)
    page.wait_for_selector(VIEW, state='hidden')


def shown(selector):
    return f"() => {{ const el = document.querySelector('{selector}'); return !!el && el.checkVisibility(); }}"


def open_list(page):
    page.goto(BASE + '/')
    page.wait_for_function(
        """([sel, n]) => document.querySelectorAll(sel).length >= n""", arg=[SESSION_ROW, LIST_ROWS])


def numbers(page, selector, prefix):
    """The number after `prefix` in each matching line, in document order."""
    texts = page.eval_on_selector_all(selector, 'els => els.map(e => e.textContent)')
    found = []
    for text in texts:
        match = re.match(re.escape(prefix) + r' (\d+)', text.strip())
        if match:
            found.append(int(match.group(1)))
    return found


def drawn_lines(page):
    return page.locator(TRANSCRIPT_LINE).count()


def settled_rows(page):
    """How many rows the transcript holds once the tail replay has stopped
    drawing: the count is read until two reads in a row agree, so a check does
    not mistake the replay's own arrivals for a row a control drew."""
    rows = -1
    for _ in range(50):
        counted = page.locator(TRANSCRIPT_ROWS).count()
        if counted == rows:
            return counted
        rows = counted
        page.wait_for_timeout(100)
    return rows


def page_drawn(page, lines_before):
    """Whether a read-back adds lines and leaves the earlier row idle."""
    return until(page, """([sel, before]) => document.querySelectorAll(sel).length > before""",
                 [TRANSCRIPT_LINE, lines_before]) and until(
        page, """(sel) => { const row = document.querySelector(sel); return !row || !row.disabled; }""", EARLIER)


def read_back_one_page(page, how):
    """Reads one older page, by scrolling the transcript to its top or by
    tapping the row there. Returns whether the page was drawn."""
    lines_before = drawn_lines(page)
    if how == 'tap':
        page.tap(EARLIER)
    else:
        page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = 0; }')
    return page_drawn(page, lines_before)


def read_back_everything(page):
    """Scrolls to the top until the earlier row goes or a scroll draws
    nothing more. The caller checks what was drawn and whether the row went."""
    for _ in range(100):
        if not page.locator(EARLIER).count() or not read_back_one_page(page, 'scroll'):
            return


def overflow(page):
    return page.evaluate(OVERFLOW, SIDEWAYS_SCROLLERS)


def fits(result):
    return (result['scrollWidth'] <= result['width']
            and not result['escaped'] and not result['scrollsSideways'])


def check_tail_and_read_back(browser, errors):
    page = new_page(browser, errors)
    open_session(page, LONG)
    check('the transcript opens at its bottom', until(page, AT_BOTTOM, TRANSCRIPT))

    users = numbers(page, USER_LINE, 'user message')
    check('a session opens at its newest messages only',
          users and users[-1] == TURNS - 1 and len(users) < TURNS, f'{len(users)} of {TURNS}')
    check('the earlier row shows while older messages exist',
          page.locator(EARLIER).is_visible() and 'earlier' in page.inner_text(EARLIER).lower())
    check('an open session covers the list', session_showing(page))

    # The scroll and the reading of the line's place are one synchronous step,
    # so the page cannot land between them.
    anchor = f'user message {users[0]}'
    place = """([transcript, lines, text, scroll]) => {
      if (scroll !== null) document.querySelector(transcript).scrollTop = scroll;
      const line = [...document.querySelectorAll(lines)].find(e => e.textContent.trim() === text);
      return line.getBoundingClientRect().top;
    }"""
    lines_before = drawn_lines(page)
    before = page.evaluate(place, [TRANSCRIPT, USER_LINE, anchor, 100])
    drawn = page_drawn(page, lines_before)
    check('scrolling near the top reads an older page back', drawn)
    if not drawn:
        page.context.close()
        return
    after = page.evaluate(place, [TRANSCRIPT, USER_LINE, anchor, None])
    check('a read-back keeps the reader\'s line in place', abs(after - before) <= 1, f'{before} -> {after}')
    order = numbers(page, USER_LINE, 'user message')
    check('a read-back keeps the messages in order',
          order == list(range(order[0], TURNS)), order[:3])

    check('tapping the earlier row reads an older page back', read_back_one_page(page, 'tap'))

    read_back_everything(page)
    users = numbers(page, USER_LINE, 'user message')
    replies = numbers(page, REPLY_LINE, 'reply')
    check('reading everything back shows every message once, in order',
          users == list(range(TURNS)) and replies == list(range(TURNS)),
          f'{len(users)} user lines, {len(replies)} replies')
    check('the earlier row goes once nothing older is left', not page.locator(EARLIER).count())
    page.context.close()


def check_ask_across_a_page_boundary(browser, errors):
    page = new_page(browser, errors)
    pages = []
    page.on('response', lambda response: '/history?' in response.url and pages.append(response))
    open_session(page, ASK)
    read_back_everything(page)
    # The seed puts the page boundary between the question and its answered
    # copy for the pane's page size: the first page read back ends with the
    # unanswered question. If the page size changes, this fails rather than
    # the checks below passing with no boundary to cross.
    first = [e['event'] for e in pages[0].json()['events']] if pages else []
    messages = [e['message']['block'] for e in first if e['kind'] == 'message']
    check('the seeded page boundary falls between a question and its answered copy',
          messages and messages[-1]['kind'] == 'ask' and messages[-1].get('answer') is None,
          messages[-1:] if messages else 'no page was read back')
    asks = page.locator(ASK_BOX)
    check('a question cut from its answered copy shows once',
          asks.count() == 1, f'{asks.count()} question boxes')
    check('the question shows with its answer',
          asks.count() == 1 and asks.first.locator(ASK_ANSWER).count() == 1)
    before = numbers(page, USER_LINE, 'before')
    after = numbers(page, USER_LINE, 'after')
    check('the lines around the question are all there, in order',
          before == list(range(ASK_BEFORE)) and after == list(range(ASK_AFTER)))
    page.context.close()


def check_history(browser, errors):
    page = new_page(browser, errors)
    open_list(page)
    check('the list shows with no session open', list_showing(page))

    scrolled = scroll_list(page, 600)
    check('the list scrolls', scrolled > 0, scrolled)
    visible_row(page).tap()
    page.wait_for_selector(VIEW, state='visible')
    opened = page.evaluate('location.hash')
    check('opening a session covers the list and names the session in the address',
          session_showing(page) and opened.startswith('#s='), opened)

    drop_hidden_list_scroll(page)
    page.tap(BACK)
    page.wait_for_selector(VIEW, state='hidden')
    check('the header back control returns to the list',
          list_showing(page) and page.evaluate('location.hash') == '')
    check('the list keeps its scroll after back', list_scroll(page) == scrolled, list_scroll(page))

    page.go_forward()
    page.wait_for_selector(VIEW, state='visible')
    check('forward reopens the session', page.evaluate('location.hash') == opened and session_showing(page))

    drop_hidden_list_scroll(page)
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    check('the browser back returns to the list with its scroll',
          list_showing(page) and list_scroll(page) == scrolled, list_scroll(page))
    page.context.close()

    # A link opened in a new tab: the pane writes the list under the session,
    # so back stays in the pane.
    page = new_page(browser, errors)
    open_session(page, LONG)
    check('a link to a session opens it', session_showing(page))
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    check('back from a session loaded by its link lands on the list',
          list_showing(page) and page.url.startswith(BASE), page.url)
    page.go_forward()
    page.wait_for_selector(VIEW, state='visible')
    check('forward from there reopens the linked session',
          page.evaluate('location.hash') == '#s=' + quote(LONG) and session_showing(page))
    page.context.close()


def check_widths(browser, errors):
    page = new_page(browser, errors)
    open_session(page, LONG)
    for size in (PORTRAIT, LANDSCAPE):
        label = f'{size[0]}x{size[1]}'
        page.set_viewport_size({'width': size[0], 'height': size[1]})
        result = overflow(page)
        check(f'the session view with wide lines fits the screen at {label}', fits(result), result)

        page.tap(WATCH_CHILD)
        page.wait_for_selector(CHILD_LINE)
        result = overflow(page)
        check(f'the child panel fits the screen at {label}',
              page.is_visible(CHILD_PANEL) and fits(result), result)
        page.tap(CHILD_CLOSE)
        page.wait_for_selector(CHILD_PANEL, state='hidden')

        page.tap(MORE)
        page.wait_for_selector(SHEET, state='visible')
        result = overflow(page)
        check(f'the actions sheet fits the screen at {label}', fits(result), result)
        page.tap(SHEET_CLOSE)
        page.wait_for_selector(SHEET, state='hidden')
    page.context.close()

    page = new_page(browser, errors)
    open_list(page)
    for size in (PORTRAIT, LANDSCAPE):
        label = f'{size[0]}x{size[1]}'
        page.set_viewport_size({'width': size[0], 'height': size[1]})
        result = overflow(page)
        check(f'the list with unbroken summaries fits the screen at {label}', fits(result), result)
    page.context.close()


def check_field_sizes(browser, errors):
    # iOS zooms the page into any field under 16px when it takes the focus.
    sizes = """() => [...document.querySelectorAll('input, textarea, select')]
      .map(e => [e.id || e.name || e.tagName, parseFloat(getComputedStyle(e).fontSize)])
      .filter(([, size]) => size < 16)"""
    page = new_page(browser, errors)
    for size in (PORTRAIT, LANDSCAPE):
        label = f'{size[0]}x{size[1]}'
        page.set_viewport_size({'width': size[0], 'height': size[1]})
        open_list(page)
        small = page.evaluate(sizes)
        check(f'every field on the list is at least 16px at {label}', not small, small)
        open_session(page, LONG)
        small = page.evaluate(sizes)
        check(f'every field in a session is at least 16px at {label}', not small, small)
    page.context.close()


def check_bottom_control(browser, errors):
    page = new_page(browser, errors)
    open_session(page, LONG)
    hidden = f"() => !document.querySelector('{BOTTOM_CONTROL}').checkVisibility()"
    shown = f"() => document.querySelector('{BOTTOM_CONTROL}').checkVisibility()"
    scroll_up = 't => { t.scrollTop = t.scrollHeight / 2; }'
    check('the bottom control is hidden at the bottom',
          until(page, AT_BOTTOM, TRANSCRIPT) and page.is_hidden(BOTTOM_CONTROL))

    page.eval_on_selector(TRANSCRIPT, scroll_up)
    showed = until(page, shown)
    check('the bottom control shows once the reader scrolls up', showed)

    if showed:
        page.tap(BOTTOM_CONTROL)
    check('the bottom control returns the transcript to the bottom',
          showed and until(page, AT_BOTTOM, TRANSCRIPT) and until(page, hidden))

    page.eval_on_selector(TRANSCRIPT, scroll_up)
    until(page, shown)
    page.tap(COMPOSER)
    page.keyboard.type('x')
    check('typing in the composer returns the transcript to the bottom',
          until(page, AT_BOTTOM, TRANSCRIPT) and until(page, hidden))
    page.context.close()


def check_keyboard_ride(browser, errors):
    """With a field focused, the session view covers exactly the visible part
    of the page, however the browser moved that part. Leaving the field gives
    the view the whole screen back at once, before the keyboard reports that
    it has gone."""
    page = new_page(browser, errors, stub_viewport=True)
    open_session(page, LONG)

    def keyboard(visible, offset_top, scroll_y):
        """Moves the visible part and returns whether the view covers it. The
        view is first brought to a different box, so a view still showing
        the previous step cannot pass for this one."""
        page.evaluate('(a) => window.__viewportStub.set(...a)', [600, 0, 0])
        settled, _ = view_covers_visible_part(page)
        page.evaluate('(a) => window.__viewportStub.set(...a)', [visible, offset_top, scroll_y])
        ok, box = view_covers_visible_part(page)
        return settled and ok, box

    page.tap(COMPOSER)
    ok, box = keyboard(500, 0, 344)
    check('a document scroll with the composer focused: the view covers the visible part', ok, box)
    scrolled_back = until(page, DOCUMENT_AT_TOP)
    ok, box = keyboard(500, 344, 0)
    check('a viewport offset with the composer focused: the view covers the visible part', ok, box)
    offset_back = until(page, DOCUMENT_AT_TOP)
    check('with the composer focused, the document is kept at its top: a scroll and an offset are both undone',
          scrolled_back and offset_back, page.evaluate('() => [window.scrollY, window.visualViewport.offsetTop]'))
    ok, box = keyboard(500, 0, 0)
    check('a keyboard that pushes nothing: the view covers the visible part', ok, box)

    # The stub still reports the keyboard, and sends no resize: only leaving
    # the field can give the view its height back here.
    page.evaluate('() => document.activeElement.blur()')
    ok, box = view_covers_screen(page)
    check('leaving the field gives the view the whole screen back', ok, box)
    page.evaluate('() => window.__viewportStub.set(null, 0, 200)')
    check('with no field focused, the document is kept at its top too', until(page, DOCUMENT_AT_TOP),
          page.evaluate('() => window.scrollY'))
    page.evaluate('() => window.__viewportStub.set(null, 0, null)')
    page.context.close()


def check_what_a_closed_session_leaves_behind(browser, errors):
    """Each session opened after another starts from nothing the first one
    left: its scroll, a read-back still in flight, its state, its activity,
    its question and its streamed paragraph."""
    # At the very top the teardown moves no scroll, so no scroll event can
    # re-arm auto-follow on the closing session's behalf.
    page = new_page(browser, errors, visual_viewport=False)
    open_session(page, ASK)
    read_back_everything(page)
    page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = 0; }')
    until(page, shown(BOTTOM_CONTROL))
    leave_session(page)
    open_from_list(page, LONG_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    check('a session opened after one left at its top follows its newest line',
          until(page, AT_BOTTOM, TRANSCRIPT) and until(page, f'() => !({shown(BOTTOM_CONTROL)})()'))
    page.context.close()

    page = new_page(browser, errors)
    open_session(page, LONG)
    page.wait_for_selector(EARLIER)

    # A read-back held until the next session is open, so its page lands there.
    held = []
    page.route('**/history?*', lambda route: held.append(route))
    page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = 0; }')
    for _ in range(50):
        if held:
            break
        page.wait_for_timeout(100)
    leave_session(page)
    open_from_list(page, ASK_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    for route in held:
        with page.expect_response(lambda response: '/history?' in response.url):
            route.continue_()
    page.unroute('**/history?*')
    foreign = f"""() => [...document.querySelectorAll('{USER_LINE}')]
      .some(e => e.textContent.trim().startsWith('user message'))"""
    check('a read-back that lands after its session closed draws nothing',
          held and not until(page, foreign, timeout=1500), f'{len(held)} read-backs held')

    # The label counts every second, not only when the session list is polled.
    leave_session(page)
    open_from_list(page, RUNNING_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    page.evaluate("""(sel) => {
      const label = document.querySelector(sel);
      window.__labels = new Set();
      new MutationObserver(() => {
        if (/· \\d+s$/.test(label.textContent)) window.__labels.add(label.textContent);
      }).observe(label, { childList: true, characterData: true, subtree: true });
    }""", STATUS_LABEL)
    check('a running session counts the seconds since its newest activity',
          until(page, '() => window.__labels.size >= 3', timeout=5000),
          page.evaluate('() => [...window.__labels]'))

    # The session list reports a state that no stream event carried. The real
    # server sends a state event with each change, so this rewrite forces a
    # race between the list and the stream in the list's favour, which runs
    # the list-poll path in session-list.js. The label's one-second count must
    # follow the list, not the state the session opened with.
    def stopped(route):
        response = route.fetch()
        listed = response.json()
        for session in listed:
            if session['id'] == RUNNING:
                session['state'] = 'waiting_for_input'
        route.fulfill(response=response, json=listed)
    page.route('**/sessions', stopped)
    page.wait_for_function(f"() => document.querySelector('{STATUS_LABEL}').hidden", timeout=5000)
    check('the header follows a state the session list reports',
          not until(page, f"() => !document.querySelector('{STATUS_LABEL}').hidden", timeout=2500))
    page.unroute('**/sessions')

    leave_session(page)
    open_from_list(page, ASK_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    page.tap(VIEW_TITLE)
    page.wait_for_selector('#activity-log', state='visible')
    rows = page.locator(ACTIVITY_ROW).count()
    check('the activity console holds none of the last session\'s activity', rows == 0, f'{rows} rows')
    page.tap(VIEW_TITLE)

    # The composer holds the focus when a session opens, and the ask composer
    # waits for it to leave. The pending question has the same text and child
    # as the ask session's, so a record of it left behind would take the ask
    # session's answered copy for its own answer.
    leave_session(page)
    open_from_list(page, PENDING_ASK_ROW)
    page.wait_for_selector(ASK_BOX)
    page.evaluate('() => document.activeElement.blur()')
    check('the ask composer shows for a pending question', until(page, shown(ASK_COMPOSER)))

    leave_session(page)
    open_from_list(page, EMPTY_ROW)
    page.evaluate('() => document.activeElement.blur()')
    check('a session with no question shows the chat box, not the last session\'s question',
          not until(page, shown(ASK_COMPOSER), timeout=1500) and page.is_visible(CHAT_ROW))

    leave_session(page)
    open_from_list(page, ASK_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    check('a session opens with its own answered question, whatever the last session asked',
          until(page, f"() => !!document.querySelector('{ASK_BOX} {ASK_ANSWER}')"))

    # A stream that sends one delta and ends: the streamed paragraph is the
    # session's, and the next session's first reply does not replace it.
    page.route('**/sessions/empty-session/events*', lambda route: route.fulfill(
        status=200, content_type='text/event-stream', body='data: {"delta": "streamed words"}\n\n'))
    leave_session(page)
    open_from_list(page, EMPTY_ROW)
    streamed = until(page, f"""() => [...document.querySelectorAll('{REPLY_LINE}')]
      .some(e => e.textContent.includes('streamed words'))""")
    leave_session(page)
    page.unroute('**/sessions/empty-session/events*')
    open_from_list(page, DIAGRAM_ROW)
    check('a session opens with its own first reply after one that was streaming',
          streamed and until(page, f"""() => [...document.querySelectorAll('{REPLY_LINE}')]
            .some(e => e.textContent.includes('Here it is'))"""), f'streamed: {streamed}')
    page.context.close()


def check_diagram(browser, errors):
    page = new_page(browser, errors)
    open_session(page, DIAGRAM)
    check('a mermaid fence draws as a diagram', until(page, shown(DIAGRAM_DRAWN), timeout=20000))
    page.context.close()


def sse(*frames, retry=600000):
    """An event-stream body carrying `frames`. The long retry keeps the
    browser from replaying the body while a check reads what it drew."""
    return f'retry: {retry}\n\n' + ''.join('data: ' + json.dumps(frame) + '\n\n' for frame in frames)


def staged_stream(page, session_id, first, later):
    """Answers the session's event stream with `first` at once, and holds the
    reconnect that follows it. The returned function answers the held
    reconnect with `later`, so a check can act on the screen in between."""
    held = []
    served = [0]

    def handle(route):
        served[0] += 1
        if served[0] == 1:
            body = sse(*first, retry=50)
        elif served[0] == 2:
            held.append(route)
            return
        else:
            body = sse()
        route.fulfill(status=200, content_type='text/event-stream', body=body)
    page.route(f'**/sessions/{session_id}/events*', handle)

    def release():
        wait_for(page, lambda: held)
        held[0].fulfill(status=200, content_type='text/event-stream', body=sse(*later))
    return release


def message_frame(role, block, at_ms=None):
    event = {'kind': 'message', 'message': {'role': role, 'block': block}}
    if at_ms is not None:
        event['at_ms'] = at_ms
    return {'event': event}


def route_stream(page, session_id, *frames):
    """Answers the session's event stream with `frames` instead of the store's."""
    page.route(f'**/sessions/{session_id}/events*', lambda route: route.fulfill(
        status=200, content_type='text/event-stream', body=sse(*frames)))


def wait_for(page, condition, timeout=5000):
    """Whether the Python `condition` becomes true within `timeout` ms."""
    for _ in range(timeout // 100):
        if condition():
            return True
        page.wait_for_timeout(100)
    return condition()


def wait_for_poll(page):
    """Waits for the session list's next poll to answer and be drawn."""
    try:
        with page.expect_response(lambda r: r.url.endswith('/sessions'), timeout=6000):
            pass
    except PlaywrightTimeout:
        return False
    page.wait_for_timeout(300)
    return True


def texts(page, selector):
    return page.eval_on_selector_all(selector, 'els => els.map(e => e.textContent)')


def focused_id(page):
    return page.evaluate('() => document.activeElement ? document.activeElement.id : null')


def rect(page, selector):
    return page.eval_on_selector(
        selector, 'e => { const r = e.getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; }')


def check_list(browser, errors):
    page = new_page(browser, errors)
    open_list(page)
    item = page.locator('.session-item', has=page.locator(SESSION_ROW, has_text=LONG_ROW)).first
    lines = item.locator('.row-main > *')
    lead = [lines.nth(0).inner_text(), lines.nth(1).inner_text()]
    check('a summarized session row leads with its summary, with the node and directory under it',
          lead == [LONG_ROW, 'node-1 / /work/repo'], lead)

    toggle = item.locator('.children-toggle')
    child_line = page.locator('#session-list .child-row', has_text=CHILD[:8])
    toggle.tap()
    polled = wait_for_poll(page)
    check('an opened children group stays open when the list refreshes',
          polled and child_line.is_visible() and toggle.inner_text() == 'hide children')
    toggle.tap()
    polled = wait_for_poll(page)
    check('a closed children group stays closed when the list refreshes',
          polled and child_line.is_hidden() and toggle.inner_text() == '1 child')

    # The panel's child rows have rules of their own, which must not reach the
    # session list's child lines. The wide screen has no narrow-screen minimum.
    page.set_viewport_size({'width': LANDSCAPE[0], 'height': LANDSCAPE[1]})
    toggle.tap()
    look = child_line.evaluate('e => { const s = getComputedStyle(e); return [s.display, s.paddingLeft, s.minHeight]; }')
    check('a child line in the session list keeps its own look, not the panel row\'s',
          look[:2] == ['flex', '26px'] and look[2] in ('auto', '0px'), look)
    page.context.close()


def check_stamps(browser, errors):
    zone = 'Asia/Kolkata'
    page = new_page(browser, errors, locale='en-US', timezone_id=zone)
    open_session(page, KINDS)
    page.wait_for_selector(f'{REPLY_LINE} >> text=kinds end')
    events = page.request.get(f'{BASE}/sessions/{KINDS}/history?before=1000000000&messages=200').json()['events']
    first = next(e['event'] for e in events
                 if e['event']['kind'] == 'message' and e['event']['message']['block'].get('text') == 'kinds start')
    local = datetime.fromtimestamp(first['at_ms'] / 1000, ZoneInfo(zone))
    shown_time = page.evaluate("""(sel) => {
      const line = [...document.querySelectorAll(sel)].find(e => e.textContent.trim() === 'kinds start');
      const row = line.closest('.stamp-row');
      return row ? row.querySelector('.ts').textContent : null;
    }""", USER_LINE)
    match = re.fullmatch(r'(\d{1,2}):(\d\d):(\d\d)', shown_time or '')
    check('a line\'s time is the reader\'s local time on a 00-23 clock',
          match and tuple(int(g) for g in match.groups()) == (local.hour, local.minute, local.second),
          f'{shown_time} for {local:%H:%M:%S}')

    unstamped = page.evaluate("""(sel) => [...document.querySelector(sel).children]
      .filter(e => !(e.classList.contains('stamp-row') && e.querySelector(':scope > .ts')))
      .map(e => e.className || e.tagName)""", TRANSCRIPT)
    check('every durable entry carries its time, except a warning, whose event has none',
          unstamped == ['line warning'], unstamped)

    layout = page.evaluate("""(sel) => {
      const line = [...document.querySelectorAll(sel)].find(e => e.textContent.trim() === 'kinds start');
      const row = line.closest('.stamp-row').getBoundingClientRect();
      const ts = line.closest('.stamp-row').querySelector('.ts').getBoundingClientRect();
      const entry = line.getBoundingClientRect();
      return { tsRight: ts.right, tsTop: ts.top, entryLeft: entry.left, entryTop: entry.top,
               entryRight: entry.right, rowRight: row.right };
    }""", USER_LINE)
    check('the time stands in a column beside its entry, on its first line, and the entry takes the rest of the row',
          layout['tsRight'] <= layout['entryLeft'] + 0.5
          and abs(layout['entryRight'] - layout['rowRight']) <= 1
          and abs(layout['tsTop'] - layout['entryTop']) <= 16, layout)

    # A message whose event carries no stamp, then a streamed paragraph.
    route_stream(page, EMPTY,
                 message_frame('user', {'kind': 'text', 'text': 'no stamp'}),
                 {'delta': 'streaming words'})
    leave_session(page)
    open_from_list(page, EMPTY_ROW)
    page.wait_for_selector(f'{REPLY_LINE} >> text=streaming words')
    placed = page.evaluate("""([user, reply]) => {
      const bare = [...document.querySelectorAll(user)].find(e => e.textContent === 'no stamp');
      const live = [...document.querySelectorAll(reply)].find(e => e.textContent === 'streaming words');
      const alone = (e) => !!e && e.parentElement.id === 'transcript' && !e.closest('.stamp-row');
      return [alone(bare), alone(live), document.querySelectorAll('#transcript .ts').length];
    }""", [USER_LINE, REPLY_LINE])
    check('an entry whose event has no time draws with no time column', placed[0] and placed[2] == 0, placed)
    check('a streamed paragraph carries no time', placed[1] and placed[2] == 0, placed)
    page.unroute(f'**/sessions/{EMPTY}/events*')
    page.context.close()


def check_kinds(browser, errors):
    page = new_page(browser, errors)
    # From the list, so the child's line is drawn with the child's name.
    open_list(page)
    open_from_list(page, KINDS_ROW)
    page.wait_for_selector(f'{REPLY_LINE} >> text=kinds end')
    lines = texts(page, '#transcript .line.mono')
    check('a model call draws as one line with its counts and its cost',
          lines == ['test-model completion (10 in, 3 cached, 5 out, $0.5000)'], lines)
    lines = texts(page, '#transcript .line.context')
    check('a context size note states the count, the window, the percentage and the compaction point',
          lines == ['context: 50000 / 100000 tokens (50%), compaction at 80000'], lines)
    lines = texts(page, '#transcript .line.cleared')
    border = page.eval_on_selector('#transcript .line.cleared', 'e => getComputedStyle(e).borderTopStyle')
    check('a cleared context draws as a break that names its reason and not the fresh instructions',
          len(lines) == 1 and 'starting over' in lines[0] and 'fresh instructions' not in page.inner_text(TRANSCRIPT)
          and border == 'dashed', [lines, border])

    tables = page.evaluate("""() => [...document.querySelectorAll('#transcript table')].map(t => ({
      head: [...t.querySelectorAll('th')].map(c => [c.textContent, getComputedStyle(c).textAlign]),
      rows: [...t.querySelectorAll('tr')].filter(r => r.querySelector('td'))
        .map(r => [...r.querySelectorAll('td')].map(c => [c.textContent, getComputedStyle(c).textAlign])),
    }))""")
    aligned = next((t for t in tables if t['head'] and t['head'][0][0] == 'left'), None)
    check('a table draws each column with the alignment its delimiter row asks for, and the delimiter row is no row',
          aligned is not None
          and [c[0] for c in aligned['head']] == ['left', 'centre', 'right']
          and aligned['head'][1][1] == 'center' and aligned['head'][2][1] == 'right'
          and len(aligned['rows']) == 1
          and [c[0] for c in aligned['rows'][0]] == ['l', 'c', 'r']
          and aligned['rows'][0][0][1] in ('start', 'left')
          and aligned['rows'][0][1][1] == 'center' and aligned['rows'][0][2][1] == 'right', tables)
    prose = page.evaluate("""() => [...document.querySelectorAll('#transcript .msg.assistant p')]
      .some(p => p.textContent === 'a | b')""")
    check('a lone pipe in prose stays prose', prose and len(tables) == 2, len(tables))
    holder = page.evaluate("""() => {
      const wrap = [...document.querySelectorAll('#transcript .md-table-wrap')]
        .find(w => w.querySelector('th') && w.querySelector('th').textContent === 'wide');
      return [wrap.scrollWidth > wrap.clientWidth, getComputedStyle(wrap).overflowX];
    }""")
    result = overflow(page)
    check('a wide table scrolls sideways inside its own holder, and the page does not',
          holder == [True, 'auto'] and fits(result), [holder, result])
    cells = page.evaluate("""() => {
      const th = document.querySelector('#transcript th'), td = document.querySelector('#transcript td');
      const s = (e) => getComputedStyle(e);
      return [s(th).borderTopStyle, s(th).borderTopWidth, s(td).borderTopStyle,
              s(th).backgroundColor !== s(td).backgroundColor];
    }""")
    check('a table\'s cells are ruled and its header row is shaded', cells == ['solid', '1px', 'solid', True], cells)

    results = page.evaluate("""() => [...document.querySelectorAll('#transcript .tool-result')]
      .map(r => [r.textContent, !!r.querySelector('.child-watch')])""")
    by_text = {text.replace('watch', ''): watched for text, watched in results}
    check('a message_child result carries the watch control of the child its call named',
          by_text.get('ok: true') is True, results)
    check('a failed spawn, and another tool\'s result that carries a child id, carry no watch control',
          by_text.get('spawn failedno node') is False
          and any(t.startswith('child_id: ') and w is False for t, w in by_text.items()), results)
    link = page.eval_on_selector('#transcript .line.child-report .child-link', 'e => [e.textContent, e.title]')
    watch = page.locator('#transcript .line.child-report .child-watch').count()
    check('a child\'s line leads with its name, keeps its id in the tooltip, and carries the watch control',
          link[0] == CHILD_NAME and link[1].startswith(CHILD) and watch == 1, [link, watch])

    leave_session(page)
    open_from_list(page, REUSE_ROW)
    page.wait_for_selector('#transcript .tool-result')
    check('a call the last session left unanswered names no child in the next session\'s result',
          page.locator('#transcript .tool-result .child-watch').count() == 0)

    leave_session(page)
    open_from_list(page, ASK_ROW)
    page.wait_for_selector(ASK_BOX)
    check('a child\'s question carries the watch control', page.locator(f'{ASK_BOX} .child-watch').count() == 1)

    leave_session(page)
    open_from_list(page, RUNNING_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    page.tap(VIEW_TITLE)
    page.wait_for_selector('#activity-log', state='visible')
    labels = texts(page, f'{ACTIVITY_ROW} .activity-label')
    check('the activity console lists the session\'s activity', labels == ['working'], labels)
    page.context.close()


def check_panel(browser, errors):
    page = new_page(browser, errors, size=LANDSCAPE)
    open_session(page, LONG)
    check('the panel is closed until a child is followed', page.is_hidden(CHILD_PANEL))
    entries = page.evaluate('[history.length, location.hash]')
    page.tap(WATCH_CHILD)
    page.wait_for_selector(CHILD_LINE)
    streams = page.evaluate('window.__openStreams()')
    check('following a child opens its own stream beside the session\'s',
          len(streams) == 2 and any(url.endswith(f'/sessions/{CHILD}/events') for url in streams), streams)
    check('following a child writes no history entry', page.evaluate('[history.length, location.hash]') == entries)
    rows = page.evaluate("""() => [...document.querySelectorAll('#child-list .child-row')].map(r => ({
      followed: r.classList.contains('followed'), dot: r.querySelector('.dot').className,
      name: r.querySelector('.child-row-name').textContent, id: r.querySelector('.child-row-id').textContent,
      title: r.title }))""")
    check('the panel lists the session\'s children with their state, name and short id, the followed one marked',
          rows == [{'followed': True, 'dot': 'dot waiting_for_input', 'name': CHILD_NAME,
                    'id': CHILD[:8], 'title': CHILD}], rows)
    box = page.evaluate("""() => {
      const r = (sel) => document.querySelector(sel).getBoundingClientRect();
      const panel = r('#child-panel'), wrap = r('#transcript-wrap'), row = r('#conversation');
      return { panelLeft: panel.left, panelWidth: panel.width, wrapRight: wrap.right,
               wrapWidth: wrap.width, rowWidth: row.width };
    }""")
    check('on a wide screen the panel is a column beside the transcript, at most 420px and 45% wide',
          box['panelLeft'] >= box['wrapRight'] - 1 and box['wrapWidth'] > 0
          and box['panelWidth'] <= 420.5 and box['panelWidth'] <= box['rowWidth'] * 0.45 + 0.5, box)
    check('the panel follows its child\'s newest line', until(page, AT_BOTTOM, '#child-transcript'))
    # The panel sets a smaller type size on its transcript, so a line whose
    # size follows its transcript's is compared as a share of it.
    diffs = page.evaluate("""() => {
      const props = ['color', 'fontFamily', 'fontStyle', 'paddingTop', 'paddingLeft', 'marginTop',
                     'borderTopStyle', 'backgroundColor', 'whiteSpace', 'textAlign', 'display'];
      const size = (e, box) => (parseFloat(getComputedStyle(e).fontSize) / parseFloat(getComputedStyle(box).fontSize)).toFixed(3);
      const session = document.querySelector('#transcript'), panel = document.querySelector('#child-transcript');
      const diffs = [];
      for (const sel of ['.msg.user', '.msg.assistant', '.stamp-row', '.ts', 'pre']) {
        const a = session.querySelector(sel), b = panel.querySelector(sel);
        if (!a || !b) { diffs.push(sel + ' missing'); continue; }
        const sa = getComputedStyle(a), sb = getComputedStyle(b);
        for (const p of props) if (sa[p] !== sb[p]) diffs.push(`${sel} ${p}: ${sa[p]} / ${sb[p]}`);
        if (sa.fontSize !== sb.fontSize && size(a, session) !== size(b, panel)) diffs.push(`${sel} size: ${size(a, session)} / ${size(b, panel)}`);
      }
      return diffs;
    }""")
    check('a line in the panel draws like the same line in the session\'s transcript', not diffs, diffs)

    page.set_viewport_size({'width': PORTRAIT[0], 'height': PORTRAIT[1]})
    check('on a phone the panel covers the view as a sheet',
          rect(page, CHILD_PANEL) == [0, 0, PORTRAIT[0], PORTRAIT[1]], rect(page, CHILD_PANEL))
    page.tap(CHILD_CLOSE)
    page.wait_for_selector(CHILD_PANEL, state='hidden')
    streams = page.evaluate('window.__openStreams()')
    check('collapsing the panel closes the child\'s stream and keeps the session\'s',
          len(streams) == 1 and not any(CHILD in url for url in streams), streams)

    page.tap(WATCH_CHILD)
    page.wait_for_selector(CHILD_LINE)
    # The panel covers the view on a phone, so the browser's back leaves.
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    streams = page.evaluate('window.__openStreams()')
    open_from_list(page, LONG_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    check('leaving a session closes its panel and the child\'s stream',
          streams == [] and page.is_hidden(CHILD_PANEL) and not page.locator(CHILD_LINE).count(), streams)

    # One child at a time: the kinds session names two. The panel is a column
    # here, so the transcript's controls stay in reach.
    page.set_viewport_size({'width': LANDSCAPE[0], 'height': LANDSCAPE[1]})
    leave_session(page)
    open_from_list(page, KINDS_ROW)
    page.wait_for_selector(f'{REPLY_LINE} >> text=kinds end')
    page.tap('#transcript .line.child-report .child-watch')
    page.wait_for_selector(CHILD_LINE)
    page.locator('#transcript .tool-result .child-watch').first.tap()
    replaced = until(page, "(id) => document.querySelector('#child-panel-title').title === id", ASK_CHILD)
    streams = page.evaluate('window.__openStreams()')
    check('following another child replaces the one followed, on one stream',
          replaced and len(streams) == 2 and any(url.endswith(f'/sessions/{ASK_CHILD}/events') for url in streams)
          and not page.locator(CHILD_LINE).count(), streams)

    # The child's name opens it as the session view, which takes the session's
    # entry: back goes to the list.
    page.tap(CHILD_CLOSE)
    page.tap('#transcript .child-link')
    opened = until(page, "(hash) => location.hash === hash", '#s=' + quote(CHILD))
    page.wait_for_selector('#watch-banner', state='visible')
    check('a child\'s name in the transcript opens it as the session view, watch-only', opened)
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    check('back from a session opened from a session lands on the list',
          list_showing(page) and page.evaluate('location.hash') == '')
    page.context.close()


def check_panel_frames(browser, errors):
    page = new_page(browser, errors)
    route_stream(page, CHILD,
                 {'event': {'kind': 'state', 'state': 'running', 'at_ms': 1}},
                 {'event': {'kind': 'activity', 'phase': 'wake_started', 'at_ms': 1}},
                 message_frame('assistant', {'kind': 'ask', 'message': 'Child question?', 'options': ['a', 'b']}, 1),
                 message_frame('assistant', {'kind': 'ask', 'message': 'Child question?', 'options': ['a', 'b'],
                                             'answer': 'a'}, 2))
    open_session(page, LONG)
    page.tap(VIEW_TITLE)
    page.wait_for_selector('#activity-log', state='visible')
    before = page.evaluate(f"""() => [document.querySelector('#view-state-dot').className,
      document.querySelector('{STATUS_LABEL}').textContent,
      document.querySelectorAll('{ACTIVITY_ROW}').length,
      document.querySelectorAll('#transcript > *').length]""")
    page.tap(WATCH_CHILD)
    page.wait_for_selector('#child-transcript .ask .answer')
    asks = page.eval_on_selector_all('#child-transcript .ask', 'els => els.map(e => e.querySelectorAll(".answer").length)')
    check('a child\'s question and its answered copy are one box in the panel', asks == [1], asks)
    after = page.evaluate(f"""() => [document.querySelector('#view-state-dot').className,
      document.querySelector('{STATUS_LABEL}').textContent,
      document.querySelectorAll('{ACTIVITY_ROW}').length,
      document.querySelectorAll('#transcript > *').length]""")
    check('a child\'s frames leave the session\'s header, console and transcript alone', after == before,
          [before, after])
    page.context.close()

    page = new_page(browser, errors)
    naming = {}

    def rename(route):
        response = route.fetch()
        listed = response.json()
        for session in listed:
            if session['id'] == CHILD:
                session.update(naming)
        route.fulfill(response=response, json=listed)
    page.route('**/sessions', rename)
    open_session(page, LONG)
    page.tap(WATCH_CHILD)
    page.wait_for_selector(CHILD_LINE)
    for rule, given, name in [
        ('by its summary', {'summary': 'the child summary'}, 'the child summary'),
        ('by its summary, cut to one row', {'summary': 'x' * 80}, 'x' * 60 + '…'),
        ('without one, by the first line of its instructions that is not blank',
         {'summary': None, 'prompt': '\n   \n  first line  \nsecond line'}, 'first line'),
        ('with neither, by its id', {'summary': None, 'prompt': None}, CHILD),
    ]:
        naming.clear()
        naming.update(given)
        named = until(page, "(name) => document.querySelector('#child-panel-title').textContent === name", name,
                      timeout=7000)
        check(f'a child is named {rule}',
              named and page.eval_on_selector('#child-panel-title', 'e => e.title') == CHILD,
              page.inner_text('#child-panel-title'))
    page.unroute('**/sessions')
    page.context.close()


def check_addresses(browser, errors):
    page = new_page(browser, errors)
    page.goto(f'{BASE}/#s=%E0%A4%A')
    page.wait_for_function("""([sel, n]) => document.querySelectorAll(sel).length >= n""", arg=[SESSION_ROW, LIST_ROWS])
    check('an address whose session id does not decode shows the list', list_showing(page))
    page.context.close()

    # Safari copies the entry's state onto an address pasted over it, so the
    # new entry carries another session's state. Chromium does not, so the
    # check writes that entry itself, and a load opens it.
    page = new_page(browser, errors)
    open_session(page, LONG)
    page.evaluate("(id) => history.pushState(history.state, '', '#s=' + encodeURIComponent(id))", ASK)
    page.reload()
    moved = until(page, """(sel) => [...document.querySelectorAll(sel)]
      .some(e => e.textContent.trim().startsWith('after'))""", USER_LINE)
    page.go_back()
    check('an address that carries another session\'s entry opens its own session, with the list under it',
          moved and until(page, f"() => document.querySelector('{VIEW}').hidden && location.hash === ''")
          and list_showing(page))
    page.context.close()

    page = new_page(browser, errors)
    open_list(page)
    open_from_list(page, LONG_ROW)
    page.wait_for_selector(TRANSCRIPT_LINE)
    length = page.evaluate('history.length')
    page.reload()
    page.wait_for_selector(TRANSCRIPT_LINE)
    kept = session_showing(page) and page.evaluate('history.length') == length
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    check('a reload keeps the open session and adds no entry', kept and list_showing(page))
    page.context.close()

    closed = "() => location.hash === '' && document.querySelector('#session-view').hidden"
    page = new_page(browser, errors, http_errors=True)
    page.on('dialog', lambda dialog: dialog.accept())
    open_list(page)
    page.route(f'**/sessions/{EMPTY}', lambda route: route.fulfill(status=404, body='no such session'))
    page.locator(SESSION_ROW, has_text=EMPTY_ROW).first.tap()
    check('a session the control plane no longer has closes, and the address stops naming it',
          until(page, closed) and list_showing(page) and 'ended' in page.inner_text('#status'))
    page.unroute(f'**/sessions/{EMPTY}')

    open_from_list(page, EMPTY_ROW)

    def without(route):
        response = route.fetch()
        route.fulfill(response=response, json=[s for s in response.json() if s['id'] != EMPTY])
    page.route('**/sessions', without)
    check('a session the list no longer shows closes, and the address stops naming it',
          until(page, closed, timeout=7000) and list_showing(page))
    page.unroute('**/sessions')

    page.route('**/stop', lambda route: route.fulfill(status=200, content_type='application/json', body='{}'))
    open_from_list(page, EMPTY_ROW)
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.tap('#btn-stop')
    check('a stopped session closes, and the address stops naming it', until(page, closed) and list_showing(page))
    page.unroute('**/stop')

    # A detail reply held until the pane has moved on to another session.
    for late, what in [('body', 'a late reply for a session the pane left leaves the header alone'),
                       ('404', 'a late "no such session" for a session the pane left closes nothing'),
                       ('abort', 'a late failure for a session the pane left writes nothing to the status line')]:
        held = []
        page.route(f'**/sessions/{ASK}', lambda route: held.append(route))
        open_from_list(page, ASK_ROW)
        wait_for(page, lambda: held)
        leave_session(page)
        open_from_list(page, EMPTY_ROW)
        for route in held:
            if late == 'body':
                response = route.fetch()
                body = response.json()
                body.update({'node': 'other-node', 'dir': '/elsewhere'})
                route.fulfill(response=response, json=body)
            elif late == '404':
                route.fulfill(status=404, body='no such session')
            else:
                route.abort()
        page.unroute(f'**/sessions/{ASK}')
        page.wait_for_timeout(800)
        header = page.evaluate("() => [document.querySelector('#view-node').textContent, document.querySelector('#view-dir').textContent]")
        status = page.inner_text('#status')
        check(what, held and session_showing(page) and page.evaluate('location.hash') == '#s=' + quote(EMPTY)
              and header == ['node-1', '/work/repo'] and not status.startswith('session: '),
              [len(held), header, status])
        leave_session(page)
    page.context.close()


def check_leaving_clears_the_view(browser, errors):
    # The fork's route is held and then aborted, so the browser logs the failed
    # request it was made to fail: this page declares its own error responses.
    page = new_page(browser, errors, http_errors=True)
    # A request the reader walks away from must not hold the next screen's
    # control or leave its note there. The fork's request is held open, so
    # nothing but the teardown can give the control back: the handler's own
    # re-enable belongs to the screen the request was made for.
    held = []
    page.route('**/fork', lambda route: held.append(route))
    open_session(page, LONG)
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.tap('#btn-fork')
    check('a fork in flight turns the control off and names what it waits for',
          until(page, "() => document.querySelector('#view-fork').textContent === 'forking…'")
          and page.is_disabled('#btn-fork'), page.inner_text('#view-fork'))
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    open_from_list(page, LONG_ROW)
    left_behind = page.evaluate("""() => {
      const $ = (id) => document.getElementById(id);
      return {
        disabled: [$('btn-fork').disabled, $('btn-clear').disabled],
        notes: [$('view-fork').textContent, $('view-clear').textContent],
      };
    }""")
    check('a request left behind holds no control and leaves no note on the next session',
          left_behind == {'disabled': [False, False], 'notes': ['', '']}, left_behind)
    for route in held:
        route.abort()
    page.unroute('**/fork')

    open_session(page, CHILD)
    page.wait_for_selector('#watch-banner', state='visible')
    hidden = page.evaluate("""() => ['input-row', 'row-permission', 'row-persona', 'row-fork', 'row-clear', 'row-interrupt', 'row-stop']
      .map(id => document.getElementById(id).hidden)""")
    check('a watch-only child has no chat box and no controls in its sheet', all(hidden), hidden)
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    state = page.evaluate("""() => {
      const $ = (id) => document.getElementById(id);
      return {
        sheets: [$('view-sheet').hidden, $('ask-sheet').hidden],
        text: ['view-node', 'view-dir', 'view-id-copy', 'view-sheet-meta', 'view-waiting', 'view-permission', 'view-fork', 'view-clear']
          .map(id => $(id).textContent).join(''),
        waiting: $('view-waiting').hidden,
        footer: [$('input-row').hidden, $('watch-banner').hidden],
        rows: ['row-permission', 'row-persona', 'row-fork', 'row-clear', 'row-interrupt', 'row-stop'].map(id => $(id).hidden),
        permission: $('btn-permission').textContent,
        dot: $('view-state-dot').className,
        persona: $('persona-name').value,
      };
    }""")
    check('leaving a session clears its header and sheet, closes its sheets and gives the footer back', state == {
        'sheets': [True, True], 'text': '', 'waiting': True, 'footer': [False, True],
        'rows': [False] * 6, 'permission': 'Switch to read-only', 'dot': 'dot', 'persona': '',
    }, state)
    page.context.close()


def check_composer(browser, errors):
    for touch in (True, False):
        label = 'a touch screen' if touch else 'a fine pointer'
        page = new_page(browser, errors, touch=touch)
        press = page.tap if touch else page.click
        posts = []

        def sent(route):
            posts.append(route.request.post_data_json)
            route.fulfill(status=200, content_type='application/json', body='{}')
        page.route('**/messages', sent)
        open_session(page, LONG)
        check(f'opening a session focuses the chat box on {label}',
              until(page, "() => document.activeElement && document.activeElement.id === 'input'"))

        page.fill(COMPOSER, 'hello')
        page.evaluate('() => document.activeElement.blur()')
        page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = t.scrollHeight / 2; }')
        until(page, shown(BOTTOM_CONTROL))
        press('#btn-send')
        delivered = wait_for(page, lambda: posts) and posts[0].get('content') == 'hello'
        check(f'sending returns the transcript to the bottom on {label}',
              delivered and until(page, AT_BOTTOM, TRANSCRIPT)
              and until(page, "(sel) => document.querySelector(sel).value === ''", COMPOSER), posts)
        if touch:
            check('on a touch screen, sending leaves the focus where the tap put it', focused_id(page) != 'input')
        else:
            check('on a fine pointer, sending gives the focus back to the chat box',
                  until(page, "() => document.activeElement.id === 'input'"))

        page.fill(COMPOSER, 'again')
        page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = t.scrollHeight / 2; }')
        until(page, shown(BOTTOM_CONTROL))
        page.focus(COMPOSER)
        page.keyboard.press('Control+Enter')
        check(f'Ctrl+Enter sends the chat box on {label}',
              wait_for(page, lambda: len(posts) == 2) and posts[1].get('content') == 'again'
              and until(page, AT_BOTTOM, TRANSCRIPT), posts)

        page.evaluate('() => document.activeElement.blur()')
        page.dispatch_event('#input-row', 'click')
        check(f'a tap on the chat box\'s row focuses the field on {label}', focused_id(page) == 'input')

        page.goto(f'{BASE}/#s={quote(PENDING_ASK)}')
        page.wait_for_selector(ASK_BOX)
        if touch:
            check('the ask composer waits while the chat box holds the focus',
                  not until(page, shown(ASK_COMPOSER), timeout=1000) and page.is_visible(CHAT_ROW)
                  and focused_id(page) == 'input')
        page.evaluate('() => document.activeElement.blur()')
        until(page, shown(ASK_COMPOSER))
        press('#btn-ask-type')
        page.wait_for_selector('#ask-free', state='visible')
        if touch:
            check('on a touch screen, answering in your own words leaves the focus where the tap put it',
                  focused_id(page) != 'ask-input')
        else:
            check('on a fine pointer, answering in your own words focuses the answer field',
                  until(page, "() => document.activeElement.id === 'ask-input'"))

        page.goto(f'{BASE}/#s={quote(OPEN_ASK)}')
        page.wait_for_selector(ASK_BOX)
        page.evaluate('() => document.activeElement.blur()')
        page.wait_for_selector('#ask-free', state='visible')
        if touch:
            check('on a touch screen, a question with no options does not take the focus',
                  not until(page, "() => document.activeElement.id === 'ask-input'", timeout=1000))
        else:
            check('on a fine pointer, a question with no options focuses its answer field',
                  until(page, "() => document.activeElement.id === 'ask-input'"))
        page.context.close()

    page = new_page(browser, errors)
    route_stream(page, EMPTY, *[{'delta': f'streamed line {i}\n'} for i in range(200)])
    open_list(page)
    open_from_list(page, EMPTY_ROW)
    grew = until(page, "(sel) => { const t = document.querySelector(sel); return t.scrollHeight > t.clientHeight * 2; }",
                 TRANSCRIPT)
    check('a transcript at its newest line follows the lines that arrive', grew and until(page, AT_BOTTOM, TRANSCRIPT))
    page.context.close()


def check_layout(browser, errors):
    page = new_page(browser, errors)
    open_session(page, LONG)
    page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = t.scrollHeight / 2; }')
    until(page, shown(BOTTOM_CONTROL))
    place = page.evaluate("""() => {
      const r = (sel) => document.querySelector(sel).getBoundingClientRect();
      const b = r('#btn-bottom'), t = r('#transcript'), c = r('#input-row');
      return { w: b.width, h: b.height, right: t.right - b.right, above: c.top - b.bottom,
               inside: b.top >= t.top && b.left >= t.left };
    }""")
    check('the bottom control floats in the transcript\'s lower corner above the chat box, at thumb size',
          place['w'] >= 44 and place['h'] >= 44 and 0 <= place['right'] <= 32 and 0 <= place['above'] <= 32
          and place['inside'], place)
    page.context.close()

    hidden_shown = """() => [...document.querySelectorAll('[hidden]')]
      .filter(e => getComputedStyle(e).display !== 'none').map(e => e.id || e.className || e.tagName)"""
    page = new_page(browser, errors)
    for size in (PORTRAIT, LANDSCAPE):
        label = f'{size[0]}x{size[1]}'
        page.set_viewport_size({'width': size[0], 'height': size[1]})
        open_list(page)
        on_list = page.evaluate(hidden_shown)
        open_session(page, LONG)
        in_session = page.evaluate(hidden_shown)
        check(f'every hidden element is off the screen at {label}', not on_list and not in_session,
              [on_list, in_session])

    page.set_viewport_size({'width': PORTRAIT[0], 'height': PORTRAIT[1]})
    open_list(page)
    doc = page.evaluate('() => [document.scrollingElement.scrollHeight, window.innerHeight]')
    screens = []
    for strip, tab, listing, back in [
        ('#health-strip', '#machines-tab', '#machines-list', '#btn-machines-back'),
        ('#skills-strip', '#skills-tab', '#skills-list', '#btn-skills-back'),
        ('#mcp-strip', '#mcp-tab', '#mcp-list', '#btn-mcp-back'),
    ]:
        page.tap(strip)
        page.wait_for_selector(tab, state='visible')
        screens.append(page.evaluate("""([tab, listing]) => {
          const t = document.querySelector(tab).getBoundingClientRect();
          const l = document.querySelector(listing);
          return [Math.round(t.bottom) <= window.innerHeight, getComputedStyle(l).overflowY,
                  Math.round(l.getBoundingClientRect().bottom) <= window.innerHeight];
        }""", [tab, listing]))
        page.tap(back)
        page.wait_for_selector(tab, state='hidden')
    check('the document never scrolls, and each tab scrolls its own list inside the screen',
          doc[0] <= doc[1] and all(s == [True, 'auto', True] for s in screens), [doc, screens])

    page.goto(f'{BASE}/ui#s={quote(DIAGRAM)}')
    check('a mermaid fence draws as a diagram on the page served at /ui',
          until(page, shown(DIAGRAM_DRAWN), timeout=20000))
    page.context.close()


def check_keyboard_limits(browser, errors):
    page = new_page(browser, errors, stub_viewport=True)
    open_session(page, LONG)
    page.tap(COMPOSER)
    raw = """() => { const r = document.querySelector('#session-view').getBoundingClientRect();
                    return [Math.round(r.top), Math.round(r.height)]; }"""

    def viewport(height, offset_top, scroll_y, quiet=False):
        page.evaluate('(a) => window.__viewportStub.' + ('quiet' if quiet else 'set') + '(...a)',
                      [height, offset_top, scroll_y])

    def settles_at(box, timeout=3000):
        return until(page, f'(box) => JSON.stringify(({raw})()) === JSON.stringify(box)', box, timeout=timeout)

    viewport(500, 0, 0)
    settled = settles_at([0, 500])
    viewport(200, 0, 0)
    ok, box = view_covers_visible_part(page)
    check('a keyboard that leaves a small part of the screen: the view covers that part, composer included',
          settled and ok, box)

    viewport(500, 0, 0)
    settled = settles_at([0, 500])
    viewport(PORTRAIT[1] + 100, 0, 0)
    taller = not until(page, f'() => ({raw})()[1] !== 500', timeout=800)
    viewport(0, 0, 0)
    empty = not until(page, f'() => ({raw})()[1] !== 500', timeout=800)
    check('a viewport report taller than the screen, or empty, is not a keyboard',
          settled and taller and empty, page.evaluate(raw))

    viewport(500, 600, 200)
    ok, box = view_covers_visible_part(page)
    check('an offset past the short viewport, with a document scroll: both are undone and the view covers the visible part',
          ok and until(page, DOCUMENT_AT_TOP), box)

    viewport(600, 0, 0)
    settled, _ = view_covers_visible_part(page)
    page.wait_for_timeout(100)
    viewport(500, 344, 0, quiet=True)
    ok, box = view_covers_visible_part(page, timeout=1500)
    check('a keyboard still moving after its last report: the view catches up once it settles',
          settled and ok, box)

    viewport(500, 0, 0)
    settles_at([0, 500])
    page.evaluate("() => document.getElementById('btn-copy-id').click()")
    page.wait_for_selector('#toast', state='visible')
    toast = page.evaluate("""() => { const t = document.querySelector('#toast');
      return [t.getBoundingClientRect().bottom, getComputedStyle(t).pointerEvents]; }""")
    check('a toast stands above the keyboard and takes no tap', toast[0] <= 500 and toast[1] == 'none', toast)
    page.evaluate('() => window.__viewportStub.set(null, 0, null)')
    page.context.close()


def screen_in_flow(page, screen):
    """Whether `screen` is a child of the body in its normal flow, covering
    the whole screen."""
    return page.eval_on_selector(screen, """s => {
      const r = s.getBoundingClientRect();
      return s.parentElement === document.body && getComputedStyle(s).position === 'static'
        && Math.round(r.top) === 0 && Math.round(r.height) === window.innerHeight;
    }""")


def check_screens(browser, errors):
    """The list, a session and each tab are screens: exactly one is in the
    page at a time, in the body's flow, and it covers the screen."""
    page = new_page(browser, errors)
    open_list(page)
    check('the list is the only screen in the page, in flow over the whole screen',
          list_showing(page) and screen_in_flow(page, HOME_SCREEN), screens_shown(page))
    for strip, tab, back in [
        ('#health-strip', '#machines-tab', '#btn-machines-back'),
        ('#skills-strip', '#skills-tab', '#btn-skills-back'),
        ('#mcp-strip', '#mcp-tab', '#btn-mcp-back'),
    ]:
        page.tap(strip)
        page.wait_for_selector(tab, state='visible')
        check(f'{tab} is the only screen in the page, in flow over the whole screen',
              screens_shown(page) == [tab] and screen_in_flow(page, tab), screens_shown(page))
        page.tap(back)
        page.wait_for_selector(tab, state='hidden')
        check(f'leaving {tab} leaves the list as the only screen', list_showing(page), screens_shown(page))

    open_from_list(page, LONG_ROW)
    check('a session is the only screen in the page, in flow over the whole screen',
          session_showing(page) and screen_in_flow(page, VIEW), screens_shown(page))
    # A session reached by forward while a tab shows replaces the tab.
    leave_session(page)
    page.tap('#health-strip')
    page.wait_for_selector('#machines-tab', state='visible')
    page.go_forward()
    page.wait_for_selector(VIEW, state='visible')
    check('a session opened while a tab shows is the only screen in the page',
          session_showing(page), screens_shown(page))
    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    check('back from it leaves the list as the only screen', list_showing(page), screens_shown(page))
    page.context.close()


def check_viewport_report(browser, errors):
    """With `?viewport-report` in the address the pane sends what it sees of
    the viewport as each event arrives, in batches at most four a second;
    without it, nothing."""
    keys = {'at', 'event', 'inner_width', 'inner_height', 'visual', 'scroll_y', 'scroll_height',
            'view', 'composer', 'focused'}

    def record(page):
        page.add_init_script(RECORD_REPORTS)
        sent = []
        page.on('response', lambda response: response.url.endswith('/viewport-report') and sent.append(
            (response.request.post_data_json, response.status)))
        return sent

    def samples(sent):
        return [sample for body, _ in sent for sample in body['samples']]

    for query in ('?viewport-report', ''):
        page = new_page(browser, errors)
        sent = record(page)
        page.goto(f'{BASE}/{query}#s={quote(LONG)}')
        page.wait_for_selector(TRANSCRIPT_LINE)
        page.tap(COMPOSER)
        # A burst of resizes, as a keyboard's movement sends.
        for height in range(844, 500, -20):
            page.set_viewport_size({'width': PORTRAIT[0], 'height': height})
        if query:
            focused = until(page, '() => document.activeElement.id === "input"')
            page.wait_for_timeout(1200)
            taken = samples(sent)
            pages = {body['page'] for body, _ in sent}
            check('with ?viewport-report the pane sends its viewport samples from one page load, each batch taken with 204',
                  focused and taken and all(set(sample) == keys for sample in taken) and len(pages) == 1
                  and all(status == 204 for _, status in sent)
                  and any(sample['focused'] == 'textarea#input' and sample['composer'] for sample in taken),
                  sent[:2])
            # Chromium sends one resize per frame, so a burst gives fewer events
            # than steps.
            check('the burst\'s resizes are sampled, and the samples arrive in the order they were taken',
                  any(sample['event'] == 'resize' for sample in taken)
                  and [s['at'] for s in taken] == sorted(s['at'] for s in taken),
                  [s['event'] for s in taken])
            starts = page.evaluate('() => window.__reportTimes')
            gaps = [round(b - a) for a, b in zip(starts, starts[1:])]
            check('the batches come at most four a second', gaps and min(gaps) >= 240, gaps)
        else:
            page.wait_for_timeout(1200)
            check('without ?viewport-report the pane sends no report', not sent, len(sent))
        page.context.close()

    # A scroll the pane undoes reaches the report as it arrived.
    page = new_page(browser, errors, stub_viewport=True)
    sent = record(page)
    page.goto(f'{BASE}/?viewport-report#s={quote(LONG)}')
    page.wait_for_selector(TRANSCRIPT_LINE)
    page.tap(COMPOSER)
    page.evaluate('(a) => window.__viewportStub.set(...a)', [500, 0, 344])
    undone = until(page, DOCUMENT_AT_TOP)
    seen = False
    for _ in range(30):
        if any(sample['scroll_y'] == 344 for sample in samples(sent)):
            seen = True
            break
        page.wait_for_timeout(100)
    check('a document scroll the pane undoes still reaches the report with its scroll',
          undone and seen, [(s['event'], s['scroll_y']) for s in samples(sent)][-5:])
    page.evaluate('() => window.__viewportStub.set(null, 0, null)')
    page.context.close()


def check_fork(browser, errors):
    page = new_page(browser, errors, http_errors=True)
    held = []
    page.route('**/fork', lambda route: held.append(route))
    open_session(page, LONG)
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.tap('#btn-fork')
    wait_for(page, lambda: held)
    check('the fork control is off while the clone runs', held and page.is_disabled('#btn-fork'))
    for route in held:
        route.fulfill(status=200, content_type='application/json', body=json.dumps({'id': EMPTY}))
    page.unroute('**/fork')
    check('a fork opens the fork',
          until(page, "(hash) => location.hash === hash", '#s=' + quote(EMPTY))
          and page.is_hidden(SHEET) and not page.is_disabled('#btn-fork'))

    page.route('**/fork', lambda route: route.fulfill(status=409, body='no repository'))
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.tap('#btn-fork')
    refused = until(page, "() => document.querySelector('#view-fork').textContent.startsWith('fork: ')")
    check('a refused fork says so in the sheet, and the control comes back',
          refused and page.is_visible(SHEET) and not page.is_disabled('#btn-fork'))
    page.context.close()


def check_clear(browser, errors):
    page = new_page(browser, errors, http_errors=True)
    # The control asks before it posts, so every press here answers the
    # browser's own confirm box.
    page.on('dialog', lambda dialog: dialog.accept())
    open_session(page, LONG)
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    check('a root session offers the clear control in its actions sheet',
          page.is_visible(CLEAR_ROW) and page.inner_text(CLEAR_BTN) == 'Clear context',
          page.inner_text(CLEAR_BTN))

    # The pane draws nothing for a clear: the break is the marker's own durable
    # event on the session's stream, and this route sends none. Every row the
    # transcript holds is counted, the divider kind included, so a handler that
    # drew the break itself fails here.
    rows = settled_rows(page)
    page.route('**/clear', lambda route: route.fulfill(status=204, body=''))
    page.tap(CLEAR_BTN)
    check('a clear closes the sheet, toasts, and draws no row of its own',
          until(page, "() => document.querySelector('#toast').textContent === 'context cleared'")
          and page.is_hidden(SHEET) and settled_rows(page) == rows,
          settled_rows(page))
    # The answer leaves nothing to wait for: the sheet that opens next has no
    # note and a live control, not the `clearing…` the request wrote.
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    check('a clear that answered leaves no note behind and a live control',
          page.inner_text(CLEAR_NOTE) == '' and not page.is_disabled(CLEAR_BTN),
          [page.inner_text(CLEAR_NOTE), page.is_disabled(CLEAR_BTN)])
    page.unroute('**/clear')

    page.route('**/clear', lambda route: route.fulfill(
        status=409, body='the session is running; interrupt it first'))
    page.tap(MORE)
    page.wait_for_selector(SHEET, state='visible')
    page.tap(CLEAR_BTN)
    refused = until(page, f"() => document.querySelector('{CLEAR_NOTE}').textContent.startsWith('clear: ')")
    check('a refused clear says so beside the control, and the control comes back',
          refused and page.is_visible(SHEET) and not page.is_disabled(CLEAR_BTN),
          page.inner_text(CLEAR_NOTE))
    page.unroute('**/clear')

    # A request that never answers must not leave a control stuck: it names what
    # it waits for while it runs, and the session change that follows gives the
    # control and the note back. The route is held open, so nothing but the
    # session change can answer for it.
    held = []
    page.route('**/clear', lambda route: held.append(route))
    page.wait_for_selector(SHEET, state='visible')
    page.tap(CLEAR_BTN)
    check('a clear in flight turns the control off and names what it waits for',
          until(page, f"() => document.querySelector('{CLEAR_NOTE}').textContent === 'clearing…'")
          and page.is_disabled(CLEAR_BTN), page.inner_text(CLEAR_NOTE))

    page.go_back()
    page.wait_for_selector(VIEW, state='hidden')
    open_from_list(page, LONG_ROW)
    # The sheet is hidden again, so the control and its note are read from the
    # document rather than through a visible-locator call.
    after = page.evaluate(
        f"() => [document.querySelector('{CLEAR_BTN}').disabled, "
        f"document.querySelector('{CLEAR_NOTE}').textContent]")
    check('leaving the session gives the clear control and its note back',
          after == [False, ''], after)
    # The held request is aborted before the route is dropped, so it never
    # reaches the server and cannot clear the session the later checks read.
    for route in held:
        route.abort()
    page.unroute('**/clear')
    page.context.close()


def text_frame(role, text):
    return message_frame(role, {'kind': 'text', 'text': text}, 1)


def ask_block(message, answer=None, child_id=None):
    block = {'kind': 'ask', 'message': message, 'options': ['a', 'b']}
    if answer is not None:
        block['answer'] = answer
    if child_id is not None:
        block['child_id'] = child_id
    return block


def check_following(browser, errors):
    # Lines that arrive while the reader is scrolled up leave them where they
    # are.
    page = new_page(browser, errors)
    release = staged_stream(page, EMPTY, [text_frame('user', f'early {i}') for i in range(80)],
                            [text_frame('user', 'late line'), {'delta': 'late words'}])
    open_list(page)
    open_from_list(page, EMPTY_ROW)
    page.wait_for_selector(f'{USER_LINE} >> text=early 79')
    until(page, AT_BOTTOM, TRANSCRIPT)
    page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = t.scrollHeight / 2; }')
    until(page, shown(BOTTOM_CONTROL))
    top = page.eval_on_selector(TRANSCRIPT, 't => t.scrollTop')
    release()
    page.wait_for_selector(f'{REPLY_LINE} >> text=late words')
    after = page.eval_on_selector(TRANSCRIPT, 't => t.scrollTop')
    check('lines that arrive while the reader is scrolled up leave the reader where they are',
          top > 0 and abs(after - top) <= 1 and page.locator(f'{USER_LINE} >> text=late line').count() == 1,
          [top, after])
    page.context.close()

    # A streamed paragraph is replaced by its durable text, and dropped by a
    # durable block of another kind.
    page = new_page(browser, errors)
    route_stream(page, EMPTY,
                 {'delta': 'streamed then stored'},
                 message_frame('assistant', {'kind': 'text', 'text': 'streamed then stored'}, 1),
                 {'delta': 'streamed then dropped'},
                 message_frame('assistant', {'kind': 'tool_call', 'id': 'call-x', 'name': 'shell',
                                             'args': {'command': 'true'}}, 2))
    open_list(page)
    open_from_list(page, EMPTY_ROW)
    page.wait_for_selector('#transcript .line.tool')
    stored = texts(page, REPLY_LINE)
    check('a streamed paragraph followed by its durable text shows the text once',
          stored.count('streamed then stored') == 1, stored)
    check('a streamed paragraph followed by a tool call leaves no streamed paragraph',
          not any('streamed then dropped' in t for t in texts(page, TRANSCRIPT)), stored)
    page.context.close()

    # Opening a line makes the transcript taller: a reader at the newest line
    # stays there, and a reader scrolled up is not moved.
    page = new_page(browser, errors)
    open_session(page, LONG)
    until(page, AT_BOTTOM, TRANSCRIPT)
    open_line = """([sel, which]) => {
      const lines = [...document.querySelectorAll(sel)].filter(l => l.textContent.includes('/very/long/path'));
      const line = which === 'last' ? lines[lines.length - 1] : lines[Math.floor(lines.length / 2)];
      const t = document.querySelector('#transcript');
      const before = t.scrollHeight;
      line.click();
      return t.scrollHeight - before;
    }"""
    grew = page.evaluate(open_line, ['#transcript .line.tool', 'last'])
    check('opening a line at the newest line keeps the reader at the newest line',
          grew > 0 and until(page, AT_BOTTOM, TRANSCRIPT), grew)
    page.eval_on_selector(TRANSCRIPT, 't => { t.scrollTop = t.scrollHeight / 2; }')
    until(page, shown(BOTTOM_CONTROL))
    top = page.eval_on_selector(TRANSCRIPT, 't => t.scrollTop')
    grew = page.evaluate(open_line, ['#transcript .line.tool', 'middle'])
    page.wait_for_timeout(300)
    after = page.eval_on_selector(TRANSCRIPT, 't => t.scrollTop')
    check('opening a line while scrolled up does not move the reader',
          grew > 0 and top > 0 and abs(after - top) <= 1,
          [grew, top, after])
    page.context.close()


def check_ask_records(browser, errors):
    # The panel's frames keep their own ask record: the session's question
    # still takes its own answered copy after a child's question is drawn.
    page = new_page(browser, errors)
    release = staged_stream(page, EMPTY,
                            [message_frame('assistant', ask_block('Session question?', child_id=CHILD), 1)],
                            [message_frame('assistant', ask_block('Session question?', 'a', CHILD), 2)])
    route_stream(page, CHILD, message_frame('assistant', ask_block('Child question?'), 1))
    open_list(page)
    open_from_list(page, EMPTY_ROW)
    page.wait_for_selector(ASK_BOX)
    page.tap(f'{ASK_BOX} .child-watch')
    page.wait_for_selector('#child-transcript .ask')
    release()
    page.wait_for_selector(f'{ASK_BOX} {ASK_ANSWER}')
    boxes = page.eval_on_selector_all(ASK_BOX, 'els => els.map(e => e.querySelectorAll(".answer").length)')
    check('a child\'s question in the panel leaves the session\'s question to take its own answer', boxes == [1], boxes)
    page.context.close()

    # A page read back draws with an ask record of its own: the session's
    # question still takes its own answered copy after the page is drawn.
    page = new_page(browser, errors)
    release = staged_stream(page, EMPTY,
                            [{'history': {'before': 1000, 'more': True}}]
                            + [text_frame('user', f'recent {i}') for i in range(3)]
                            + [message_frame('assistant', ask_block('Tail question?'), 1)],
                            [message_frame('assistant', ask_block('Tail question?', 'a'), 2)])
    older = {'events': [{'seq': 500 + i, 'event': {'kind': 'message', 'at_ms': 1, 'message': {
        'role': 'user', 'block': {'kind': 'text', 'text': f'older {i}'}}}} for i in range(3)], 'more': False}
    page.route(f'**/sessions/{EMPTY}/history*', lambda route: route.fulfill(
        status=200, content_type='application/json', body=json.dumps(older)))
    open_list(page)
    open_from_list(page, EMPTY_ROW)
    page.wait_for_selector(ASK_BOX)
    page.tap(EARLIER)
    page.wait_for_selector(f'{USER_LINE} >> text=older 0')
    release()
    page.wait_for_selector(f'{ASK_BOX} {ASK_ANSWER}')
    boxes = page.eval_on_selector_all(ASK_BOX, 'els => els.map(e => e.querySelectorAll(".answer").length)')
    check('a page read back leaves the session\'s question to take its own answer', boxes == [1], boxes)
    page.context.close()


CHECKS = [
    check_tail_and_read_back,
    check_ask_across_a_page_boundary,
    check_history,
    check_widths,
    check_field_sizes,
    check_bottom_control,
    check_keyboard_ride,
    check_what_a_closed_session_leaves_behind,
    check_diagram,
    check_list,
    check_stamps,
    check_kinds,
    check_panel,
    check_panel_frames,
    check_addresses,
    check_leaving_clears_the_view,
    check_composer,
    check_layout,
    check_keyboard_limits,
    check_screens,
    check_viewport_report,
    check_fork,
    check_clear,
    check_following,
    check_ask_records,
]


def main():
    if PlaywrightError is None:
        print(INSTALL)
        return NO_BROWSER
    errors = []
    with sync_playwright() as playwright:
        try:
            browser = playwright.chromium.launch()
        except PlaywrightError as error:
            # Any launch failure lands here. The hint below is wrong when
            # Chromium is installed but cannot run; the error above says which.
            print(f'{error}\n\nChromium could not be started. If it is not installed:\n{INSTALL}')
            return NO_BROWSER
        for run in CHECKS:
            try:
                run(browser, errors)
            except Exception:
                check(run.__name__ + ' ran to the end', False, traceback.format_exc(limit=2))
        browser.close()
    check('no page errors or console errors', not errors, errors[:5])
    print(f'{len(failures)} failed' if failures else 'all checks passed')
    return 1 if failures else 0


if __name__ == '__main__':
    sys.exit(main())
