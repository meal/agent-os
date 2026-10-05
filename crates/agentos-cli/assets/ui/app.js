function formatTimes(root = document) {
  root.querySelectorAll('time[data-ts]').forEach(el => {
    const date = new Date(Number(el.dataset.ts));
    if (!Number.isNaN(date.getTime())) {el.textContent = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'short'}).format(date); el.dateTime = date.toISOString();}
  });
}
formatTimes();
if (window.htmx) {htmx.config.allowEval = false; htmx.config.allowScriptTags = false; htmx.config.includeIndicatorStyles = false;}
document.addEventListener('htmx:configRequest', e => {e.detail.headers['X-CSRF-Token'] = document.querySelector('meta[name=csrf-token]')?.content || '';});
document.addEventListener('htmx:beforeSwap', e => {if (e.detail.xhr.status >= 400) {e.detail.shouldSwap = false;const message = new DOMParser().parseFromString(e.detail.xhr.responseText, 'text/html').querySelector('.error p')?.textContent || 'Refresh the view or reopen the launch link.';document.querySelector('#request-error').textContent = `Request failed (${e.detail.xhr.status}). ${message}`;}});
document.addEventListener('htmx:afterSwap', () => {formatTimes();selectView();loadEvents().catch(() => {document.querySelector('#freshness').textContent = 'Stale · event read failed. Retrying…';});});
let polling = false;
let lastSuccess = new Date();
async function loadEvents() {
  let page = document.querySelector('.event-page');
  const task = document.querySelector('[data-task]')?.dataset.task;
  if (!page || !task) return;
  while (page.dataset.more === 'true') {
    const response = await fetch(`/tasks/${task}/events?after=${page.dataset.after}`);
    if (!response.ok) throw new Error('Event read failed');
    const doc = new DOMParser().parseFromString(await response.text(), 'text/html');
    const next = doc.querySelector('.event-page');
    const seen = new Set([...page.querySelectorAll('[data-seq]')].map(el => el.dataset.seq));
    next.querySelectorAll('[data-seq]').forEach(el => {if (!seen.has(el.dataset.seq)) page.querySelector('tbody').append(el);});
    page.dataset.after = next.dataset.after; page.dataset.more = next.dataset.more;
  }
  formatTimes(page);
}
async function pollTask() {
  const task = document.querySelector('[data-task]')?.dataset.task;
  if (!task || document.hidden || polling) return;
  polling = true;
  try {
    const response = await fetch(`/tasks/${task}/status`);
    if (!response.ok) throw new Error('Status read failed');
    const html = await response.text();document.querySelector('#status').innerHTML = html;
    await loadEvents();lastSuccess = new Date();
    document.querySelector('#freshness').textContent = 'Updated ' + lastSuccess.toLocaleTimeString();
    document.dispatchEvent(new CustomEvent('agentos:state'));
  } catch {document.querySelector('#freshness').textContent = 'Stale · last update ' + lastSuccess.toLocaleTimeString() + '. Retrying…';}
  finally {polling = false;}
}
const pollTimer = setInterval(() => {
  if (document.querySelector('[data-active=true]')) pollTask();
}, 2000);
document.addEventListener('visibilitychange', () => {if (!document.hidden) pollTask();});

function selectView() {
 const pane = document.querySelector('#pane');
 if (!pane) return;
 pane.querySelectorAll('[data-view-section]').forEach(section => {section.hidden = pane.dataset.view && section.dataset.viewSection !== pane.dataset.view;});
}
document.querySelectorAll('[role=tab]').forEach(tab => {
 tab.addEventListener('click', () => {
  document.querySelectorAll('[role=tab]').forEach(other => {other.setAttribute('aria-selected', String(other === tab));other.removeAttribute('aria-current');});
  tab.setAttribute('aria-current', 'page');document.querySelector('#pane').dataset.view = tab.dataset.view || 'contract';
 });
 tab.addEventListener('keydown', event => {
  const tabs = [...document.querySelectorAll('[role=tab]')];let index = tabs.indexOf(tab);
  if (event.key === 'ArrowRight') index = (index + 1) % tabs.length;
  else if (event.key === 'ArrowLeft') index = (index + tabs.length - 1) % tabs.length;
  else if (event.key === 'Home') index = 0;
  else if (event.key === 'End') index = tabs.length - 1;
  else return;
  event.preventDefault();tabs[index].focus();
 });
});
async function refreshActions() {
  const task = document.querySelector('[data-task]')?.dataset.task;
  if (!task || document.hidden) return;
  const response = await fetch(`/tasks/${task}/actions`);
  if (!response.ok) return;
  const html = await response.text();const actions = document.querySelector('#run-actions');
  if (actions) {actions.innerHTML = html;htmx.process(actions);}
}
const actionHost = document.querySelector('[data-task]');
if (actionHost) refreshActions().catch(() => {});
document.addEventListener('agentos:state', () => refreshActions().catch(() => {}));
document.addEventListener('htmx:afterSwap', e => {if (e.detail.target.id === 'status') {refreshActions().catch(() => {});pollTask();}});

document.querySelector('#contract-file')?.addEventListener('change', async event => {
 const file = event.target.files[0];
 if (!file) return;
 if (file.size > 256 * 1024) {document.querySelector('#request-error').textContent = 'Contract exceeds 256 KiB.';return;}
 document.querySelector('#contract-json').value = await file.text();
});

document.addEventListener('htmx:afterSwap', event => {
 if (event.detail.target.id === 'export-result') event.detail.target.querySelector('[data-download]')?.click();
});
