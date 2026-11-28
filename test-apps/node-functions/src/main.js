const res = await fetch('/api/hello?name=site');
document.querySelector('#out').textContent = await res.text();
