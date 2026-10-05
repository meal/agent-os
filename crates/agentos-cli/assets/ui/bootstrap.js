const token = location.hash.slice(1);
history.replaceState(null, '', '/');
try {
  const response = await fetch('/session', {method: 'POST', credentials: 'same-origin', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({token})});
  if (response.ok) location.replace('/tasks');
  else document.querySelector('[data-error]').textContent = 'Open the launch link printed by agentos ui.';
} catch { document.querySelector('[data-error]').textContent = 'The local server is unavailable. Reopen the launch link when it is running.'; }
