function (maxCandidates, textChars, markerOnly) {
  // Runs in SenClaw's isolated world: the page shares the DOM but never sees
  // this cache, so it can neither detect nor tamper with node identities.
  if (!document.body) return null;
  const cache = globalThis.__senclaw || (globalThis.__senclaw = { ids: new WeakMap(), nodes: new Map(), next: 1 });
  const identity = e => {
    if (!cache.ids.has(e)) cache.ids.set(e, cache.next++);
    const id = cache.ids.get(e);
    cache.nodes.set(id, e);
    return id;
  };
  for (const [id, e] of cache.nodes) if (!e.isConnected) cache.nodes.delete(id);

  const SENSITIVE_AUTOCOMPLETE = ['current-password', 'new-password', 'one-time-code', 'cc-number', 'cc-csc', 'cc-exp', 'cc-exp-month', 'cc-exp-year'];
  const SENSITIVE_NAME = /(otp|one.?time|passcode|cvv|cvc|card.?number|security.?code|pin\b|mã.?otp|mật.?khẩu|mat.?khau)/i;
  const safe = e => !['password', 'file', 'hidden'].includes(e.type);
  const sensitive = e => SENSITIVE_AUTOCOMPLETE.includes((e.getAttribute('autocomplete') || '').toLowerCase()) ||
    SENSITIVE_NAME.test([e.name, e.id, e.getAttribute('aria-label'), e.getAttribute('placeholder')].filter(Boolean).join(' '));
  const visible = e => !e.closest('[aria-hidden="true"],[inert]') &&
    e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true });
  const byId = (e, id) => {
    const root = e.getRootNode();
    return (root && root.getElementById ? root.getElementById(id) : null) || e.ownerDocument.getElementById(id);
  };
  const name = (e, seen = new Set()) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    const referenced = (e.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
      .map(id => name(byId(e, id), seen)).filter(Boolean).join(' ');
    return (referenced || e.getAttribute('aria-label') ||
      [...(e.labels || [])].map(l => name(l, seen)).filter(Boolean).join(' ') ||
      (['button', 'submit', 'reset'].includes(e.type) ? e.value : '') || e.getAttribute('alt') ||
      (e.tagName === 'INPUT' ? '' : [...e.childNodes].map(n => n.nodeType === 3 ? n.textContent :
        n.nodeType === 1 && n.getAttribute('aria-hidden') !== 'true' ? name(n, seen) : '').join(' ').replace(/\s+/g, ' ').trim()) ||
      e.getAttribute('title') || e.getAttribute('placeholder') || '').trim();
  };
  const roles = ['button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'menuitemradio',
    'menuitemcheckbox', 'option', 'gridcell', 'combobox', 'textbox', 'searchbox', 'spinbutton'];
  const selector = 'a[href],button,input,textarea,select,summary,[contenteditable="true"],' +
    roles.map(role => '[role="' + role + '"]').join(',');
  const role = e => {
    const explicit = e.getAttribute('role');
    if (roles.includes(explicit)) return explicit;
    if (e.tagName === 'BUTTON' || e.tagName === 'SUMMARY') return 'button';
    if (e.tagName === 'A') return 'link';
    if (e.tagName === 'SELECT') return 'combobox';
    if (e.tagName === 'TEXTAREA' || e.isContentEditable) return 'textbox';
    if (e.tagName === 'INPUT') {
      if (['checkbox', 'radio'].includes(e.type)) return e.type;
      if (['button', 'submit', 'reset', 'image'].includes(e.type)) return 'button';
      if (e.type === 'search') return 'searchbox';
      if (e.type === 'number') return 'spinbutton';
      if (['text', 'email', 'url', 'tel', ''].includes(e.type)) return 'textbox';
    }
    return null;
  };

  // Roots: the document, every open shadow root, every same-origin frame.
  // A frame's rects are frame-relative, so each root carries its offset.
  let shadowRoots = 0, frames = 0, crossOrigin = 0;
  const roots = [];
  const addRoot = (root, dx, dy) => {
    roots.push({ root, dx, dy });
    const walker = (root.ownerDocument || root).createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    let el;
    while ((el = walker.nextNode())) {
      if (el.shadowRoot) { shadowRoots++; addRoot(el.shadowRoot, dx, dy); }
      if (el.tagName === 'IFRAME' || el.tagName === 'FRAME') {
        frames++;
        let doc = null;
        try { doc = el.contentDocument; } catch (_) { doc = null; }
        if (!doc || !doc.body) { crossOrigin++; continue; }
        const r = el.getBoundingClientRect();
        if (!r.width || !r.height || !visible(el)) continue;
        addRoot(doc, dx + r.x + el.clientLeft, dy + r.y + el.clientTop);
      }
    }
  };
  addRoot(document, 0, 0);

  const pageKey = () => [performance.timeOrigin, location.href, scrollX, scrollY, innerWidth, innerHeight,
    [...document.querySelectorAll('input,textarea,select')].filter(safe)
      .map(e => [identity(e), e.value, e.checked, e.selectedIndex, e.disabled, e.readOnly])];
  const guard = e => {
    if (!e || !e.isConnected || !visible(e)) return null;
    const scope = e.closest('form,dialog,[role="dialog"],article,li,tr,[role="row"]') || e.parentElement;
    return [identity(e), role(e), name(e), e.value ?? null, e.checked ?? null, e.selectedIndex ?? null,
      e.readOnly ?? null, e.matches(':disabled'), e.getAttribute('aria-disabled'),
      e.getAttribute('aria-expanded'), e.getAttribute('aria-checked'), e.getAttribute('aria-selected'),
      e.getAttribute('aria-pressed'), e.getAttribute('href'), (scope && scope.innerText || '').slice(0, 6000)];
  };
  cache.pageKey = pageKey;
  cache.guard = guard;

  const actions = [];
  for (const { root, dx, dy } of roots) {
    for (const e of root.querySelectorAll(selector)) {
      if (!safe(e) || !visible(e) || e.matches(':disabled') || e.closest('[aria-disabled="true"]')) continue;
      const r = e.getBoundingClientRect(), x = dx + r.x + r.width / 2, y = dy + r.y + r.height / 2, rname = role(e);
      if (!rname || r.width <= 0 || r.height <= 0 || x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) continue;
      if (rname === 'gridcell' && e.querySelector('button,[role="button"]')) continue;
      const base = { node: identity(e), role: rname, label: name(e) || rname };
      for (const key of ['checked', 'selected', 'expanded']) {
        const value = e.getAttribute('aria-' + key);
        if (value !== null) base[key] = value;
      }
      if (['checkbox', 'radio'].includes(e.type)) base.checked = String(e.checked);
      // A toggle button (like, follow, mute) says whether it is on the same way.
      else if (base.checked === undefined && e.getAttribute('aria-pressed') !== null) base.checked = e.getAttribute('aria-pressed');
      if (sensitive(e)) base.sensitive = true;
      if (dx || dy) base.frame = true;
      if (e.tagName === 'SELECT') {
        for (const o of e.options) if (!o.selected && !o.disabled && !o.closest('optgroup[disabled]'))
          actions.push({ ...base, kind: 'select', value: o.value,
            current_value: [...e.selectedOptions].map(o => o.label).join(', '), label: base.label + ' → ' + o.label });
      } else {
        const editable = !e.readOnly && e.getAttribute('aria-readonly') !== 'true' &&
          (['textbox', 'searchbox', 'spinbutton'].includes(rname) ||
            (rname === 'combobox' && ['INPUT', 'TEXTAREA'].includes(e.tagName)));
        const raw = 'value' in e && e.tagName !== 'BUTTON' ? String(e.value) :
          e.isContentEditable || rname === 'combobox' ? e.innerText.trim() : '';
        // A card number, CVV or code the person typed never leaves the page:
        // the model only learns that the field is filled.
        const value = base.sensitive ? '' : raw;
        if (base.sensitive && raw) base.filled = true;
        actions.push({ ...base, kind: editable ? 'fill' : 'click', value });
        if (editable) actions.push({ ...base, kind: 'click', value, label: 'Open ' + base.label });
      }
    }
  }

  const words = [], walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  const range = document.createRange();
  let node, length = 0;
  while ((node = walker.nextNode()) && length < textChars) {
    const value = node.textContent.trim(), parent = node.parentElement;
    if (!value || !parent || parent.closest('script,style,noscript,template') || !visible(parent)) continue;
    range.selectNodeContents(node);
    const r = range.getBoundingClientRect();
    if (r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < innerHeight && r.right > 0 && r.left < innerWidth) {
      words.push(value);
      length += value.length;
    }
  }
  const text = words.join('\n').slice(0, textChars), height = document.documentElement.scrollHeight;
  const page_key = pageKey();
  // Compare meaning and identity. Geometry is re-read and hit-tested just before input.
  const semantics = actions.map(({ rect, ...action }) => action);
  const marker = [performance.timeOrigin, location.href, scrollX, scrollY, innerWidth, innerHeight,
    document.title, text, semantics, page_key[6]];
  if (markerOnly) return { marker };

  const guards = {};
  for (const a of actions) if (!(a.node in guards)) guards[a.node] = guard(cache.nodes.get(a.node));
  const omitted_actions = Math.max(0, actions.length - maxCandidates);
  actions.splice(maxCandidates);
  actions.forEach((a, i) => { a.id = 'e' + (i + 1); });

  if (scrollY + innerHeight < height - 2) actions.push({ id: 'scroll_down', kind: 'scroll', label: 'Scroll down', delta: 560 });
  if (scrollY > 0) actions.push({ id: 'scroll_up', kind: 'scroll', label: 'Scroll up', delta: -560 });
  const active = document.activeElement;
  if (active && active !== document.body && (active.isContentEditable || active.tagName === 'TEXTAREA' ||
    (active.tagName === 'INPUT' && !['checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'hidden', 'password'].includes(active.type)))) {
    // Enter submits: what it would submit decides whether it needs the person.
    const enter = { id: 'key_enter', kind: 'key', key: 'Enter', label: 'Press Enter in ' + (name(active) || role(active) || 'the focused field') };
    if (active.tagName === 'TEXTAREA' || active.isContentEditable) enter.multiline = true;
    const form = active.form || active.closest('form');
    if (active.type === 'search' || role(active) === 'searchbox' || (form && form.getAttribute('role') === 'search')) enter.search = true;
    const submit = form && form.querySelector('button[type=submit], input[type=submit], button:not([type])');
    if (submit) enter.submit = name(submit) || submit.value || '';
    actions.push(enter);
  }
  if (history.length > 1) actions.push({ id: 'go_back', kind: 'back', label: 'Go back to the previous page' });
  actions.push({ id: 'wait', kind: 'wait', label: 'Wait for the page to update' });

  let canvas = false;
  for (const c of document.querySelectorAll('canvas')) {
    const r = c.getBoundingClientRect();
    if (r.width * r.height > innerWidth * innerHeight * 0.25 && visible(c)) { canvas = true; break; }
  }
  return {
    url: location.href, title: document.title, w: innerWidth, h: innerHeight, text,
    scroll: { y: scrollY, height }, actions, marker, page_key, guards, omitted_actions,
    coverage: { frames, cross_origin_frames: crossOrigin, open_shadow_roots: shadowRoots, canvas },
  };
}
