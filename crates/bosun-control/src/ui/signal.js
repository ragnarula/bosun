// Bosun Signal's drawn parts: each persona's signal flag, the robot avatar a
// seed draws, and the pane's line icons. Every one is built with
// `createElementNS`, so the pane inserts no markup.

export { avatar, flag, flagLetter, icon, memberName, personaLabel };

const NS = 'http://www.w3.org/2000/svg';

function svgEl(name, attrs, parent) {
  const el = document.createElementNS(NS, name);
  for (const [key, value] of Object.entries(attrs)) el.setAttribute(key, String(value));
  if (parent) parent.appendChild(el);
  return el;
}

// FNV-1a: the same seed always picks the same flag and the same robot.
function hash(text) {
  let h = 2166136261;
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 16777619) >>> 0;
  }
  return h >>> 0;
}

// Each persona flies one International Code of Signals flag, drawn on a
// 24 × 18 cloth. A rect is [colour, x, y, width, height]; a polygon is
// ['poly-colour', points].
const FLAGS = {
  P: [['blue', 0, 0, 24, 18], ['white', 8, 6, 8, 6]],
  K: [['yellow', 0, 0, 12, 18], ['blue', 12, 0, 12, 18]],
  O: [['poly-yellow', '0,0 0,18 24,18'], ['poly-red', '0,0 24,0 24,18']],
  U: [['red', 0, 0, 12, 9], ['white', 12, 0, 12, 9], ['white', 0, 9, 12, 9], ['red', 12, 9, 12, 9]],
  G: [
    ['yellow', 0, 0, 4, 18], ['blue', 4, 0, 4, 18], ['yellow', 8, 0, 4, 18],
    ['blue', 12, 0, 4, 18], ['yellow', 16, 0, 4, 18], ['blue', 20, 0, 4, 18],
  ],
  D: [['yellow', 0, 0, 24, 5], ['blue', 0, 5, 24, 8], ['yellow', 0, 13, 24, 5]],
  T: [['red', 0, 0, 8, 18], ['white', 8, 0, 8, 18], ['blue', 16, 0, 8, 18]],
  J: [['blue', 0, 0, 24, 6], ['white', 0, 6, 24, 6], ['blue', 0, 12, 24, 6]],
  N: (() => {
    const cells = [];
    for (let y = 0; y < 4; y++) {
      for (let x = 0; x < 4; x++) {
        cells.push([(x + y) % 2 ? 'white' : 'blue', x * 6, y * 4.5, 6, 4.5]);
      }
    }
    return cells;
  })(),
  X: [['white', 0, 0, 24, 18], ['blue', 10, 0, 4, 18], ['blue', 0, 7, 24, 4]],
};
const PERSONA_FLAGS = { lead: 'P', architect: 'K', builder: 'O', reviewer: 'U', researcher: 'G' };
const SPARE_FLAGS = ['D', 'T', 'J', 'N', 'X'];

// The flag a persona flies: its own for the five named ones, and a stable
// spare chosen by its name for any other.
function flagLetter(persona) {
  if (!persona) return null;
  const key = String(persona).toLowerCase();
  return PERSONA_FLAGS[key] || SPARE_FLAGS[hash(key) % SPARE_FLAGS.length];
}

function flag(persona) {
  const letter = flagLetter(persona) || 'P';
  const wrap = document.createElement('span');
  wrap.className = 'bs-flag';
  const svg = svgEl('svg', {
    viewBox: '0 0 24 18',
    role: 'img',
    'aria-label': (persona ? persona + ' flag, ' : '') + 'signal ' + letter,
  }, wrap);
  for (const part of FLAGS[letter]) {
    if (part[0].startsWith('poly-')) {
      svgEl('polygon', { points: part[1], class: 'f-' + part[0].slice(5) }, svg);
    } else {
      svgEl('rect', { x: part[1], y: part[2], width: part[3], height: part[4], class: 'f-' + part[0] }, svg);
    }
  }
  svgEl('rect', { x: 0.5, y: 0.5, width: 23, height: 17, rx: 1.5, class: 'f-edge' }, svg);
  return wrap;
}

