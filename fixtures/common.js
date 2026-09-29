// Shared fixture helpers. rec() posts an outcome event the bench checks.
window.rec = (o) => fetch('/__record', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(o) });
window.q = (s, r = document) => r.querySelector(s);
window.qa = (s, r = document) => [...r.querySelectorAll(s)];
window.later = (ms) => fetch('/api/delay?ms=' + ms).then((r) => r.json());
