"""Browser checks for the web pane, on a phone-sized Chromium.

Usage: python3 pane.py <base url>

The base url serves the real control-plane router over a store that
`tests/browser.rs` seeds. This script knows the seeded session ids and their
contents, and nothing else about the server. It drives the pane only through
the DOM, the browser's history and a stubbed visual viewport, so the checks
hold while the pane's code is restructured. Each check prints PASS or FAIL.
The exit status is 1 when any check fails, and 2 when Chromium cannot be
started: Playwright or its Chromium is not installed, or it cannot run here.
"""

import re
import sys
import traceback
from urllib.parse import quote

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
ASK_BEFORE = 100
ASK_AFTER = 59
LIST_ROWS = 40

PORTRAIT = (390, 844)
LANDSCAPE = (844, 390)

# The pane's elements these checks read. A restructure that renames one changes
# it here only.
HOME = 'body > main'
SESSION_ROW = '.session-row'
VIEW = '#session-view'
TRANSCRIPT = '#transcript'
TRANSCRIPT_LINE = '#transcript .msg'
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
WATCH_CHILD = '#transcript .child-watch'
CHILD_PANEL = '#child-panel'
CHILD_LINE = '#child-transcript .msg'
CHILD_CLOSE = '#btn-child-collapse'
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


# The screen's layout, as the checks see it. The pane today lays the session
# view over the home column and hides the column; a new layout changes these
# helpers and leaves the checks alone.

def list_showing(page):
    """The session list is on screen and no session covers it."""
    return page.is_visible(HOME) and page.is_hidden(VIEW)


def session_showing(page):
    return page.is_visible(VIEW) and page.is_hidden(HOME)


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


# The session view's box in the coordinates of the visible part of the page,
# which the viewport stub sets: its top is 0 when the view starts at the visible
# top. The stub fakes a document scroll, so the view's place is moved by the
# scroll the test set. That models a view placed in the document, which a
# document scroll carries with the page. A `position: fixed` view stays put
# under a document scroll, and this measure must then leave scrollY out.
VIEW_IN_VISIBLE_PART = f"""() => {{
  const stub = window.__viewportStub;
  const rect = document.querySelector('{VIEW}').getBoundingClientRect();
  const top = rect.top + stub.realScrollY() - window.scrollY - window.visualViewport.offsetTop;
  return {{ top: Math.round(top), height: Math.round(rect.height),
           visible: Math.round(window.visualViewport.height) }};
}}"""


def view_covers_visible_part(page, timeout=3000):
    """Whether the view comes to start at the visible top with the visible
    height, and the box it has."""
    ok = until(page, f"() => {{ const b = ({VIEW_IN_VISIBLE_PART})(); return b.top === 0 && b.height === b.visible; }}",
               timeout=timeout)
    return ok, page.evaluate(VIEW_IN_VISIBLE_PART)


def view_covers_screen(page, timeout=3000):
    """Whether the view comes to cover the whole layout viewport, as it does
    with no keyboard, and the box it has."""
    box = f"""() => {{ const r = document.querySelector('{VIEW}').getBoundingClientRect();
      return {{ top: Math.round(r.top), height: Math.round(r.height), screen: window.innerHeight }}; }}"""
    ok = until(page, f"() => {{ const b = ({box})(); return b.top === 0 && b.height === b.screen; }}",
               timeout=timeout)
    return ok, page.evaluate(box)


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
# announced with a resize event, as a keyboard's is. `__viewportStub` gives the
# measuring helpers the real scroll beside the one the test set.
VIEWPORT_STUB = """
(() => {
  const realScrollY = Object.getOwnPropertyDescriptor(window, 'scrollY');
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
  window.__viewportStub = {
    set(height, offsetTop, scrollY) {
      Object.assign(set, { height, offsetTop, scrollY });
      viewport.dispatchEvent(new Event('resize'));
    },
    realScrollY: () => realScrollY.get.call(window),
  };
})();
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


def new_page(browser, errors, size=PORTRAIT, stub_viewport=False):
    context = browser.new_context(
        viewport={'width': size[0], 'height': size[1]},
        is_mobile=True,
        has_touch=True,
        device_scale_factor=3,
    )
    context.add_init_script(NO_SCROLL_ANCHORING)
    if stub_viewport:
        context.add_init_script(VIEWPORT_STUB)
    page = context.new_page()
    page.set_default_timeout(15000)
    page.on('pageerror', lambda error: errors.append(f'page error: {error}'))
    page.on('console', lambda msg: msg.type == 'error' and errors.append(f'console: {msg.text}'))
    return page


def open_session(page, session_id):
    page.goto(f'{BASE}/#s={quote(session_id)}')
    page.wait_for_selector(TRANSCRIPT_LINE)


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
    ok, box = keyboard(500, 344, 0)
    check('a viewport offset with the composer focused: the view covers the visible part', ok, box)
    ok, box = keyboard(500, 0, 0)
    check('a keyboard that pushes nothing: the view covers the visible part', ok, box)

    # The stub still reports the keyboard, and sends no resize: only leaving
    # the field can give the view its height back here.
    page.evaluate('() => document.activeElement.blur()')
    ok, box = view_covers_screen(page)
    check('leaving the field gives the view the whole screen back', ok, box)
    page.evaluate('() => window.__viewportStub.set(null, 0, null)')
    page.context.close()


CHECKS = [
    check_tail_and_read_back,
    check_ask_across_a_page_boundary,
    check_history,
    check_widths,
    check_field_sizes,
    check_bottom_control,
    check_keyboard_ride,
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