// A robot face built from the seed: body colour, crown, head shape, eyes and
// mouth each come from different bits of its hash. Shapes only, never a
// human face.
function robot(seed) {
  const h = hash(String(seed || 'crew'));
  const hull = 'fill:var(--hull-' + (1 + (h % 6)) + ')';
  const visor = 'fill:var(--visor)';
  const svg = svgEl('svg', { viewBox: '0 0 40 40', 'aria-hidden': 'true' });
  const part = (name, attrs, style) => svgEl(name, { ...attrs, style }, svg);
  part('rect', { x: 11, y: 31, width: 18, height: 12, rx: 4, opacity: 0.75 }, hull);
  const crown = (h >>> 3) % 3;
  if (crown === 0) {
    part('rect', { x: 19, y: 4, width: 2, height: 6 }, hull);
    part('circle', { cx: 20, cy: 4, r: 2.5 }, hull);
  } else if (crown === 1) {
    part('rect', { x: 6, y: 15, width: 4, height: 8, rx: 2 }, hull);
    part('rect', { x: 30, y: 15, width: 4, height: 8, rx: 2 }, hull);
  } else {
    part('path', { d: 'M13 10 L16 5 L19 10 Z' }, hull);
    part('path', { d: 'M21 10 L24 5 L27 10 Z' }, hull);
  }
  part('rect', { x: 9, y: 9, width: 22, height: 21, rx: [5, 9, 13][(h >>> 5) % 3] }, hull);
  const eyes = (h >>> 7) % 4;
  if (eyes === 0) {
    part('circle', { cx: 15.5, cy: 18.5, r: 2.6 }, visor);
    part('circle', { cx: 24.5, cy: 18.5, r: 2.6 }, visor);
  } else if (eyes === 1) {
    part('rect', { x: 12.5, y: 16, width: 15, height: 5, rx: 2.5 }, visor);
  } else if (eyes === 2) {
    part('rect', { x: 13, y: 16, width: 5, height: 5, rx: 1 }, visor);
    part('rect', { x: 22, y: 16, width: 5, height: 5, rx: 1 }, visor);
  } else {
    part('rect', { x: 13, y: 17.5, width: 6, height: 2.6, rx: 1.3 }, visor);
    part('rect', { x: 21, y: 17.5, width: 6, height: 2.6, rx: 1.3 }, visor);
  }
  const mouth = (h >>> 9) % 3;
  if (mouth === 0) {
    part('rect', { x: 16, y: 24, width: 8, height: 2, rx: 1 }, visor);
  } else if (mouth === 1) {
    for (let i = 0; i < 4; i++) part('rect', { x: 14.5 + i * 3, y: 23.5, width: 2, height: 3.2, rx: 0.5 }, visor);
  } else {
    part('path', { d: 'M15.5 23.5 Q20 27.5 24.5 23.5 L24.5 24.6 Q20 28.8 15.5 24.6 Z' }, visor);
  }
  return svg;
}

const STATE_WORDS = {
  working: 'working',
  waiting: 'waiting',
  'needs-you': 'needs you',
  idle: 'idle',
  stopped: 'stopped',
};

// A crew member's face: the persona's uploaded picture when it has one,
// otherwise the robot `seed` draws; a ring for `state`; and the persona's
// flag at the corner of a medium or large avatar. Sizes: xs, sm, md, lg.
function avatar({ seed, persona, state, size, src, label }) {
  const outer = document.createElement('span');
  outer.className = 'bs-avatar' + (size && size !== 'md' ? ' bs-avatar--' + size : '');
  if (state) outer.dataset.state = state;
  const name = label || personaLabel(persona) || 'Crew member';
  outer.setAttribute('role', 'img');
  outer.setAttribute('aria-label', name + (state ? ', ' + (STATE_WORDS[state] || state) : ''));
  const pic = document.createElement('span');
  pic.className = 'bs-avatar__pic';
  if (src) {
    const img = document.createElement('img');
    img.src = src;
    img.alt = '';
    pic.appendChild(img);
  } else {
    pic.appendChild(robot(seed || persona || name));
  }
  outer.appendChild(pic);
  if (persona) {
    const badge = document.createElement('span');
    badge.className = 'bs-avatar__flag';
    badge.appendChild(flag(persona));
    outer.appendChild(badge);
  }
  return outer;
}

// A persona's name as a person reads it: `builder` is Builder.
function personaLabel(persona) {
  if (!persona) return '';
  return persona.charAt(0).toUpperCase() + persona.slice(1);
}

// The name a crew member goes by: its persona, numbered when more than one
// member of the crew flies the same flag. `order` is the crew in the order
// its members joined.
function memberName(member, order) {
  const label = personaLabel(member.persona) || 'Agent';
  const same = order.filter((other) => other.persona === member.persona);
  if (same.length < 2) return label;
  return label + ' ' + (same.indexOf(member) + 1);
}

