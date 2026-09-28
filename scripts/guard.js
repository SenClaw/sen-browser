function (node) {
  // The scoped click/select guard, recomputed against the live page.
  const c = globalThis.__senclaw;
  if (!c || !c.pageKey) return null;
  return [c.pageKey(), c.guard(c.nodes.get(node))];
}
