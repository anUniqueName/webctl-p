(args => {
  const lines = () => [...new Set((document.body?.innerText || '').split(/\r?\n/).map(s => s.replace(/\s+/g, ' ').trim()).filter(s => s.length >= 2 && s.length <= 80))].slice(0, 2000);
  // 不看 opacity：提示框常以透明度 0 插入再淡入，插入瞬间按不可见处理会漏掉
  const visible = el => {
    if (!(el instanceof Element)) return false;
    const r = el.getBoundingClientRect(), s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.display !== 'none' && s.visibility !== 'hidden';
  };
  if (args.mode === 'install') {
    window.__webctlObs?.observer?.disconnect();
    const state = {url: location.href, title: document.title, length: document.body?.innerText.length || 0, lines: lines(), added: []};
    state.observer = new MutationObserver(records => {
      for (const record of records) for (const node of record.addedNodes) {
        if (state.added.length >= 30) return;
        const elements = node.nodeType === 1 ? [node, ...node.querySelectorAll('*')] : [];
        for (const el of elements) {
          const text = (el.innerText || '').replace(/\s+/g, ' ').trim();
          if (text.length >= 2 && text.length <= 80 && visible(el) && !state.added.includes(text) && state.added.length < 30) state.added.push(text);
        }
      }
    });
    state.observer.observe(document.documentElement, {childList: true, subtree: true, characterData: true});
    window.__webctlObs = state;
    return true;
  }
  if (args.mode === 'probe') return {url: location.href, length: document.body?.innerText.length || 0};
  const state = window.__webctlObs;
  if (!state) return null;
  state.observer?.disconnect();
  const before = new Set(state.lines);
  return {old_url: state.url, old_title: state.title, old_length: state.length, url: location.href, title: document.title, length: document.body?.innerText.length || 0, added: [...new Set([...state.added, ...lines().filter(line => !before.has(line))])].slice(0, 10)};
})
