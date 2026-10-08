// The Crew screen: each persona's face and flag, and the three ways to change
// a face: draw another robot, upload a picture, or go back to the robot.

import { $, crewTab } from './dom.js';
import { post, toastError, toastOk } from './common.js';
import { personas, refreshPersonas } from './new-session.js';
import { leaveScreen, showScreen } from './screens.js';
import { avatar, icon, personaLabel } from './signal.js';

export { renderCrewScreen };

const list = $('crew-personas');
const fileInput = $('avatar-file');
let uploadFor = null;

function personaAvatar(persona) {
  const src = persona.picture_at_secs
    ? '/personas/' + encodeURIComponent(persona.name) + '/avatar?v=' + persona.picture_at_secs
    : null;
  return avatar({ seed: persona.avatar_seed, persona: persona.name, size: 'lg', src });
}

function action(label, iconName, onClick, danger) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'bs-btn' + (danger ? ' bs-btn--ghost is-danger' : '');
  button.appendChild(icon(iconName));
  button.appendChild(document.createTextNode(label));
  button.addEventListener('click', onClick);
  return button;
}

function renderCrewScreen() {
  list.textContent = '';
  if (!personas.length) {
    const empty = document.createElement('div');
    empty.className = 'bs-empty';
    empty.textContent = 'No personas are configured.';
    list.appendChild(empty);
    return;
  }
  for (const persona of personas) {
    const row = document.createElement('div');
    row.className = 'crew-persona';
    row.appendChild(personaAvatar(persona));
    const text = document.createElement('div');
    text.className = 'crew-persona__text';
    const name = document.createElement('div');
    name.className = 'crew-persona__name';
    name.textContent = personaLabel(persona.name) + (persona.default ? ' · default' : '');
    text.appendChild(name);
    const description = document.createElement('div');
    description.className = 'crew-persona__description';
    description.textContent = persona.description || '';
    text.appendChild(description);
    const actions = document.createElement('div');
    actions.className = 'bs-row crew-persona__actions';
    actions.appendChild(action('Shuffle', 'shuffle', () => shuffle(persona)));
    actions.appendChild(action('Upload picture', 'upload', () => pickPicture(persona)));
    if (persona.picture_at_secs) {
      actions.appendChild(action('Use the robot', 'trash', () => removePicture(persona), true));
    }
    text.appendChild(actions);
    row.appendChild(text);
    list.appendChild(row);
  }
}

async function afterChange(message) {
  await refreshPersonas();
  renderCrewScreen();
  toastOk(message);
}

async function shuffle(persona) {
  try {
    await post('/personas/' + encodeURIComponent(persona.name) + '/avatar/shuffle', {});
    await afterChange(personaLabel(persona.name) + ' has a new face');
  } catch (error) {
    toastError('shuffle: ' + error.message);
  }
}

function pickPicture(persona) {
  uploadFor = persona;
  fileInput.value = '';
  fileInput.click();
}

fileInput.addEventListener('change', async () => {
  const file = fileInput.files && fileInput.files[0];
  const persona = uploadFor;
  uploadFor = null;
  if (!file || !persona) return;
  try {
    const response = await fetch('/personas/' + encodeURIComponent(persona.name) + '/avatar', {
      method: 'PUT',
      headers: { 'Content-Type': file.type || 'application/octet-stream' },
      body: file,
    });
    if (!response.ok) throw new Error(await response.text() || 'HTTP ' + response.status);
    await afterChange(personaLabel(persona.name) + "'s picture is saved");
  } catch (error) {
    toastError('upload: ' + error.message);
  }
});

async function removePicture(persona) {
  try {
    const response = await fetch('/personas/' + encodeURIComponent(persona.name) + '/avatar', {
      method: 'DELETE',
    });
    if (!response.ok) throw new Error('HTTP ' + response.status);
    await afterChange(personaLabel(persona.name) + ' shows its robot again');
  } catch (error) {
    toastError('remove: ' + error.message);
  }
}

$('crew-strip-btn').addEventListener('click', () => {
  showScreen(crewTab);
  renderCrewScreen();
});
$('btn-crew-back').addEventListener('click', () => leaveScreen(crewTab));
