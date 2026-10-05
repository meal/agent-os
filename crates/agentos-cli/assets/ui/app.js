function formatTimes(root = document) {
  root.querySelectorAll('time[data-ts]').forEach(el => {
    const date = new Date(Number(el.dataset.ts));
    if (!Number.isNaN(date.getTime())) {el.textContent = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'short'}).format(date); el.dateTime = date.toISOString();}
  });
}
formatTimes();
if (window.htmx) {htmx.config.allowEval = false; htmx.config.allowScriptTags = false; htmx.config.includeIndicatorStyles = false;}
document.addEventListener('htmx:configRequest', e => {e.detail.headers['X-CSRF-Token'] = document.querySelector('meta[name=csrf-token]')?.content || '';});
document.addEventListener('htmx:beforeSwap', e => {if (e.detail.xhr.status >= 400) {e.detail.shouldSwap = false;document.querySelector('#request-error').textContent = 'Request failed. Refresh the view or reopen the launch link.';}});
document.addEventListener('htmx:afterSwap', () => {formatTimes();loadEvents();});
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