// Line icons on a 24 grid, 1.75 stroke, round ends, drawn in currentColor.
const ICONS = {
  back: ['M15 5 8 12l7 7'],
  stop: ['M7 7h10v10H7z'],
  send: ['M5 12h13', 'M13 6l6 6-6 6'],
  plus: ['M12 5v14', 'M5 12h14'],
  chat: ['M5 6h14v9h-9l-4 3.5V15H5z'],
  tasks: ['M10 7h9', 'M10 12h9', 'M10 17h9', 'M4.5 7l1.2 1.2L7.8 6', 'M4.5 12l1.2 1.2 2.1-2.2', 'M4.5 17l1.2 1.2 2.1-2.2'],
  files: ['M7 3h7l4 4v14H7z', 'M14 3v4h4'],
  log: ['M5 6h14', 'M5 10h14', 'M5 14h10', 'M5 18h7'],
  more: ['M6 12h.01', 'M12 12h.01', 'M18 12h.01'],
  fork: ['M7 4v7a4 4 0 0 0 4 4h6', 'M17 4v7', 'M14 12l3 3-3 3'],
  clear: ['M4.5 12a7.5 7.5 0 1 0 2.2-5.3', 'M4.5 4.5v3.8h3.8'],
  copy: ['M9 9h11v11H9z', 'M15 9V4H4v11h5'],
  trash: ['M5 7h14', 'M9.5 7V4.5h5V7', 'M7 7l1 13h8l1-13'],
  check: ['M5 12.5l4.5 4.5L19 7'],
  edit: ['M4 20h4L19 9l-4-4L4 16z', 'M13.5 6.5l4 4'],
  read: ['M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z', 'M12 9.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5z'],
  terminal: ['M4 6l5 5-5 5', 'M11 18h9'],
  build: ['M14.5 4.5a4.5 4.5 0 0 0-4.3 5.8L4 16.5 7.5 20l6.2-6.2a4.5 4.5 0 0 0 5.8-4.3l-2.6 1.1-2.5-2.5z'],
  review: ['M11 4.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13z', 'M16 16l4.5 4.5'],
  test: ['M9 3.5h6', 'M10 3.5V9l-5 8.5A2 2 0 0 0 6.7 20.5h10.6a2 2 0 0 0 1.7-3L14 9V3.5', 'M7.5 14h9'],
  design: ['M12 3.5 5 20.5', 'M12 3.5l7 17', 'M8 14h8'],
  research: ['M4 5h5.5A2.5 2.5 0 0 1 12 7.5V20a2 2 0 0 0-2-2H4z', 'M20 5h-5.5A2.5 2.5 0 0 0 12 7.5V20a2 2 0 0 1 2-2h6z'],
  ask: ['M12 3.5a8.5 8.5 0 1 0 0 17 8.5 8.5 0 0 0 0-17z', 'M9.6 9.4a2.5 2.5 0 1 1 3.4 2.3c-.6.3-1 .9-1 1.5v.6', 'M12 16.8h.01'],
  shield: ['M12 3.5l7 3v5.5c0 4.2-3 7.2-7 8.8-4-1.6-7-4.6-7-8.8V6.5z'],
  persona: ['M12 4a4 4 0 1 0 0 8 4 4 0 0 0 0-8z', 'M4.5 20.5a7.5 7.5 0 0 1 15 0'],
  branch: ['M7 4v16', 'M7 14a6 6 0 0 0 6-6V7', 'M13 4.5a2.5 2.5 0 1 0 0 .01'],
  machines: ['M4 5h16v10H4z', 'M9 19h6', 'M12 15v4'],
  skills: ['M12 3l2.6 5.6 6 .7-4.5 4.1 1.2 6L12 16.4 6.7 19.4l1.2-6L3.4 9.3l6-.7z'],
  plug: ['M9 3v5', 'M15 3v5', 'M6 8h12v3a6 6 0 0 1-12 0z', 'M12 17v4'],
  crew: ['M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M16 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M2.5 19a5.5 5.5 0 0 1 11 0', 'M10.5 19a5.5 5.5 0 0 1 11 0'],
  upload: ['M12 16V5', 'M7 9l5-5 5 5', 'M5 19h14'],
  shuffle: ['M4 7h4l8 10h4', 'M4 17h4l8-10h4', 'M18 5l2 2-2 2', 'M18 15l2 2-2 2'],
};

function icon(name) {
  const wrap = document.createElement('span');
  wrap.className = 'bs-icon';
  wrap.setAttribute('aria-hidden', 'true');
  const svg = svgEl('svg', { viewBox: '0 0 24 24' }, wrap);
  for (const d of ICONS[name] || ICONS.more) {
    const path = svgEl('path', { d }, svg);
    if (name === 'more') path.setAttribute('style', 'stroke-width:3');
  }
  return wrap;
}
