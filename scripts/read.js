function (maxChars) {
  // The whole page's readable text, for an LLM to read or answer from.
  const text = ((document.body && document.body.innerText) || '').replace(/\n{3,}/g, '\n\n').trim();
  const links = [...document.querySelectorAll('a[href]')].slice(0, 200)
    .map(a => ({ text: (a.innerText || a.getAttribute('aria-label') || '').trim().slice(0, 120), href: a.href }))
    .filter(l => l.text && /^https?:/.test(l.href));
  return { url: location.href, title: document.title, text: text.slice(0, maxChars), truncated: text.length > maxChars, links };
}
