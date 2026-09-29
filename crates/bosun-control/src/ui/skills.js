// The skill repositories screen and its strip.

import {
  btnRefToggle,
  btnSkillRepoAdd,
  btnSkillRepoCancel,
  btnSkillsAdd,
  btnSkillsBack,
  formSkillRef,
  formSkillRepo,
  skillRefLabel,
  skillsAdd,
  skillsList,
  skillsStrip,
  skillsStripText,
  skillsTab,
} from './dom.js';
import { shortSha, showStatus, showToast, toastOk } from './common.js';

export { refreshSkillRepos };

let skillRepos = [];
let skillReposFetching = false;

// The skill-repos sheet: the strip on home opens it, the head's back closes
// it, and a per-repo row's primary tap Updates the repo while the buttons
// under it toggle enable/disable and remove. State is one status dot and a
// package count per row, so the pane reads at a glance.
async function refreshSkillRepos() {
  if (skillReposFetching) return;
  skillReposFetching = true;
  try {
    const response = await fetch('/skills/repos');
    if (!response.ok) throw new Error('HTTP ' + response.status);
    skillRepos = await response.json();
    renderSkillsStrip();
    renderSkillRepos();
  } catch (error) {
    showStatus('skills: ' + error.message);
  } finally {
    skillReposFetching = false;
  }
}

function renderSkillsStrip() {
  if (skillRepos.length === 0) {
    skillsStripText.textContent = 'skill repos · none yet';
    return;
  }
  const packages = skillRepos.reduce((sum, repo) => sum + repo.package_count, 0);
  skillsStripText.textContent = skillRepos.length + ' repo'
    + (skillRepos.length === 1 ? '' : 's')
    + ' · ' + packages + ' package' + (packages === 1 ? '' : 's');
}

function openSkills() {
  skillsTab.hidden = false;
  refreshSkillRepos();
}

function closeSkills() {
  skillsTab.hidden = true;
  skillsAdd.hidden = true;
}

skillsStrip.addEventListener('click', openSkills);
skillsStrip.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    openSkills();
  }
});

btnSkillsBack.addEventListener('click', closeSkills);

btnSkillsAdd.addEventListener('click', () => {
  skillsAdd.hidden = !skillsAdd.hidden;
  if (!skillsAdd.hidden) formSkillRepo.focus();
});

btnRefToggle.addEventListener('click', () => {
  skillRefLabel.hidden = !skillRefLabel.hidden;
  btnRefToggle.textContent = skillRefLabel.hidden
    ? 'track a branch, tag, or sha'
    : 'track the default branch';
  if (!skillRefLabel.hidden) formSkillRef.focus();
});

btnSkillRepoCancel.addEventListener('click', () => {
  skillsAdd.hidden = true;
  formSkillRepo.value = '';
  formSkillRef.value = '';
});

btnSkillRepoAdd.addEventListener('click', async () => {
  const repo = formSkillRepo.value.trim();
  if (!repo) {
    showToast('enter a repo like owner/repo', 'error');
    return;
  }
  const ref = skillRefLabel.hidden ? '' : formSkillRef.value.trim();
  btnSkillRepoAdd.disabled = true;
  try {
    const response = await fetch('/skills/repos', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ repo, ref: ref || undefined }),
    });
    if (!response.ok) throw new Error('HTTP ' + response.status);
    toastOk('added ' + repo);
    formSkillRepo.value = '';
    formSkillRef.value = '';
    skillRefLabel.hidden = true;
    btnRefToggle.textContent = 'track a branch, tag, or sha';
    skillsAdd.hidden = true;
    await refreshSkillRepos();
  } catch (error) {
    // A failed add still stores the repo row with its last_error, so refresh
    // the list to show it; a retry would 409 'already exists' otherwise.
    showToast('add skill repo: ' + error.message, 'error');
    await refreshSkillRepos();
  } finally {
    btnSkillRepoAdd.disabled = false;
  }
});

