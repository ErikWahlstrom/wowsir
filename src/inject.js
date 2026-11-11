// wowsir page library. Runs in an isolated world (invisible to page scripts),
// sharing the DOM with the page. Everything the agent sees is produced here
// in a single pass, so one CDP round-trip = one snapshot.
(() => {
if (globalThis.__wowsir) return;
const K = (globalThis.__wowsir = {});

// ---------------------------------------------------------------- shadow roots
// Closed shadow roots are invisible to page JS; the daemon finds them over CDP
// (DOM.describeNode) and registers them here, so they are walked like open ones.
const CLOSED = new WeakMap(); // host -> closed ShadowRoot
const CHECKED = new WeakSet(); // upgraded custom elements already asked about
const rootOf = (el) => el.shadowRoot || CLOSED.get(el) || null;
K.addClosed = (host, root) => { CLOSED.set(host, root); CHECKED.add(host); };
// custom elements that might hide a closed root (asked once each, after they are defined)
K.closedCandidates = () => {
  const out = [];
  for (const el of allElements(document)) {
    if (!el.localName.includes('-') || rootOf(el) || CHECKED.has(el) || !el.matches(':defined')) continue;
    CHECKED.add(el);
    out.push(el);
  }
  return out.length ? out : null;
};

// ---------------------------------------------------------------- refs
let nextRef = 1;
const refOf = new WeakMap(); // Element -> n
const byRef = new Map(); // n -> WeakRef<Element>
function ref(el) {
  let n = refOf.get(el);
  if (!n) {
    n = nextRef++;
    refOf.set(el, n);
    byRef.set(n, new WeakRef(el));
  }
  return n;
}
K.init = (n) => { if (n > nextRef) nextRef = n; };
K.refOf = (el) => ref(el);
// where a frame's content starts on the page (its content box), for frame-relative points
K.frameBox = (el) => {
  const r = el.getBoundingClientRect(), cs = getComputedStyle(el);
  return { x: r.left + el.clientLeft + parseFloat(cs.paddingLeft), y: r.top + el.clientTop + parseFloat(cs.paddingTop) };
};
K.nextRef = () => nextRef;

// ---------------------------------------------------------------- dom quiet
let lastMut = performance.now();
new MutationObserver(() => { lastMut = performance.now(); })
  .observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
K.quiet = () => Math.round(performance.now() - lastMut);

// ---------------------------------------------------------------- helpers
const norm = (s) => (typeof s === 'string' ? s : '').replace(/[\u200b-\u200d\u2060\ufeff\ue000-\uf8ff]/g, '').replace(/[\s\u00a0]+/g, ' ').trim();
const trunc = (s, n) => (s.length > n ? s.slice(0, n - 1) + '…' : s);
const q = (s) => '"' + s.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"';

class Retry extends Error { constructor(m) { super(m); this.retry = true; } }

const SKIP = new Set(['script', 'style', 'noscript', 'template', 'head', 'meta', 'link', 'title', 'base', 'datalist']);
const INLINE = new Set(['a', 'abbr', 'b', 'bdi', 'bdo', 'cite', 'code', 'data', 'dfn', 'em', 'i', 'kbd', 'mark', 'q', 's',
  'samp', 'small', 'span', 'strong', 'sub', 'sup', 'time', 'u', 'var', 'font', 'del', 'ins', 'label', 'br', 'wbr', 'img', 'svg']);
const INTERACTIVE = new Set(['link', 'button', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox', 'listbox', 'slider',
  'spinbutton', 'switch', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option', 'treeitem', 'clickable']);
const CONTAINER = new Set(['navigation', 'main', 'banner', 'contentinfo', 'complementary', 'form', 'dialog', 'alertdialog',
  'list', 'table', 'grid', 'region', 'group', 'menu', 'menubar', 'tablist', 'tabpanel', 'radiogroup', 'toolbar', 'tree',
  'search', 'alert', 'status', 'article', 'heading', 'row', 'listbox']);
const NAME_FROM_CONTENT = new Set(['button', 'link', 'heading', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
  'option', 'treeitem', 'checkbox', 'radio', 'switch', 'cell', 'gridcell', 'columnheader', 'rowheader', 'clickable', 'tooltip']);
const INPUT_ROLE = { button: 'button', submit: 'button', reset: 'button', image: 'button', checkbox: 'checkbox',
  radio: 'radio', range: 'slider', number: 'spinbutton', search: 'searchbox', color: 'button', file: 'button' };
const BLOCKISH = (el) => !INLINE.has(el.localName);

function kids(el) {
  const sr = rootOf(el);
  if (sr) return sr.childNodes;
  if (el.localName === 'slot') {
    const a = el.assignedNodes({ flatten: true });
    return a.length ? a : el.childNodes;
  }
  if (el.localName === 'iframe' || el.localName === 'frame') {
    try { const d = el.contentDocument; if (d && d.body) return [d.body]; } catch (e) {}
    return [];
  }
  return el.childNodes;
}
function parentOf(n) { return n.parentNode instanceof ShadowRoot ? n.parentNode.host : (n.parentElement || frameOf(n)); }
function frameOf(n) { const d = n.ownerDocument || n; try { return d.defaultView && d.defaultView.frameElement; } catch (e) { return null; } }
function composedContains(a, b) { for (let n = b; n; n = parentOf(n)) if (n === a) return true; return false; }

function inSectioning(el) {
  for (let p = el.parentElement; p; p = p.parentElement)
    if (['article', 'aside', 'main', 'nav', 'section'].includes(p.localName)) return true;
  return false;
}

function roleOf(el) {
  const r = el.getAttribute('role');
  if (r) { const x = r.trim().split(/\s+/)[0]; return x === 'presentation' || x === 'none' ? null : x; }
  const t = el.localName;
  switch (t) {
    // <a> without href: frameworks often attach the click handler in JS (menus, "Sign in" items)
    case 'a': case 'area': return el.hasAttribute('href') ? 'link' : (norm(el.textContent) ? 'clickable' : null);
    case 'button': case 'summary': return 'button';
    case 'input': {
      const ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'hidden') return null;
      if (ty === 'checkbox' && el.getAttribute('switch') !== null) return 'switch';
      return INPUT_ROLE[ty] || 'textbox';
    }
    case 'select': return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
    case 'textarea': return 'textbox';
    case 'h1': case 'h2': case 'h3': case 'h4': case 'h5': case 'h6': return 'heading';
    case 'img': return el.getAttribute('alt') ? 'img' : null;
    case 'nav': return 'navigation';
    case 'main': return 'main';
    case 'header': return inSectioning(el) ? null : 'banner';
    case 'footer': return inSectioning(el) ? null : 'contentinfo';
    case 'aside': return 'complementary';
    case 'form': return 'form';
    case 'search': return 'search';
    case 'dialog': return 'dialog';
    case 'ul': case 'ol': case 'menu': return 'list';
    case 'table': return layoutTable(el) ? null : 'table';
    case 'tr': return layoutTable(el.closest('table')) ? null : 'row';
    case 'td': return layoutTable(el.closest('table')) ? null : 'cell';
    case 'th': return layoutTable(el.closest('table')) ? null : 'columnheader';
    case 'fieldset': case 'details': return 'group';
    case 'option': return 'option';
    case 'progress': return 'progressbar';
    case 'iframe': return 'iframe';
    case 'section': return el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby') ? 'region' : null;
  }
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
  return null;
}

// Layout tables (tables used for page structure, e.g. HN) are not data: a
// table is "layout" if it nests another table or is marked presentational.
const layoutCache = new WeakMap();
function layoutTable(t) {
  if (!t) return true;
  let v = layoutCache.get(t);
  if (v === undefined) {
    v = !!t.querySelector('table') || t.getAttribute('role') === 'presentation' || !!(t.parentElement && t.parentElement.closest('td') && t.parentElement.closest('table') && layoutTable(t.parentElement.closest('table')) && !t.querySelector('th'));
    layoutCache.set(t, v);
  }
  return v;
}

// "clickable": generic element that looks clickable (pointer cursor that starts here, onclick, tabindex)
function clickable(el) {
  const t = el.localName;
  if ((t === 'label' && el.control) || t === 'html' || t === 'body') return false; // control-less labels can be dropdown toggles
  if (el.hasAttribute('onclick')) return true;
  const ti = el.getAttribute('tabindex');
  if (ti !== null && +ti >= 0) return true;
  const cs = getComputedStyle(el);
  if (cs.cursor !== 'pointer') return false;
  const p = parentOf(el);
  if (p && p.nodeType === 1 && getComputedStyle(p).cursor === 'pointer') return false;
  // a wrapper around real controls is not itself the click target
  return !el.querySelector('a[href],button,input,select,textarea,[role=button],[role=link],[role=tab],[role=menuitem]');
}

function effectiveRole(el) {
  const r = roleOf(el);
  if (r) return r;
  return clickable(el) ? 'clickable' : null;
}

// Visible text of an element subtree (for names). Cheap: no layout except checkVisibility on elements.
function textOf(el, depth = 0) {
  let out = '';
  for (const n of kids(el)) {
    if (n.nodeType === 3) { out += n.data; continue; }
    if (n.nodeType !== 1) continue;
    const t = n.localName;
    if (SKIP.has(t) || t === 'select') continue;
    if (n.getAttribute('aria-hidden') === 'true') continue;
    if (t === 'img') { out += ' ' + (n.getAttribute('alt') || '') + ' '; continue; }
    if (t === 'svg') {
      const l = n.getAttribute('aria-label') || (n.querySelector('title') || {}).textContent || '';
      out += ' ' + l + ' '; continue;
    }
    if (t === 'input') { const ty = n.type; if (ty === 'submit' || ty === 'button' || ty === 'reset') out += ' ' + n.value + ' '; continue; }
    if (t === 'textarea') continue;
    if (t === 'br') { out += ' '; continue; }
    if (!n.checkVisibility() && getComputedStyle(n).display !== 'contents') continue;
    const al = n.getAttribute('aria-label');
    if (al && effectiveRoleCheap(n)) { out += ' ' + al + ' '; continue; }
    if (depth > 30) continue;
    const s = textOf(n, depth + 1);
    out += BLOCKISH(n) ? ' ' + s + ' ' : s;
  }
  return norm(out);
}
function effectiveRoleCheap(n) { return n.hasAttribute('role') || ['button', 'a', 'input'].includes(n.localName); }

function idText(el, ids) {
  const root = el.getRootNode();
  return norm(ids.split(/\s+/).map((id) => {
    const e = (root.getElementById && root.getElementById(id)) || el.ownerDocument.getElementById(id);
    return e ? textOf(e) || norm(e.getAttribute('aria-label')) : '';
  }).join(' '));
}

function nameOf(el, role) {
  let v = el.getAttribute('aria-labelledby');
  if (v) { const s = idText(el, v); if (s) return s; }
  v = norm(el.getAttribute('aria-label'));
  if (v) return v;
  const t = el.localName;
  if (t === 'input' || t === 'textarea' || t === 'select') {
    const ty = t === 'input' ? el.type : '';
    if (ty === 'submit' || ty === 'button' || ty === 'reset')
      return norm(el.value) || (ty === 'submit' ? 'Submit' : ty === 'reset' ? 'Reset' : '');
    if (ty === 'image') return norm(el.alt) || 'Submit';
    if (el.labels && el.labels.length) {
      const s = norm([...el.labels].map((l) => textOf(l)).join(' '));
      if (s) return s;
    }
    return norm(el.getAttribute('placeholder')) || norm(el.getAttribute('title')) || '';
  }
  if (t === 'img') return norm(el.alt);
  if (t === 'fieldset') { const lg = el.querySelector(':scope > legend'); if (lg) return norm(lg.textContent); }
  if (t === 'table') { const c = el.querySelector(':scope > caption'); if (c) return norm(c.textContent); }
  if (t === 'details') { const s = el.querySelector(':scope > summary'); if (s) return textOf(s); }
  if (role && NAME_FROM_CONTENT.has(role)) {
    const s = textOf(el);
    if (s) return s;
    // icon-only controls: fall back to a descendant's title/alt, then the link target
    const d = el.querySelector('[title],[alt],[aria-label]');
    const ds = d && norm(d.getAttribute('aria-label') || d.getAttribute('title') || d.getAttribute('alt'));
    if (ds) return ds;
    if (norm(el.getAttribute('title'))) return norm(el.getAttribute('title'));
    if (role === 'link') { const h = el.getAttribute('href') || ''; return h.length > 60 ? h.slice(0, 59) + '…' : h; }
  }
  if (t === 'dialog' || role === 'dialog' || role === 'alertdialog') {
    const h = el.querySelector('h1,h2,h3,h4,h5,h6,[role=heading]');
    if (h) return textOf(h);
  }
  return norm(el.getAttribute('title'));
}

function headingLevel(el) {
  const a = el.getAttribute('aria-level');
  if (a) return +a;
  const m = /^h([1-6])$/.exec(el.localName);
  return m ? +m[1] : 2;
}

function deepActive() {
  let a = document.activeElement;
  while (a) {
    if (rootOf(a) && rootOf(a).activeElement) a = rootOf(a).activeElement;
    else if ((a.localName === 'iframe') && (() => { try { return a.contentDocument && a.contentDocument.activeElement; } catch (e) { return null; } })())
      a = a.contentDocument.activeElement;
    else break;
  }
  return a;
}

function attrsOf(el, role, opts) {
  const a = [];
  const t = el.localName;
  const aria = (n) => el.getAttribute('aria-' + n);
  if (role === 'textbox' || role === 'searchbox' || role === 'spinbutton' || role === 'slider') {
    let v = t === 'input' || t === 'textarea' ? el.value : el.isContentEditable ? el.innerText : aria('valuetext') || aria('valuenow') || '';
    // custom sliders/spinbuttons often show their value only as text inside
    if (!v && t !== 'input' && t !== 'textarea' && (role === 'slider' || role === 'spinbutton')) v = el.innerText || '';
    if ((role === 'slider' || role === 'spinbutton') && t === 'input' && el.max) v += ' (' + (el.min || 0) + '–' + el.max + ')';
    v = norm(v);
    if (v) a.push('value=' + q(el.type === 'password' ? '•'.repeat(Math.min(v.length, 8)) : trunc(v, 60)));
    else if (el.getAttribute('placeholder') && nameOf(el, role) !== norm(el.getAttribute('placeholder')))
      a.push('placeholder=' + q(trunc(norm(el.getAttribute('placeholder')), 40)));
    if (t === 'input' && el.type && !['text', 'search', 'number', 'range'].includes(el.type)) a.push('type=' + el.type);
  }
  if (t === 'input' && el.type === 'file') a.push('type=file' + (el.files && el.files.length ? ' files=' + q([...el.files].map((f) => f.name).join(', ')) : ''));
  if (role === 'combobox' && t !== 'select') {
    // editable comboboxes hold their value in the input; select-only ones show it as text
    const v = norm(t === 'input' || t === 'textarea' ? el.value : el.innerText || '');
    if (v && v !== nameOf(el, role)) a.push('value=' + q(trunc(v, 60)));
  }
  if (t === 'select') {
    const sel = [...el.selectedOptions].map((o) => norm(o.label));
    if (sel.length) a.push('value=' + q(trunc(sel.join(', '), 60)));
    const opts2 = [...el.options].map((o) => norm(o.label)).filter(Boolean);
    const nOpt = FULL ? opts2.length : 8;
    const shown = opts2.slice(0, nOpt).map((s) => (FULL ? s : trunc(s, 40)));
    a.push('options=[' + shown.join('|') + (opts2.length > nOpt ? '|+' + (opts2.length - nOpt) : '') + ']');
  }
  if (role === 'checkbox' || role === 'radio' || role === 'switch' || role === 'menuitemcheckbox' || role === 'menuitemradio') {
    const c = t === 'input' ? el.checked : aria('checked') === 'true';
    if ((t === 'input' && el.indeterminate) || aria('checked') === 'mixed') a.push('mixed');
    else if (c) a.push('checked');
  }
  if (el.disabled || aria('disabled') === 'true') a.push('disabled');
  if (el.required || aria('required') === 'true') a.push('required');
  if (el.readOnly && (t === 'input' || t === 'textarea')) a.push('readonly');
  if (aria('invalid') === 'true' || (el.matches && (() => { try { return el.matches(':user-invalid'); } catch (e) { return false; } })()))
    a.push('invalid');
  const ex = aria('expanded');
  if (ex) a.push(ex === 'true' ? 'expanded' : 'collapsed');
  if (t === 'details') a.push(el.open ? 'expanded' : 'collapsed');
  if (aria('selected') === 'true' && role !== 'option') a.push('selected');
  if (role === 'option' && (aria('selected') === 'true' || el.selected)) a.push('selected');
  if (aria('pressed') === 'true') a.push('pressed');
  if (aria('current') && aria('current') !== 'false') a.push('current');
  if (role === 'dialog' && (aria('modal') === 'true' || (t === 'dialog' && el.matches(':modal')))) a.push('modal');
  if (opts.focused === el) a.push('focused');
  if (role === 'link' && opts.urls) {
    const h = el.getAttribute('href') || '';
    a.push('href=' + q(trunc(h, 80)));
  }
  if (opts.ids) {
    const tid = el.getAttribute('data-testid') || el.getAttribute('data-test') || el.getAttribute('data-cy');
    if (tid) a.push('testid=' + q(tid));
    else if (el.id) a.push('#' + el.id);
  }
  return a;
}

// ---------------------------------------------------------------- snapshot
// Node: {r: role/line-kind, n: name, ref, a: attrs[], k: kids[], el}
function inViewport(el, vw) {
  if (!vw) return true;
  let r = el.getBoundingClientRect();
  let ox = 0, oy = 0;
  for (let f = frameOf(el); f; f = frameOf(f)) { const fr = f.getBoundingClientRect(); ox += fr.left; oy += fr.top; }
  return r.bottom + oy > 0 && r.top + oy < vw.h && r.right + ox > 0 && r.left + ox < vw.w && r.width + r.height > 0;
}

// Walk the children of `el`. Text is gathered in "inline runs": consecutive
// text nodes and inline elements form one text line; any block or control
// ends the run. Links inside a run stay in the text AND get their own line.
function walk(el, out, inInter, noText, opts, vw, depth) {
  if (depth > 250) return;
  let run = '', plain = '', pend = [], runEl = null;
  const flush = () => {
    const p = norm(plain);
    if (p && (p.length > 1 || /\w/.test(p)) && (!vw || (runEl && inViewport(runEl, vw))))
      out.push({ r: 'text', n: trunc(norm(run), opts.textLen), k: [] });
    for (const x of pend) out.push(x);
    run = ''; plain = ''; pend = []; runEl = null;
  };
  for (const n of kids(el)) {
    if (n.nodeType === 3) {
      if (!noText && n.data.trim()) { run += n.data; plain += n.data; runEl = runEl || (n.parentElement || el); }
      else if (!noText) run += n.data;
      continue;
    }
    if (n.nodeType !== 1) continue;
    const t = n.localName;
    if (SKIP.has(t) || n.getAttribute('aria-hidden') === 'true') continue;
    if (!noText && INLINE.has(t) && !(t === 'label' && n.control) && t !== 'img' && t !== 'svg') {
      if (t === 'br') { run += ' '; continue; }
      const r = effectiveRole(n);
      if (!r || r === 'link') {
        if (!n.checkVisibility({ visibilityProperty: true }) && getComputedStyle(n).display !== 'contents') continue;
        const s = textOf(n);
        if (s) {
          run += s;
          if (r !== 'link' && !n.querySelector('a[href]')) plain += s;
          else if (r !== 'link') plain += s.replace(textOfLinks(n), '');
          runEl = runEl || n;
        }
        walkEl(n, pend, inInter, true, opts, vw, depth);
        continue;
      }
    }
    flush();
    walkEl(n, out, inInter, noText, opts, vw, depth);
  }
  flush();
}
function textOfLinks(n) { return [...n.querySelectorAll('a[href]')].map((a) => textOf(a)).join(' '); }

// Handle one element: emit its node (if it has a meaningful role) and walk its children.
function walkEl(n, out, inInter, noText, opts, vw, depth) {
  const t = n.localName;
  if (t === 'input' && n.type === 'hidden') return;
  const isCheck = t === 'input' && (n.type === 'checkbox' || n.type === 'radio');
  let contents = false;
  if (!n.checkVisibility()) {
    if (getComputedStyle(n).display === 'contents') contents = true;
    // visually-hidden native checkbox/radio styled through its label: keep it
    else if (!(isCheck && n.labels && n.labels.length && n.labels[0].checkVisibility())) return;
  }
  // closed <dialog>s and faded-out, non-interactive layers (CSS modals/toasts) are not on screen
  if (!contents && t === 'dialog' && !n.open) return;
  if (!contents && !n.checkVisibility({ opacityProperty: true }) && getComputedStyle(n).pointerEvents === 'none') return;
  const hiddenSelf = !contents && !isCheck && !n.checkVisibility({ visibilityProperty: true });
  const role = inInter ? roleOf(n) : effectiveRole(n);
  const node = { r: role, k: [] };
  let target = out, cInter = inInter, cNoText = noText || hiddenSelf || (t === 'label' && !!n.control);
  const nativeOpt = t === 'option' && n.parentElement && n.parentElement.localName === 'select';

  if (role && INTERACTIVE.has(role) && !hiddenSelf && !nativeOpt) {
    if (inViewport(n, vw)) {
      node.n = trunc(nameOf(n, role), opts.nameLen);
      node.ref = ref(n);
      node.a = attrsOf(n, role, opts);
      node.i = true;
      out.push(node); target = node.k;
    }
    cInter = true; cNoText = true;
    if (t === 'select') return; // options summarized in attrs
  } else if (role === 'heading' && !hiddenSelf && !noText) {
    node.r = 'h' + headingLevel(n);
    node.n = trunc(textOf(n), opts.textLen);
    node.h = true;
    if (node.n && inViewport(n, vw)) { out.push(node); target = node.k; }
    cNoText = true;
  } else if (role === 'row' && !hiddenSelf && !noText) {
    const cells = [...kids(n)].filter((c) => c.nodeType === 1 && c.checkVisibility());
    node.n = trunc(cells.map((c) => textOf(c)).join(' | '), opts.textLen * 2);
    node.row = true;
    if (node.n.replace(/[ |]/g, '') && inViewport(n, vw)) { out.push(node); target = node.k; }
    cNoText = true;
  } else if (role && CONTAINER.has(role) && !hiddenSelf && !inInter) {
    node.n = trunc(nameOf(n, role), 60);
    node.c = true;
    if (role === 'dialog' || role === 'alertdialog') node.a = attrsOf(n, role, opts);
    out.push(node); target = node.k;
  } else if (role === 'img' && opts.full && !noText && !hiddenSelf) {
    node.n = trunc(nameOf(n, role), 80);
    if (node.n && inViewport(n, vw)) out.push(node);
    return;
  } else if (role === 'iframe' && !hiddenSelf) {
    let same = false;
    try { same = !!(n.contentDocument && n.contentDocument.body); } catch (e) {}
    if (!same) {
      node.n = trunc(n.getAttribute('title') || n.getAttribute('src') || '', 80);
      node.ref = ref(n); // the daemon puts the frame's own snapshot under this line
      node.a = ['cross-origin'];
      out.push(node);
      return;
    }
  }
  walk(n, target, cInter, cNoText, opts, vw, depth + 1);
}

function lineOf(node, base) {
  if (node.r === 'text') return q(node.n);
  let s = node.r;
  if (node.n) s += ' ' + q(node.n);
  if (node.ref) s += ' @e' + node.ref;
  const a = node.a ? (base ? node.a.filter((x) => x !== 'focused') : node.a) : null;
  if (a && a.length) s += ' ' + a.join(' ');
  return s;
}

// Runs of 4+ unnamed same-role siblings with consecutive refs (map markers,
// icon grids) become one line: `button @e83 … @e218 (136 unnamed)`.
function squash(lines) {
  const out = [];
  const pat = /^(\s*)([a-z]+) @e(\d+)$/;
  for (let i = 0; i < lines.length; ) {
    const m = pat.exec(lines[i]);
    let j = i + 1;
    if (m) {
      while (j < lines.length) {
        const m2 = pat.exec(lines[j]);
        if (!m2 || m2[1] !== m[1] || m2[2] !== m[2] || +m2[3] !== +m[3] + (j - i)) break;
        j++;
      }
    }
    if (m && j - i >= 4) {
      out.push(`${m[1]}${m[2]} @e${m[3]} … @e${+m[3] + (j - i - 1)} (${j - i} unnamed)`);
      i = j;
    } else {
      out.push(lines[i]);
      i++;
    }
  }
  return out;
}

// --grep: case-insensitive substring; `a|b` matches either
function grepper(g) {
  if (!g) return null;
  const parts = g.toLowerCase().split('|').map((s) => s.trim()).filter(Boolean);
  return { test: (l) => { const x = l.toLowerCase(); return parts.some((p) => x.includes(p)); } };
}

// Render tree to lines. mode: 'all' | 'interactive'
function render(nodes, opts, mode) {
  const grep = grepper(opts.grep);
  const rec = (list, depth) => {
    const res = [];
    for (const node of list) {
      if (node.r === 'text') {
        if (mode === 'interactive') continue;
        const l = lineOf(node);
        if (grep && !grep.test(l)) continue;
        res.push([depth, l]);
        continue;
      }
      const sub = rec(node.k, depth + 1);
      const own = lineOf(node, opts.base);
      let keep;
      if (node.i) keep = true;
      else if (mode === 'interactive') keep = !node.h && sub.length > 0;
      else keep = !!(node.n || sub.length || (node.a && node.a.length));
      if (keep && grep && !sub.length && !grep.test(own)) keep = false;
      if (!keep) { for (const [d, l] of sub) res.push([d - 1, l]); continue; }
      // unnamed wrapper around exactly one line: just show the line
      if (node.c && !node.n && sub.length === 1 && !(node.a && node.a.length)) { res.push([depth, sub[0][1]]); continue; }
      res.push([depth, own], ...sub);
    }
    return res;
  };
  return squash(rec(nodes, 0).map(([d, l]) => '  '.repeat(Math.max(0, d)) + l));
}

let FULL = false;
K.snapshot = (o) => {
  const opts = Object.assign({ textLen: 100, nameLen: 80, max: 400 }, o || {});
  FULL = !!opts.full;
  if (opts.full) { opts.textLen = 4000; opts.nameLen = 200; }
  opts.focused = deepActive();
  const vw = opts.viewport ? { w: innerWidth, h: innerHeight } : null;
  const tree = [];
  if (opts.scope) {
    const el = sectionOf(resolve(opts.scope, { raw: true }));
    walk({ shadowRoot: { childNodes: [el] }, localName: 'div' }, tree, false, false, opts, vw, 0);
  } else {
    const root = document.body || document.documentElement;
    if (root) walk({ shadowRoot: { childNodes: [root] }, localName: 'div' }, tree, false, false, opts, vw, 0);
  }
  let lines = render(tree, opts, opts.interactive ? 'interactive' : 'all');
  const base = opts.wantBase ? render(tree, { base: true }, 'all') : null;
  let more = 0;
  if (lines.length > opts.max) { more = lines.length - opts.max; lines = lines.slice(0, opts.max); }
  return { lines, more, base, title: document.title, url: location.href, next: nextRef };
};

// ---------------------------------------------------------------- resolving targets
const ROLE_ALIASES = { input: 'textbox', field: 'textbox', heading: 'heading', h: 'heading' };
const ROLE_NAMES = new Set([...INTERACTIVE, 'heading', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'img', 'dialog', 'row', 'cell',
  'navigation', 'main', 'form', 'region', 'list', 'listitem', 'table', 'alert', 'status', 'tabpanel', 'input', 'field', 'banner', 'contentinfo', 'group', 'progressbar']);

function* allElements(root) {
  const stack = [root];
  while (stack.length) {
    const r = stack.pop();
    const list = r.querySelectorAll('*');
    for (const el of list) {
      yield el;
      const sr = rootOf(el);
      if (sr) stack.push(sr);
      if (el.localName === 'iframe') { try { if (el.contentDocument) stack.push(el.contentDocument); } catch (e) {} }
    }
  }
}

function visible(el) { return el.checkVisibility({ visibilityProperty: true }) || (el.localName === 'input' && el.labels && el.labels.length && el.labels[0].checkVisibility()); }

function byRole(role, name, all) {
  role = ROLE_ALIASES[role] || role;
  let level = 0;
  const m = /^h([1-6])$/.exec(role);
  if (m) { role = 'heading'; level = +m[1]; }
  const want = norm(name).toLowerCase();
  const exact = [], partial = [];
  for (const el of allElements(document)) {
    let r = roleOf(el);
    if (r === 'searchbox' && role === 'textbox') r = 'textbox';
    if (r !== role && !(role === 'clickable' && clickable(el))) continue;
    if (level && headingLevel(el) !== level) continue;
    if (!visible(el)) continue;
    const nm = norm(nameOf(el, r)).toLowerCase();
    if (nm === want) exact.push(el); else if (nm.includes(want)) partial.push(el);
  }
  return all ? exact.concat(partial) : exact[0] || partial[0] || null;
}

function closestInteractive(el) {
  for (let n = el; n && n.nodeType === 1; n = parentOf(n)) {
    const r = roleOf(n);
    if (r && INTERACTIVE.has(r)) return n;
    if (n.localName === 'label' && n.control) return n.control;
  }
  return el;
}

function byText(s, all) {
  const want = norm(s).toLowerCase();
  const exact = [], partial = [];
  const docs = [document];
  for (const el of allElements(document)) {
    if (SKIP.has(el.localName)) continue;
    // match on elements whose own direct text (or full text if small) contains the target
    let own = '';
    for (const c of kids(el)) if (c.nodeType === 3) own += c.data;
    own = norm(own).toLowerCase();
    let full = null;
    if (!own.includes(want)) {
      if (el.childElementCount > 0 && el.childElementCount < 6 && (el.textContent || '').length < 400) {
        full = textOf(el).toLowerCase();
        if (!full.includes(want)) continue;
      } else continue;
    }
    if (!visible(el)) continue;
    const txt = full !== null ? full : own;
    (txt === want ? exact : partial).push(el);
  }
  // prefer the deepest match (most specific)
  const pick = (list) => list.filter((e) => !list.some((o) => o !== e && composedContains(e, o)));
  const res = pick(exact).concat(pick(partial));
  return all ? res : res[0] || null;
}

function deepQuery(sel, all) {
  const res = [];
  for (const root of [document, ...shadowRoots()]) {
    if (all) res.push(...root.querySelectorAll(sel));
    else { const e = root.querySelector(sel); if (e) return e; }
  }
  return all ? res : null;
}
function shadowRoots() {
  const out = [];
  for (const el of allElements(document)) if (rootOf(el)) out.push(rootOf(el));
  return out;
}

function parseTarget(t) {
  t = String(t).trim();
  let m;
  if ((m = /^@?e(\d+)$/.exec(t))) return { kind: 'ref', n: +m[1] };
  if ((m = /^text=(.+)$/s.exec(t))) return { kind: 'text', v: m[1].replace(/^["']|["']$/g, '') };
  if ((m = /^([a-z0-9]+)[:=](.+)$/s.exec(t)) && ROLE_NAMES.has(m[1])) return { kind: 'role', role: m[1], v: m[2].replace(/^["']|["']$/g, '') };
  return { kind: 'css', v: t };
}

// all matches for a target, best first
function candidates(t) {
  const p = parseTarget(t);
  if (p.kind === 'ref') return [resolve(t)];
  if (p.kind === 'text') return byText(p.v, true);
  if (p.kind === 'role') return byRole(p.role, p.v, true);
  try { return deepQuery(p.v, true).filter(visible); } catch (e) { throw new Error(`invalid target "${t}"`); }
}

// `A >> B`: B inside A, or else B in A's nearest ancestor that has one
// (so `h5:Configure >> button:Discard` finds the Discard of that panel).
function chained(t, opt) {
  const parts = t.split(/\s+>>\s+/);
  let el = resolve(parts[0], { raw: true });
  for (const part of parts.slice(1)) {
    const c = candidates(part);
    let hit = null;
    for (let a = el; a && !hit; a = parentOf(a)) {
      const inside = c.filter((e) => e !== a && composedContains(a, e));
      if (!inside.length) continue;
      // siblings of the anchor (heading + its fields): the first one after it, else the nearest before
      const after = (e) => composedContains(el, e) || (el.compareDocumentPosition(e) & Node.DOCUMENT_POSITION_FOLLOWING);
      const order = (x, y) => (x.compareDocumentPosition(y) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1);
      const fwd = inside.filter(after).sort(order);
      hit = fwd[0] || inside.sort(order)[inside.length - 1];
    }
    if (!hit) throw new Retry(`no ${part} near ${parts[0]}`);
    el = hit;
  }
  if (parseTarget(parts[parts.length - 1]).kind === 'text' && !(opt && opt.raw)) el = closestInteractive(el);
  return el;
}

function resolve(t, opt) {
  if (t && t.nodeType === 1) return t;
  if (/\s>>\s/.test(t)) return chained(t, opt);
  const p = parseTarget(t);
  let el = null;
  if (p.kind === 'ref') {
    const w = byRef.get(p.n);
    el = w && w.deref();
    if (!el || !el.isConnected) {
      const e = new Error(`@e${p.n} is gone from the page (stale ref). Run \`wowsir snap\` for fresh refs.`);
      throw e;
    }
    return el;
  }
  if (p.kind === 'text') el = byText(p.v);
  else if (p.kind === 'role') el = byRole(p.role, p.v);
  else {
    try { el = deepQuery(p.v); } catch (e) { throw new Error(`invalid target "${t}". Use @eN, role:name (e.g. button:Sign in), text=..., or a CSS selector`); }
  }
  if (!el) throw new Retry(`no element matches ${t}`);
  if (p.kind === 'text' && !(opt && opt.raw)) el = closestInteractive(el);
  return el;
}
K.resolve = resolve;

function describe(el) {
  const r = effectiveRole(el) || el.localName;
  const n = trunc(nameOf(el, r), 60);
  const rf = refOf.get(el);
  return r + (n ? ' ' + q(n) : '') + (rf ? ' @e' + rf : '');
}
K.describe = (t) => describe(resolve(t));

// ---------------------------------------------------------------- selectors (for recording)
function selectorFor(el) {
  for (const a of ['data-testid', 'data-test', 'data-test-id', 'data-cy', 'data-qa']) {
    const v = el.getAttribute(a);
    if (v) return `[${a}=${q(v)}]`;
  }
  const r = effectiveRole(el);
  if (r) {
    const n = nameOf(el, r);
    if (n && n.length <= 80) {
      const matches = byRole(r, n, true).filter((e) => norm(nameOf(e, r)).toLowerCase() === norm(n).toLowerCase());
      if (matches.length === 1 && matches[0] === el) return `${r}:${n}`;
    }
  }
  if (el.id && /^[A-Za-z][\w-]*$/.test(el.id) && el.ownerDocument.querySelectorAll('#' + el.id).length === 1) return '#' + el.id;
  const nm = el.getAttribute('name');
  if (nm) { const s = `${el.localName}[name=${q(nm)}]`; if (el.ownerDocument.querySelectorAll(s).length === 1) return s; }
  // css path
  const parts = [];
  for (let n = el; n && n.nodeType === 1 && n.localName !== 'html'; n = n.parentElement) {
    if (n.id && /^[A-Za-z][\w-]*$/.test(n.id)) { parts.unshift('#' + n.id); break; }
    let i = 1;
    for (let s = n.previousElementSibling; s; s = s.previousElementSibling) if (s.localName === n.localName) i++;
    parts.unshift(n.localName + (i > 1 ? `:nth-of-type(${i})` : ''));
  }
  return parts.join(' > ');
}
K.selector = (t) => selectorFor(resolve(t));

// ---------------------------------------------------------------- actions
function frameOffset(el) {
  let ox = 0, oy = 0;
  for (let f = frameOf(el); f; f = frameOf(f)) {
    const r = f.getBoundingClientRect();
    ox += r.left + f.clientLeft; oy += r.top + f.clientTop;
  }
  return [ox, oy];
}
function hitAt(doc, x, y) {
  let e = doc.elementFromPoint(x, y);
  while (e && rootOf(e)) {
    const i = rootOf(e).elementFromPoint(x, y);
    if (!i || i === e) break;
    e = i;
  }
  return e;
}
function waitFrame() { return new Promise((r) => requestAnimationFrame(() => r())); }

// Returns {x, y, desc} ready for a trusted CDP mouse click, or {retry: reason}.
K.point = async (t, sel) => {
  let el;
  try { el = resolve(t); } catch (e) { if (e.retry) return { retry: e.message }; throw e; }
  let target = el;
  let r = el.getBoundingClientRect();
  if ((r.width < 1 || r.height < 1 || !el.checkVisibility({ visibilityProperty: true })) && el.labels && el.labels.length) {
    target = el.labels[0]; r = target.getBoundingClientRect();
  }
  if (!target.checkVisibility({ visibilityProperty: true })) return { retry: describe(el) + ' is not visible' };
  if (r.width < 1 || r.height < 1) return { retry: describe(el) + ' has no size' };
  if (el.disabled || el.getAttribute('aria-disabled') === 'true') return { retry: describe(el) + ' is disabled' };
  const win = target.ownerDocument.defaultView;
  if (r.top < 0 || r.left < 0 || r.bottom > win.innerHeight || r.right > win.innerWidth || frameOf(target)) {
    target.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    r = target.getBoundingClientRect();
  }
  if (document.getAnimations().length) {
    await waitFrame();
    const r2 = target.getBoundingClientRect();
    if (Math.abs(r2.left - r.left) > 1 || Math.abs(r2.top - r.top) > 1) return { retry: describe(el) + ' is still moving' };
    r = r2;
  }
  const lx = r.left + r.width / 2, ly = r.top + r.height / 2;
  const hit = hitAt(target.ownerDocument, lx, ly);
  if (hit && !(composedContains(target, hit) || composedContains(el, hit) || composedContains(hit, el) ||
      (el.labels && [...el.labels].some((l) => composedContains(l, hit))))) {
    return { retry: `${describe(el)} is covered by ${describe(hit)}` };
  }
  const [ox, oy] = frameOffset(target);
  return { x: lx + ox, y: ly + oy, desc: describe(el), sel: sel ? selectorFor(el) : undefined };
};

const TEXTLIKE = new Set(['text', 'search', 'email', 'password', 'tel', 'url', 'number', '']);
// Focus + select content so the next insertText replaces it.
K.prepFill = (t, sel) => {
  let el;
  try { el = resolve(t); } catch (e) { if (e.retry) return { retry: e.message }; throw e; }
  const tag = el.localName;
  if (!visible(el)) return { retry: describe(el) + ' is not visible' };
  if (el.disabled) return { retry: describe(el) + ' is disabled' };
  if (el.readOnly) throw new Error(describe(el) + ' is readonly');
  const s = sel ? selectorFor(el) : undefined;
  if (tag === 'select') throw new Error(describe(el) + ' is a <select>; use `wowsir select ' + t + ' <option>`');
  if (tag === 'input' && !TEXTLIKE.has(el.type)) {
    if (['checkbox', 'radio'].includes(el.type)) throw new Error(describe(el) + ' is a ' + el.type + '; use `wowsir check`');
    return { direct: true, desc: describe(el), sel: s }; // date, time, color, range...
  }
  if (tag !== 'input' && tag !== 'textarea' && !el.isContentEditable) {
    // maybe a wrapper around an input
    const inner = el.querySelector('input,textarea,[contenteditable]');
    if (inner) el = inner; else throw new Error(describe(el) + ' is not editable');
  }
  el.scrollIntoView({ block: 'nearest', behavior: 'instant' });
  el.focus();
  if (el.isContentEditable) {
    const range = document.createRange(); range.selectNodeContents(el);
    const s2 = getSelection(); s2.removeAllRanges(); s2.addRange(range);
  } else el.select();
  return { desc: describe(el), sel: s, empty: !(el.value || el.innerText || '').length };
};

function setNative(el, v) {
  const proto = el.localName === 'textarea' ? HTMLTextAreaElement.prototype : el.localName === 'select' ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
  el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
}
K.setValue = (t, v) => { const el = resolve(t); setNative(el, v); return el.value; };

K.select = (t, values, sel) => {
  let el;
  try { el = resolve(t); } catch (e) { if (e.retry) return { retry: e.message }; throw e; }
  if (el.localName !== 'select') {
    const inner = el.querySelector && el.querySelector('select');
    if (!inner) throw new Error(describe(el) + ' is not a <select>. For custom dropdowns: click it, then click the option.');
    el = inner;
  }
  if (el.disabled) return { retry: describe(el) + ' is disabled' };
  const opts = [...el.options];
  const picked = [];
  for (const v of values) {
    const lv = norm(v).toLowerCase();
    const o = opts.find((o) => o.value === v) || opts.find((o) => norm(o.label).toLowerCase() === lv) || opts.find((o) => norm(o.label).toLowerCase().includes(lv));
    if (!o) throw new Error(`option "${v}" not found in ${describe(el)}. Options: ${opts.map((o) => norm(o.label)).slice(0, 20).join(', ')}`);
    picked.push(o);
  }
  const s = sel ? selectorFor(el) : undefined;
  for (const o of opts) o.selected = picked.includes(o);
  el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { desc: describe(el), picked: picked.map((o) => norm(o.label)), sel: s };
};

// true | false | 'mixed' (tri-state checkboxes)
K.checked = (t) => {
  const el = resolve(t);
  if (el.localName === 'input') return el.indeterminate ? 'mixed' : el.checked;
  const a = el.getAttribute('aria-checked') || el.getAttribute('aria-pressed') || el.getAttribute('aria-selected');
  if (a === 'mixed') return 'mixed';
  if (a !== null) return a === 'true';
  const inner = el.querySelector && el.querySelector('input[type=checkbox],input[type=radio]');
  if (inner) return inner.checked;
  throw new Error(describe(el) + ' is not checkable');
};

// Scroll inside T: T itself or its nearest scrollable ancestor, else its first scrollable descendant.
K.scrollIn = (t, amount, dir) => {
  const el = resolve(t);
  const page = (e) => e === document.documentElement || e === document.body;
  const can = (e) => e && e.nodeType === 1 && !page(e) && e.scrollHeight > e.clientHeight + 1 && /(auto|scroll|overlay)/.test(getComputedStyle(e).overflowY);
  let s = el;
  while (s && !can(s)) s = s.parentElement || (s.getRootNode && s.getRootNode().host) || null;
  if (!s) s = [...el.querySelectorAll('*')].find(can);
  if (!s) throw new Error(describe(el) + ' has no scrollable area of its own (`scroll T` alone scrolls the page to it)');
  el.scrollIntoView({ block: 'nearest', behavior: 'instant' });
  const dy = amount != null ? amount : s.clientHeight * 0.8;
  s.scrollBy({ top: dir === 'up' ? -dy : dy, behavior: 'instant' });
  return { desc: describe(s), y: Math.round(s.scrollTop), h: Math.round(s.scrollHeight - s.clientHeight) };
};

K.isFileInput = (t) => {
  let el = resolve(t);
  if (!(el.localName === 'input' && el.type === 'file')) {
    const inner = (el.querySelector && el.querySelector('input[type=file]')) || (el.control && el.control.type === 'file' && el.control);
    if (!inner) throw new Error(describe(el) + ' is not a file input');
    el = inner;
  }
  return el;
};

K.scrollInfo = () => ({ y: Math.round(scrollY), h: document.documentElement.scrollHeight, vh: innerHeight });

// ---------------------------------------------------------------- queries for wait/expect
K.probe = (t) => {
  let el;
  try { el = resolve(t, { raw: true }); } catch (e) { if (e.retry) return { found: false }; throw e; }
  return { found: true, visible: visible(el), desc: describe(el) };
};
K.count = (t) => {
  if (/\s>>\s/.test(t)) { try { resolve(t); return 1; } catch (e) { return 0; } }
  const p = parseTarget(t);
  if (p.kind === 'ref') { try { resolve(t); return 1; } catch (e) { return 0; } }
  if (p.kind === 'text') return byText(p.v, true).length;
  if (p.kind === 'role') return byRole(p.role, p.v, true).length;
  return deepQuery(p.v, true).filter(visible).length;
};
K.hasText = (s) => {
  const want = norm(s).toLowerCase();
  const body = document.body;
  if (!body) return false;
  if (norm(body.innerText).toLowerCase().includes(want)) return true;
  // shadow DOM / same-origin frames are not in innerText
  return !!byText(s);
};
K.value = (t) => {
  const el = resolve(t);
  if ('value' in el && el.localName !== 'button' && el.localName !== 'li') {
    if (el.localName === 'select') return [...el.selectedOptions].map((o) => norm(o.label)).join(', ');
    return el.value;
  }
  return norm(el.innerText);
};
// a heading/label alone says little: use the section it heads
function sectionOf(el) {
  if (!(/^(h[1-6]|legend|caption|summary|dt)$/.test(el.localName) || el.getAttribute('role') === 'heading')) return el;
  let a = el.parentElement;
  while (a && a !== document.body && a.children.length === 1) a = a.parentElement;
  return a && a !== document.body ? a : el;
}

K.focused = () => {
  const e = deepActive();
  if (!e || e === document.body || e === document.documentElement) return 'page (nothing focused)';
  ref(e);
  return describe(e);
};

K.text = (t, max) => {
  const el = t ? sectionOf(resolve(t, { raw: true })) : document.body;
  const s = (el && (el.innerText || el.textContent)) || '';
  return s.replace(/\n{3,}/g, '\n\n').trim();
};
K.html = (t) => {
  const el = t ? resolve(t, { raw: true }) : document.documentElement;
  return el.outerHTML;
};
// ---------------------------------------------------------------- layout watch
// Watched elements are sampled every animation frame, so a control that jumps
// and comes back within one action is still caught. Positions are document
// coordinates, or viewport coordinates inside fixed/sticky containers (so page
// scrolling is not "movement" for either).
const watched = new Map(); // id -> { sel, el, fixed, min, max }
let watchRaf = 0;
function pinned(el) {
  for (let n = el; n && n.nodeType === 1; n = parentOf(n)) {
    const p = getComputedStyle(n).position;
    if (p === 'fixed' || p === 'sticky') return true;
  }
  return false;
}
function boxOf(w) {
  if (!w.el || !w.el.isConnected) { try { w.el = resolve(w.sel, { raw: true }); w.fixed = pinned(w.el); } catch (e) { w.el = null; return null; } }
  const r = w.el.getBoundingClientRect();
  if (!r.width && !r.height) return null;
  const sx = w.fixed ? 0 : scrollX, sy = w.fixed ? 0 : scrollY;
  return { x: Math.round(r.left + sx), y: Math.round(r.top + sy), w: Math.round(r.width), h: Math.round(r.height) };
}
function sample() {
  for (const w of watched.values()) {
    const b = boxOf(w);
    if (!b) continue;
    if (!w.min) { w.min = { ...b }; w.max = { ...b }; continue; }
    for (const k of ['x', 'y', 'w', 'h']) { w.min[k] = Math.min(w.min[k], b[k]); w.max[k] = Math.max(w.max[k], b[k]); }
  }
  watchRaf = watched.size ? requestAnimationFrame(sample) : 0;
}
// list: [{id, sel}]. (Re)starts sampling; returns the box now plus the range seen since the last read.
K.watchRead = (list) => {
  for (const id of [...watched.keys()]) if (!list.some((x) => x.id === id)) watched.delete(id);
  const out = [];
  for (const { id, sel } of list) {
    let w = watched.get(id);
    if (!w || w.sel !== sel) { w = { sel, el: null, fixed: false, min: null, max: null }; watched.set(id, w); }
    const now = boxOf(w);
    out.push({ id, now, min: w.min, max: w.max, desc: w.el ? describe(w.el) : sel });
    w.min = now ? { ...now } : null; w.max = now ? { ...now } : null;
  }
  if (watched.size && !watchRaf) watchRaf = requestAnimationFrame(sample);
  return out;
};
// outlines for `screenshot --mark`; returns a cleanup id
K.mark = (targets) => {
  const layer = document.createElement('div');
  layer.setAttribute('data-wowsir-mark', '');
  layer.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647';
  document.documentElement.appendChild(layer);
  const placed = [];
  const hits = (a) => placed.some((b) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom);
  let i = 0;
  for (const t of targets) {
    let el;
    try { el = resolve(t, { raw: true }); } catch (e) { continue; }
    const below = i++ % 2 === 1;
    const r = el.getBoundingClientRect();
    const b = document.createElement('div');
    b.style.cssText = `position:fixed;left:${r.left - 2}px;top:${r.top - 2}px;width:${r.width + 4}px;height:${r.height + 4}px;outline:2px solid #e0109a;box-sizing:border-box`;
    const l = document.createElement('div');
    const rf = refOf.get(el);
    l.textContent = `${rf ? '@e' + rf : effectiveRole(el) || el.localName} ${Math.round(r.left)},${Math.round(r.top)} ${Math.round(r.width)}×${Math.round(r.height)}`;
    const above = !below && r.top > 16 || r.bottom + 16 > innerHeight;
    l.style.cssText = `position:fixed;left:${Math.max(0, r.left - 2)}px;top:${above ? r.top - 17 : r.bottom + 3}px;font:11px/14px monospace;background:#e0109a;color:#fff;padding:0 3px;white-space:nowrap`;
    layer.append(b, l);
    const lw = l.getBoundingClientRect().width;
    if (r.left - 2 + lw > innerWidth) l.style.left = Math.max(0, innerWidth - lw) + 'px';
    // keep labels readable: step away from labels already placed
    const dir = above ? -1 : 1;
    for (let k = 0; k < 8 && hits(l.getBoundingClientRect()); k++) l.style.top = (parseFloat(l.style.top) + dir * 16) + 'px';
    placed.push(l.getBoundingClientRect());
  }
  return true;
};
K.unmark = () => { for (const l of document.querySelectorAll('[data-wowsir-mark]')) l.remove(); return true; };

K.rect = (t) => {
  const el = resolve(t, { raw: true });
  el.scrollIntoView({ block: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  const [ox, oy] = frameOffset(el);
  return { x: r.left + ox + scrollX, y: r.top + oy + scrollY, w: r.width, h: r.height };
};
})();
