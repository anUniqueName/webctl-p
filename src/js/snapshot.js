(args => {
  const roots = [];
  const cross = [];
  const visit = root => {
    roots.push(root);
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) visit(el.shadowRoot);
      if (el.tagName === 'IFRAME') {
        try {
          if (el.contentDocument) visit(el.contentDocument);
          else cross.push(el);
        } catch (_) {
          cross.push(el);
        }
      }
    }
  };
  visit(document);
  const visible = el => {
    const rect = el.getBoundingClientRect();
    const style = getComputedStyle(el);
    if (rect.width <= 0 || rect.height <= 0 || style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) <= 0) return false;
    for (let node = el; node; node = node.parentElement) if (node.getAttribute?.('aria-hidden') === 'true') return false;
    return true;
  };
  const find = target => {
    if (!target) return document;
    const ref = /^@?(e\d+)$/.exec(target)?.[1];
    // 和其他命令一样：选择器命中多个时先取第一个可见的
    if (!ref) {
      const all = [...document.querySelectorAll(target)];
      return all.find(visible) || all[0] || null;
    }
    for (const root of roots) {
      const found = root.querySelector(`[data-webctl-ref="${ref}"]`);
      if (found) return found;
    }
    return null;
  };
  const scope = find(args.target);
  if (!scope) throw new Error(/^@?e\d+$/.test(args.target || '') ? `编号 ${args.target} 已失效，请重新 snapshot` : `找不到元素：${args.target}`);
  // 先用旧编号找到 --in 指定的范围，再清除旧编号
  for (const root of roots) {
    for (const el of root.querySelectorAll('[data-webctl-ref]')) el.removeAttribute('data-webctl-ref');
  }
  const roleNames = new Set(['button','link','checkbox','radio','tab','menuitem','menuitemcheckbox','menuitemradio','option','switch','combobox','textbox','searchbox','slider','treeitem']);
  const selected = [];
  const inside = (parent, el) => {
    for (let node = el; node; ) {
      if (node === parent) return true;
      node = node.parentNode || node.getRootNode?.().host;
    }
    return false;
  };
  // 不用 tagName.toLowerCase()：拦截页的脚本会改写 DOM 属性，实测有元素的 tagName 取到 undefined
  const tagOf = el => el.localName || String(el.nodeName || '?');
  // 单个元素读取出错（属性被页面改坏）就跳过它、最后报个数，不让整个快照失败
  let skipped = 0;
  for (const root of roots) {
    for (const el of root.querySelectorAll('*')) {
      try {
        if (scope !== document && !inside(scope, el)) continue;
        const tag = tagOf(el);
        const role = el.getAttribute('role');
        const native = (tag === 'a' && el.hasAttribute('href')) || tag === 'button' || (tag === 'input' && el.type !== 'hidden') || ['textarea','select','summary'].includes(tag) || (el.hasAttribute('contenteditable') && ['','true'].includes(el.getAttribute('contenteditable')));
        const tabindex = el.hasAttribute('tabindex') && Number(el.getAttribute('tabindex')) >= 0;
        const pointer = getComputedStyle(el).cursor === 'pointer' && !selected.some(parent => parent.contains(el));
        if ((native || roleNames.has(role) || tabindex || pointer) && visible(el)) selected.push(el);
      } catch (_) {
        skipped++;
      }
    }
  }
  const clean = text => (text || '').replace(/\s+/g, ' ').trim();
  const name = el => {
    const labelled = (el.getAttribute('aria-labelledby') || '').split(/\s+/).map(id => clean(el.ownerDocument.getElementById(id)?.innerText)).filter(Boolean).join(' ');
    const label = el.id ? clean(el.ownerDocument.querySelector(`label[for="${CSS.escape(el.id)}"]`)?.innerText) : '';
    const outer = clean(el.closest('label')?.innerText);
    return clean(el.getAttribute('aria-label')) || labelled || label || outer || (el.tagName === 'SELECT' ? '' : clean(el.innerText).slice(0, 60)) || clean(el.placeholder) || clean(el.title) || clean(el.alt) || (/^(button|submit|reset)$/.test(el.type) ? clean(el.value) : '');
  };
  const kind = el => {
    if (el.getAttribute('role')) return el.getAttribute('role');
    if (el.tagName === 'INPUT') return /^(text|search|email|password|number|tel|url)$/.test(el.type) ? 'textbox' : el.type;
    return tagOf(el);
  };
  const lines = [];
  let number = 0;
  for (const el of selected) {
    number++;
    try {
      el.setAttribute('data-webctl-ref', `e${number}`);
      if (number > args.max) continue;
      let line = `[e${number}] ${kind(el)}`;
      const label = name(el);
      if (label) line += ` "${label}"`;
      if (el.matches('input:not([type=checkbox]):not([type=radio]),textarea,[contenteditable]')) {
        const value = el.matches('[contenteditable]') ? el.innerText : el.value;
        if (el.type === 'password') line += ` value="${value.length} 个字符"`;
        else if (value) line += ` value="${clean(value).slice(0, 40)}"`;
      }
      if (el.placeholder) line += ` placeholder="${clean(el.placeholder)}"`;
      if (el.matches('input[type=checkbox],input[type=radio]')) line += el.checked ? ' checked' : ' unchecked';
      if (el.disabled) line += ' disabled';
      if (el.matches('a[href]')) {
        const url = new URL(el.href, location.href);
        line += ` href=${url.origin === location.origin ? url.pathname + url.search + url.hash : url.href}`.slice(0, 86);
      }
      if (el.tagName === 'SELECT') {
        line += ` selected="${clean(el.selectedOptions[0]?.text || el.value)}"`;
        const options = [...el.options].slice(0, 10).map(option => clean(option.text)).join(',');
        line += ` options=${options}${el.options.length > 10 ? '…' : ''}`;
      }
      const rect = el.getBoundingClientRect();
      if (rect.bottom < 0 || rect.top > innerHeight || rect.right < 0 || rect.left > innerWidth) line += ' (视口外)';
      lines.push(line);
    } catch (_) {
      // 编号照样占用，列表里会空一个号；不收回，免得和已经写到元素上的编号对不上
      skipped++;
    }
  }
  for (const frame of cross) lines.push(`[跨域 iframe] src=${(frame.src || '').slice(0, 120)}`);
  const maxScroll = Math.max(0, document.documentElement.scrollHeight - innerHeight);
  const header = [`url: ${location.href}`, `title: ${document.title}`, `scroll: ${Math.round(scrollY)}/${maxScroll}  viewport: ${innerWidth}x${innerHeight}`];
  if (number > args.max) lines.push(`… 共 ${number} 个元素，已截断到 ${args.max} 个`);
  if (skipped) lines.push(`${skipped} 个元素读取出错，已跳过`);
  return [...header, ...lines].join('\n');
})
