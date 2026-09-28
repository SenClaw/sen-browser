function (action) {
  // Resolve an observed node to current viewport coordinates and prove nothing
  // covers it. Only `<select>` is changed here; every other input is sent by the
  // runtime as trusted CDP events at the returned point.
  const c = globalThis.__senclaw;
  const e = c && c.nodes.get(action.node);
  if (!e || !e.isConnected) return { ok: false, reason: 'detached' };
  if (e.matches(':disabled') || e.closest('[aria-disabled="true"],[inert]')) return { ok: false, reason: 'disabled' };
  if (!e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return { ok: false, reason: 'hidden' };
  if (action.kind === 'fill' && (e.readOnly || e.getAttribute('aria-readonly') === 'true')) return { ok: false, reason: 'readonly' };
  let dx = 0, dy = 0, win = e.ownerDocument.defaultView;
  while (win && win !== window && win.frameElement) {
    const f = win.frameElement, fr = f.getBoundingClientRect();
    dx += fr.x + f.clientLeft;
    dy += fr.y + f.clientTop;
    win = f.ownerDocument.defaultView;
  }
  const r = e.getBoundingClientRect(), x = dx + r.x + r.width / 2, y = dy + r.y + r.height / 2;
  if (!r.width || !r.height || x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return { ok: false, reason: 'offscreen' };
  // Descend through same-origin frames and open shadow roots to the element a
  // real click at (x, y) would reach.
  let hit = document.elementFromPoint(x, y), hx = x, hy = y;
  for (let depth = 0; hit && depth < 16; depth++) {
    if (hit.shadowRoot) {
      const inner = hit.shadowRoot.elementFromPoint(hx, hy);
      if (inner && inner !== hit) { hit = inner; continue; }
    }
    if (hit.tagName === 'IFRAME' || hit.tagName === 'FRAME') {
      let doc = null;
      try { doc = hit.contentDocument; } catch (_) { doc = null; }
      if (!doc) break;
      const fr = hit.getBoundingClientRect();
      hx -= fr.x + hit.clientLeft;
      hy -= fr.y + hit.clientTop;
      const inner = doc.elementFromPoint(hx, hy);
      if (!inner) break;
      hit = inner;
      continue;
    }
    break;
  }
  let n = hit, reached = false;
  while (n) {
    if (n === e) { reached = true; break; }
    n = n.parentNode || n.host || null;
  }
  if (!reached) {
    const by = hit ? (hit.tagName.toLowerCase() + (hit.id ? '#' + hit.id : '') +
      (typeof hit.className === 'string' && hit.className ? '.' + hit.className.trim().split(/\s+/).join('.') : '')) : 'nothing';
    return { ok: false, reason: 'covered', by };
  }
  if (action.kind === 'select') {
    if (e.tagName !== 'SELECT' || ![...e.options].some(o => o.value === action.value && !o.disabled && !o.closest('optgroup[disabled]')))
      return { ok: false, reason: 'select_value' };
    e.value = action.value;
    e.dispatchEvent(new Event('input', { bubbles: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true, x, y, selected: true };
  }
  return { ok: true, x, y };
}
