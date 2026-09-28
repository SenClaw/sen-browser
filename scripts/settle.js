function (node, kind) {
  // Wait for useful state, not for an arbitrary delay: two animation frames or
  // 50 ms after an interaction; after typing into an editable combobox, until
  // its suggestions are visible, capped at 200 ms.
  return new Promise(resolve => {
    const c = globalThis.__senclaw;
    const field = c && c.nodes.get(node);
    const autocomplete = kind === 'fill' && !!field && field.getAttribute('role') === 'combobox';
    let frames = 0, stopped = false;
    const finish = () => { if (!stopped) { stopped = true; resolve(true); } };
    setTimeout(finish, autocomplete ? 200 : 50);
    const ready = () => {
      if (stopped) return;
      const ids = ((field && (field.getAttribute('aria-controls') || field.getAttribute('aria-owns'))) || '')
        .split(/\s+/).filter(Boolean);
      const roots = ids.length ? ids.map(id => document.getElementById(id)).filter(Boolean) : [document];
      const options = roots.flatMap(root => [...root.querySelectorAll('[role="option"]')]);
      if (++frames >= 2 && (!autocomplete || options.some(e => {
        const r = e.getBoundingClientRect();
        return r.width && r.height && r.bottom > 0 && r.top < innerHeight &&
          e.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true });
      }))) finish();
      else requestAnimationFrame(ready);
    };
    requestAnimationFrame(ready);
  });
}
