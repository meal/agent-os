function formatTimes(root = document) {
  root.querySelectorAll('time[data-ts]').forEach(el => {
    const date = new Date(Number(el.dataset.ts));
    if (!Number.isNaN(date.getTime())) {el.textContent = new Intl.DateTimeFormat(undefined, {dateStyle: 'medium', timeStyle: 'short'}).format(date); el.dateTime = date.toISOString();}
  });
}
formatTimes();