formSkillRepo.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    btnSkillRepoAdd.click();
  }
});

// One status dot: up (enabled, no error), down (disabled), error. A disabled
// repo keeps its packages stored but stops advertising them; re-enabling is
// one tap and needs no fetch.
function repoDot(repo) {
  const dot = document.createElement('span');
  dot.className = 'dot ' + (repo.last_error ? 'down' : repo.enabled ? 'up' : 'warn');
  return dot;
}

function renderSkillRepos() {
  skillsList.textContent = '';
  if (skillRepos.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = 'no skill repos yet — add one to pull skills from GitHub';
    skillsList.appendChild(empty);
    return;
  }
  for (const repo of skillRepos) {
    const row = document.createElement('div');
    row.className = 'repo-row';

    const primary = document.createElement('button');
    primary.type = 'button';
    primary.className = 'repo-primary';
    primary.title = 'update ' + repo.repo;
    primary.appendChild(repoDot(repo));
    const name = document.createElement('span');
    name.className = 'repo-name';
    name.textContent = repo.repo;
    primary.appendChild(name);
    primary.addEventListener('click', () => updateSkillRepo(repo));
    row.appendChild(primary);

    const meta = document.createElement('div');
    meta.className = 'repo-meta';
    meta.textContent = repo.package_count + ' package'
      + (repo.package_count === 1 ? '' : 's')
      + ' · ' + (repo.ref || 'default branch')
      + (repo.sha ? ' · ' + shortSha(repo.sha) : '');
    row.appendChild(meta);
    if (repo.last_error) {
      const error = document.createElement('div');
      error.className = 'repo-error';
      error.textContent = repo.last_error;
      row.appendChild(error);
    }
    const update = document.createElement('button');
    update.type = 'button';
    update.textContent = 'Update';
    update.addEventListener('click', () => updateSkillRepo(repo));
    const actions = document.createElement('div');
    actions.className = 'repo-actions';
    actions.appendChild(update);
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.textContent = repo.enabled ? 'Disable' : 'Enable';
    toggle.addEventListener('click', () => setSkillRepoEnabled(repo));
    actions.appendChild(toggle);
    const remove = document.createElement('button');
    remove.type = 'button';
    remove.className = 'danger';
    remove.textContent = 'Remove';
    remove.addEventListener('click', () => removeSkillRepo(repo));
    actions.appendChild(remove);
    row.appendChild(actions);

    skillsList.appendChild(row);
  }
}

async function updateSkillRepo(repo) {
  try {
    const response = await fetch(
      '/skills/repos/' + encodeURIComponent(repo.repo) + '/update',
      { method: 'POST' }
    );
    if (!response.ok) throw new Error('HTTP ' + response.status);
    toastOk('updated ' + repo.repo);
    await refreshSkillRepos();
  } catch (error) {
    showToast('update ' + repo.repo + ': ' + error.message, 'error');
  }
}

async function setSkillRepoEnabled(repo) {
  try {
    const response = await fetch(
      '/skills/repos/' + encodeURIComponent(repo.repo) + '/enabled',
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: !repo.enabled }),
      }
    );
    if (!response.ok) throw new Error('HTTP ' + response.status);
    await refreshSkillRepos();
  } catch (error) {
    showToast('skill repo: ' + error.message, 'error');
  }
}

async function removeSkillRepo(repo) {
  if (!window.confirm('Remove skill repo ' + repo.repo + ' and its packages?')) return;
  try {
    const response = await fetch(
      '/skills/repos/' + encodeURIComponent(repo.repo),
      { method: 'DELETE' }
    );
    if (!response.ok) throw new Error('HTTP ' + response.status);
    toastOk('removed ' + repo.repo);
    await refreshSkillRepos();
  } catch (error) {
    showToast('remove skill repo: ' + error.message, 'error');
  }
}
