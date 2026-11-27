// In-page statusline renderer, injected by `agy-statusline sidecar` over CDP and reused by the
// side panel for its sticky live preview and Theme gallery cards.
// No data logic lives here: the sidecar pushes ready-made numbers via window.__agyBar.update(payload)
// and polls window.__agyBar.panes() to learn which conversation each chat input belongs to.
// Re-injecting replaces the previous instance cleanly.
(function () {
  'use strict';
  if (window.__agyBar) { try { window.__agyBar.destroy(); } catch (e) {} }

  const BAR = 'agy-statusline-bar';
  const TIP = 'agy-statusline-tip';
  const INPUT = '[data-testid="agent-input-box"]';
  // The main pane's ancestor carries data-cascade-id; a subagent tab keeps it on a nearby sibling.
  // Sidebar rows also carry data-cascade-id, so only these two elements are trusted.
  const PANE = '[data-testid="conversation-view"][data-cascade-id], [data-testid="subagent-tab-conversation"][data-cascade-id]';
  const UUID = /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/;

  let data = null;
  let paneIds = [];
  let timer = 0;
  const costlySeen = new Map();

  const fmtIdle = (ms) => {
    const n = Math.max(0, Number(ms) || 0);
    if (n < 60e3) return Math.round(n / 1000) + ' 秒';
    if (n < 3600e3) return Math.round(n / 60e3) + ' 分钟';
    return (n / 3600e3).toFixed(1) + ' 小时';
  };
  // Why a call missed the cache, in one line. The side panel uses it too (window.__agyBar.missExplain).
  const missExplain = (m) => {
    if (!m) return '';
    if (m.cause === 'user') return '你隔了 ' + fmtIdle(m.idleMs) + ' 才发下一条消息';
    if (m.cause === 'turn') return '一轮中间停了 ' + fmtIdle(m.idleMs) + '（长命令、等你批准、等子代理）';
    if (m.cause === 'wait') return '隔了 ' + fmtIdle(m.idleMs) + ' 才发下一次请求（agy CLI）';
    if (m.cause === 'compact') return '上下文刚被压缩或截短';
    if (m.cause === 'model') return '换了模型（上一大调用：' + (m.fromModel || '其他模型') + '）';
    if (m.cause === 'other') return '离上一次调用结束仅隔 ' + fmtIdle(m.idleMs) + ' 也未命中（服务端缓存未接上或前缀变化）';
    return String(m.cause || '');
  };

  // ---------------------------------------------------------------- palettes (8 themes × light/dark)
  const PAL_VARS = [
    '--k-model', '--k-context', '--k-cache', '--k-timing', '--k-tokens', '--k-quota',
    '--k-warn', '--k-crit', '--k-run', '--k-muted', '--k-agent', '--k-cost', '--k-git',
    '--sep', '--cell-off', '--k-text', '--p-bg', '--pl-ink', '--bg-cut',
  ];
  const nd = (c, p) => `color-mix(in srgb, ${c} ${p}%, #2e3440)`;
  const PALETTE_DEFS = {
    cometix: ['Cometix', '默认 · Catppuccin 调深',
      ['#5c64d6', '#8839ef', '#1b8fbf', '#c4566a', '#158a90', '#e0620d', '#c9800f', '#d20f39', '#40a02b', '#8c8fa1', '#df8e1d', '#c03d8e', '#3f8f5a', '#c9c6bc', '#e6e2d8', '#4c4f69', '#fffefb', '#ffffff', '#ffffff'],
      ['#b4befe', '#cba6f7', '#89dceb', '#f2cdcd', '#94e2d5', '#fab387', '#f9e2af', '#f38ba8', '#a6e3a1', '#7f849c', '#f9e2af', '#f5c2e7', '#a6d189', '#585b70', '#45475a', '#cdd6f4', '#1e1e2e', '#1e1e2e', '#1e1e2e']],
    catppuccin: ['Catppuccin', 'Latte / Mocha',
      ['#7287fd', '#8839ef', '#04a5e5', '#e64553', '#179299', '#fe640b', '#df8e1d', '#d20f39', '#40a02b', '#8c8fa1', '#df8e1d', '#ea76cb', '#40a02b', '#bcc0cc', '#ccd0da', '#4c4f69', '#eff1f5', '#ffffff', '#eff1f5'],
      ['#b4befe', '#cba6f7', '#89dceb', '#eba0ac', '#94e2d5', '#fab387', '#f9e2af', '#f38ba8', '#a6e3a1', '#7f849c', '#f9e2af', '#f5c2e7', '#a6e3a1', '#585b70', '#45475a', '#cdd6f4', '#1e1e2e', '#1e1e2e', '#1e1e2e']],
    'tokyo-night': ['Tokyo Night', 'Day / Night',
      ['#2e7de9', '#9854f1', '#007197', '#387068', '#118c74', '#b15c00', '#8c6c3e', '#c64343', '#587539', '#848cb5', '#8c6c3e', '#f52a65', '#587539', '#a8aecb', '#c4c8da', '#3760bf', '#e1e2e7', '#ffffff', '#e1e2e7'],
      ['#7aa2f7', '#bb9af7', '#7dcfff', '#73daca', '#1abc9c', '#ff9e64', '#e0af68', '#db4b4b', '#9ece6a', '#737aa2', '#e0af68', '#f7768e', '#9ece6a', '#3b4261', '#292e42', '#c0caf5', '#1a1b26', '#1a1b26', '#1a1b26']],
    'rose-pine': ['Rosé Pine', 'Dawn / Main',
      ['#907aa9', '#d7827e', '#56949f', '#286983', '#56949f', '#ea9d34', '#ea9d34', '#b4637a', '#286983', '#797593', '#ea9d34', '#b4637a', '#286983', '#cecacd', '#dfdad9', '#575279', '#faf4ed', '#fffaf3', '#faf4ed'],
      ['#c4a7e7', '#ebbcba', '#9ccfd8', '#9ccfd8', '#9ccfd8', '#f6c177', '#f6c177', '#eb6f92', '#31748f', '#908caa', '#f6c177', '#eb6f92', '#31748f', '#524f67', '#403d52', '#e0def4', '#191724', '#191724', '#191724']],
    gruvbox: ['Gruvbox', 'Light / Dark',
      ['#076678', '#8f3f71', '#427b58', '#b57614', '#427b58', '#af3a03', '#b57614', '#9d0006', '#79740e', '#928374', '#b57614', '#9d0006', '#79740e', '#d5c4a1', '#ebdbb2', '#3c3836', '#fbf1c7', '#fbf1c7', '#fbf1c7'],
      ['#83a598', '#d3869b', '#8ec07c', '#fabd2f', '#8ec07c', '#fe8019', '#fabd2f', '#fb4934', '#b8bb26', '#a89984', '#fabd2f', '#fb4934', '#b8bb26', '#504945', '#3c3836', '#ebdbb2', '#282828', '#282828', '#282828']],
    nord: ['Nord', 'Polar',
      ['#5e81ac', nd('#b48ead', 80), nd('#88c0d0', 60), nd('#8fbcbb', 60), nd('#8fbcbb', 60), nd('#d08770', 85), nd('#ebcb8b', 60), '#bf616a', nd('#a3be8c', 65), '#7b88a1', nd('#ebcb8b', 60), nd('#bf616a', 90), nd('#a3be8c', 65), '#d8dee9', '#e5e9f0', '#2e3440', '#eceff4', '#ffffff', '#eceff4'],
      ['#81a1c1', '#b48ead', '#88c0d0', '#8fbcbb', '#8fbcbb', '#d08770', '#ebcb8b', '#bf616a', '#a3be8c', '#7b88a1', '#ebcb8b', '#bf616a', '#a3be8c', '#434c5e', '#3b4252', '#d8dee9', '#2e3440', '#2e3440', '#2e3440']],
    dracula: ['Dracula', 'Alucard / Dracula',
      ['#644ac9', '#a3144d', '#036a96', '#644ac9', '#036a96', '#a34d14', '#846e15', '#cb3a2a', '#14710a', '#6c664b', '#846e15', '#846e15', '#14710a', '#cfcfde', '#ece7d2', '#1f1f1f', '#fffbeb', '#fffbeb', '#fffbeb'],
      ['#bd93f9', '#ff79c6', '#8be9fd', '#bd93f9', '#8be9fd', '#ffb86c', '#f1fa8c', '#ff5555', '#50fa7b', '#6272a4', '#f1fa8c', '#f1fa8c', '#50fa7b', '#44475a', '#44475a', '#f8f8f2', '#282a36', '#282a36', '#282a36']],
    mono: ['Mono', '单色 · 仅告警上色',
      ['#3f4248', '#55585e', '#6b6e74', '#55585e', '#6b6e74', '#4a4d53', '#b8790e', '#c4223b', '#55585e', '#8b8e94', '#6b6e74', '#3f4248', '#55585e', '#cfcfd2', '#e4e4e7', '#3f4248', '#ffffff', '#ffffff', '#ffffff'],
      ['#d4d6da', '#bfc2c7', '#a8abb1', '#bfc2c7', '#a8abb1', '#c9ccd1', '#f9e2af', '#f38ba8', '#bfc2c7', '#7d8086', '#a8abb1', '#d4d6da', '#bfc2c7', '#4a4d53', '#3a3d42', '#d4d6da', '#1f2023', '#1f2023', '#1f2023']],
  };
  const PAL_CSS = Object.entries(PALETTE_DEFS).map(([k, [, , light, dark]]) => {
    const vars = (vals) => PAL_VARS.map((v, i) => `${v}:${vals[i]}`).join(';');
    return `.${BAR}[data-pal="${k}"],.${TIP}[data-pal="${k}"]{${vars(light)}}\n.${BAR}.dark[data-pal="${k}"],.${TIP}.dark[data-pal="${k}"]{${vars(dark)}}`;
  }).join('\n');

  const normLook = (raw) => {
    const l = raw || {};
    return {
      palette: PALETTE_DEFS[l.palette] ? l.palette : 'cometix',
      skin: ['plain', 'pill', 'powerline'].includes(l.skin) ? l.skin : 'plain',
      bars: ['thin', 'cells', 'ring', 'none'].includes(l.bars) ? l.bars : 'thin',
      density: l.density === 'full' ? 'full' : 'compact',
      icons: ['solid', 'outline', 'unicode', 'off'].includes(l.icons) ? l.icons : 'solid',
    };
  };

  // ---------------------------------------------------------------- formatting
  const fmtTok = (n) => {
    if (n == null) return '–';
    if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1).replace(/\.0$/, '') + 'M';
    if (n >= 1e3) return Math.round(n / 1e3) + 'k';
    return String(n);
  };
  const fmtUsd = (x) => {
    const v = Number(x) || 0;
    if (v > 0 && v < 0.01) return '<$0.01';
    return '$' + (v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2));
  };
  const fmtUsdFine = (x) => {
    const v = Number(x) || 0;
    if (v < 0.00005) return '$0';
    return '$' + (v < 0.01 ? v.toFixed(4) : v < 1 ? v.toFixed(3) : v.toFixed(2));
  };
  const fmtDur = (ms) => {
    const s = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
    if (h) return h + 'h' + String(m).padStart(2, '0') + 'm';
    if (m) return m + 'm' + String(sec).padStart(2, '0') + 's';
    return sec + 's';
  };
  const pad = (n) => String(n).padStart(2, '0');
  const fmtReset = (ms) => {
    if (!ms) return '';
    const d = new Date(ms);
    const hm = pad(d.getHours()) + ':' + pad(d.getMinutes());
    if (ms - Date.now() < 20 * 3600e3) return hm;
    return ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][d.getDay()] + ' ' + hm;
  };
  const pct = (f) => Math.round(f * 100) + '%';
  const clip = (s, n) => { const t = String(s || '').trim(); return t.length > n ? t.slice(0, n - 1) + '…' : t; };

  // ---------------------------------------------------------------- DOM helpers & icons
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const SVG_SOLID = {
    model: '<path d="M12 2l2.4 7.6L22 12l-7.6 2.4L12 22l-2.4-7.6L2 12l7.6-2.4z"/>',
    cache: '<ellipse cx="12" cy="5.5" rx="8" ry="3"/><path d="M4 8.5c0 1.7 3.6 3 8 3s8-1.3 8-3V12c0 1.7-3.6 3-8 3s-8-1.3-8-3zm0 6c0 1.7 3.6 3 8 3s8-1.3 8-3V18c0 1.7-3.6 3-8 3s-8-1.3-8-3z"/>',
    ttft: '<path d="M6 2h12v5l-4 5 4 5v5H6v-5l4-5-4-5zm2 2v2.3l4 5 4-5V4z"/>',
    speed: '<path d="M13 2L4 14h7l-1 8 9-12h-7z"/>',
    agent: '<path d="M9 2h6v3h3a2 2 0 012 2v11a2 2 0 01-2 2H6a2 2 0 01-2-2V7a2 2 0 012-2h3zm0 8a1.5 1.5 0 100 3 1.5 1.5 0 000-3zm6 0a1.5 1.5 0 100 3 1.5 1.5 0 000-3z"/>',
    git: '<path d="M6 3a3 3 0 011 5.83v6.34A3 3 0 115 15.17V8.83A3 3 0 016 3zm12 3a3 3 0 01-1.02 5.83A6 6 0 0111 17h-.17a3 3 0 110-2H11a4 4 0 004-3.83A3 3 0 0118 6z"/>',
    done: '<path d="M9 16.2l-4.2-4.2L3.4 13.4 9 19 21 7l-1.4-1.4z"/>',
    cost: '<circle cx="12" cy="12" r="10"/><text x="12" y="16.5" text-anchor="middle" font-size="13" font-weight="700" font-family="Arial,sans-serif" fill="var(--bg-cut)">$</text>',
  };
  const SVG_OUTLINE = {
    model: '<path d="M12 2l2.4 7.6L22 12l-7.6 2.4L12 22l-2.4-7.6L2 12l7.6-2.4z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>',
    cache: '<ellipse cx="12" cy="5.5" rx="7.5" ry="2.8" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M4.5 5.5v13c0 1.5 3.4 2.8 7.5 2.8s7.5-1.3 7.5-2.8v-13M4.5 12c0 1.5 3.4 2.8 7.5 2.8s7.5-1.3 7.5-2.8" fill="none" stroke="currentColor" stroke-width="1.8"/>',
    ttft: '<path d="M6.5 3h11v4l-4 5 4 5v4h-11v-4l4-5-4-5z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>',
    speed: '<path d="M13 2.5L4.5 13.5h6.5l-1 8 9.5-11.5h-6.5z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>',
    agent: '<rect x="4" y="6" width="16" height="14" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M12 2v4M9 12.5h.01M15 12.5h.01" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"/>',
    git: '<circle cx="6" cy="5.5" r="2.2" fill="none" stroke="currentColor" stroke-width="1.8"/><circle cx="6" cy="18.5" r="2.2" fill="none" stroke="currentColor" stroke-width="1.8"/><circle cx="18" cy="6.5" r="2.2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M6 7.8v8.4M18 8.8c0 5-3 7.2-9.8 7.2" fill="none" stroke="currentColor" stroke-width="1.8"/>',
    done: '<path d="M4.5 12.5l5 5 10-10" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>',
    cost: '<circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M12 6.8v10.4M14.5 9.3c0-1-1.1-1.7-2.5-1.7s-2.5.7-2.5 1.7c0 2.1 5 1.3 5 3.6 0 1-1.1 1.7-2.5 1.7s-2.5-.7-2.5-1.7" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>',
  };
  const UNI = { model: '✦', cache: '⛁', ttft: '⧗', speed: 'ϟ', agent: '▣', git: '⑂', done: '✓', cost: '$', context: '◔' };

  const icon = (name, look) => {
    if (look.icons === 'off') return null;
    if (look.icons === 'unicode') return el('span', 'ic uni', UNI[name] || '');
    const s = el('span', 'ic');
    const paths = look.icons === 'outline' ? SVG_OUTLINE[name] : SVG_SOLID[name];
    s.innerHTML = '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">' + (paths || '') + '</svg>';
    return s;
  };
  const ringEl = (f, cls) => {
    const frac = Math.max(0, Math.min(1, Number(f) || 0));
    const r = 8.2, c = 2 * Math.PI * r;
    const s = el('span', cls);
    s.innerHTML = '<svg viewBox="0 0 24 24" aria-hidden="true"><circle class="trk" cx="12" cy="12" r="' + r +
      '"/><circle class="arc" cx="12" cy="12" r="' + r + '" stroke-dasharray="' + (c * frac).toFixed(1) + ' ' + c.toFixed(1) + '"/></svg>';
    return s;
  };
  const ctxIcon = (f, look) => {
    if (look.icons === 'off') return null;
    if (look.icons === 'unicode') return el('span', 'ic uni', UNI.context);
    return ringEl(f, 'ic gauge');
  };
  const meter = (f, look) => {
    const frac = Math.max(0, Math.min(1, Number(f) || 0));
    if (look.bars === 'none') return null;
    if (look.bars === 'thin') {
      const m = el('span', 'meter thin');
      const fill = el('i');
      fill.style.width = Math.round(frac * 100) + '%';
      m.append(fill);
      return m;
    }
    if (look.bars === 'ring') return ringEl(frac, 'meter ring');
    const box = el('span', 'cells');
    const on = Math.round(frac * 10);
    for (let i = 0; i < 10; i++) box.append(el('i', i < on ? 'on' : ''));
    return box;
  };

  // A segment: colour key, leading icon node, text parts joined by a dim dot, custom tooltip text.
  const seg = (key, lead, parts, tipText) => {
    const s = el('span', 'seg k-' + key);
    if (lead) s.append(lead);
    parts.filter((p) => p != null && p !== '').forEach((p, i) => {
      if (i) s.append(el('span', 'dot', '·'));
      s.append(typeof p === 'string' ? el('span', 'tx', p) : p);
    });
    if (tipText) s.setAttribute('data-agy-tip', tipText);
    return s;
  };
  const dim = (t, cls) => el('span', 'tx real' + (cls ? ' ' + cls : ''), t);

  // ---------------------------------------------------------------- custom tooltip (survives redraws)
  const tip = { el: null, target: null, x: -1, y: -1, timer: 0, hideAt: 0 };
  const solidBg = (node) => {
    let e = node;
    while (e && e.nodeType === 1) {
      const bg = getComputedStyle(e).backgroundColor;
      const m = bg && bg.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)/);
      if (m && (m[4] === undefined || Number(m[4]) > 0.85)) return bg;
      e = e.parentElement;
    }
    return '';
  };
  const tipBox = () => {
    if (!tip.el || !tip.el.isConnected) {
      tip.el = el('div', TIP);
      tip.el.setAttribute('data-agy-no-i18n', '');
      tip.el.setAttribute('role', 'tooltip');
      tip.el.hidden = true;
      document.body.append(tip.el);
    }
    return tip.el;
  };
  const tipOwner = (node) => {
    const t = node && node.closest ? node.closest('[data-agy-tip]') : null;
    return t && t.closest('.' + BAR) ? t : null;
  };
  const hideTip = () => {
    clearTimeout(tip.timer);
    tip.timer = 0;
    tip.target = null;
    if (tip.el && !tip.el.hidden) { tip.el.hidden = true; tip.hideAt = Date.now(); }
  };
  const placeTip = () => {
    const t = tip.target;
    if (!t || !t.isConnected) { hideTip(); return; }
    const ownerBar = t.closest('.' + BAR);
    const box = tipBox();
    box.classList.toggle('dark', !!(ownerBar && ownerBar.classList.contains('dark')));
    box.setAttribute('data-pal', (ownerBar && ownerBar.getAttribute('data-pal')) || 'cometix');
    const text = t.getAttribute('data-agy-tip') || '';
    if (box.textContent !== text) box.textContent = text;
    const bg = solidBg(ownerBar || t);
    box.style.background = bg || '';
    box.hidden = false;
    const r = t.getBoundingClientRect();
    const w = box.offsetWidth, h = box.offsetHeight;
    const x = Math.max(8, Math.min(r.left, window.innerWidth - w - 8));
    let y = r.top - h - 6;
    if (y < 8) y = Math.min(r.bottom + 6, window.innerHeight - h - 8);
    box.style.left = x + 'px';
    box.style.top = y + 'px';
  };
  const onTipMove = (ev) => {
    tip.x = ev.clientX;
    tip.y = ev.clientY;
    const t = tipOwner(ev.target);
    if (t === tip.target) return;
    if (!t) { hideTip(); return; }
    const showing = tip.el && !tip.el.hidden;
    tip.target = t;
    clearTimeout(tip.timer);
    tip.timer = 0;
    if (showing || Date.now() - tip.hideAt < 600) { placeTip(); return; }
    tip.timer = setTimeout(() => { tip.timer = 0; if (tip.target) placeTip(); }, 350);
  };
  const tipAfterRender = () => {
    if (!tip.target) return;
    if (!tip.target.isConnected) {
      const t = tip.x >= 0 ? tipOwner(document.elementFromPoint(tip.x, tip.y)) : null;
      if (!t) { hideTip(); return; }
      tip.target = t;
    }
    if (tip.el && !tip.el.hidden) placeTip();
  };

  // ---------------------------------------------------------------- segments
  const modelMismatch = (display, resp) => {
    if (!display || !resp) return false;
    const fam = display.split(' (')[0].toLowerCase().replace(/\s+/g, '-');
    const r = resp.toLowerCase();
    return !fam.includes(r) && !r.includes(fam);
  };
  const isPlaceholderModel = (m) => !m || /PLACEHOLDER|MODEL_/i.test(m);

  const modelSeg = (a, look, forSub) => {
    if (!a.model || (forSub && isPlaceholderModel(a.model))) return null;
    const parts = [a.model];
    const odd = modelMismatch(a.model, a.respModel);
    if (odd && !forSub) {
      parts.push(dim(a.respModel, 'odd'));
    } else if (odd && forSub && !isPlaceholderModel(a.respModel)) {
      parts.push(dim(a.respModel, 'odd'));
    }
    const tipText = '界面模型：' + a.model +
      (a.respModel ? '\n接口返回：' + a.respModel : '') +
      (odd ? '\n注意：接口实际返回的模型名与界面显示不一致' : '');
    return seg('model', icon('model', look), parts, tipText);
  };

  const compBadge = (n, look) => {
    const b = el('span', 'comp');
    if (look.icons !== 'off') b.append(el('span', 'cyc', '↻'));
    b.append(n + '×');
    return b;
  };

  const ctxSeg = (a, look) => {
    if (a.ctxUsed == null) return null;
    const f = a.ctxMax ? a.ctxUsed / a.ctxMax : 0;
    const key = f >= 0.9 ? 'crit' : f >= 0.75 ? 'warn' : 'context';
    const usedText = look.density === 'full' && a.ctxMax ? fmtTok(a.ctxUsed) + ' / ' + fmtTok(a.ctxMax) : fmtTok(a.ctxUsed);
    const parts = [a.ctxMax ? pct(f) : null, usedText];
    if (a.compactions) parts.push(compBadge(a.compactions, look));
    const barChars = a.ctxMax ? Math.round(Math.min(1, f) * 24) : 0;
    const barLine = a.ctxMax ? '\n' + '█'.repeat(barChars) + '░'.repeat(24 - barChars) : '';
    const tipText = '上下文窗口：' + Number(a.ctxUsed).toLocaleString() +
      (a.ctxMax ? ' / ' + Number(a.ctxMax).toLocaleString() + ' token（' + pct(f) + '）' : ' token') +
      barLine +
      (a.compactions ? '\n已自动压缩上下文 ' + a.compactions + ' 次（可在右侧 Statusline 面板查看压缩摘要）' : '\n尚未发生上下文压缩');
    return seg(key, ctxIcon(f, look), parts, tipText);
  };

  const cacheSeg = (a, look) => {
    if (a.cacheHit == null) return null;
    const parts = look.density === 'full' ? [fmtTok(a.cacheRead), pct(a.cacheHit) + ' hit'] : [pct(a.cacheHit) + ' hit'];
    let tipText = '最近一次调用 Prompt 缓存命中率 ' + pct(a.cacheHit) +
      '\n缓存读取：' + Number(a.cacheRead || 0).toLocaleString() + ' token';
    if (a.cacheWrite > 0) tipText += '\n缓存写入：' + Number(a.cacheWrite).toLocaleString() + ' token';
    return seg('cache', icon('cache', look), parts, tipText);
  };

  const callSegs = (a, sg, look) => {
    const out = [];
    if (a.ttftMs != null && sg.ttft !== false) {
      out.push(seg('timing', icon('ttft', look), [(a.ttftMs / 1000).toFixed(1) + 's'], '最近一次调用首字延迟 (TTFT)'));
    }
    if (a.tps != null && sg.speed !== false) {
      out.push(seg('tokens', icon('speed', look), [Math.round(a.tps) + ' tok/s'],
        '最近一次调用输出速度：' + Math.round(a.tps) + ' tok/s（输出 ' + fmtTok(a.outTokens) + ' token）'));
    }
    return out;
  };

  // Always occupies layout space when idle so starting/stopping generation never jumps lines.
  const runSeg = (idle) => {
    const s = el('span', 'seg k-run' + (idle ? ' idle' : ''));
    if (idle) s.setAttribute('aria-hidden', 'true');
    s.append(el('span', 'pulse'), el('span', 'tx', 'running'));
    return s;
  };

  const costlyBadge = (usd, key, look) => {
    const b = el('span', 'costly');
    if (look.icons !== 'off') b.append(el('span', 'ex', '!'));
    b.append(fmtUsd(usd));
    if (key) {
      if (!costlySeen.has(key)) {
        costlySeen.set(key, Date.now());
        if (costlySeen.size > 200) costlySeen.delete(costlySeen.keys().next().value);
      }
      if (Date.now() - costlySeen.get(key) < 6000) b.classList.add('flash');
    }
    return b;
  };

  const costSeg = (v, today, todayCli, showConv, look, priceSource) => {
    const convText = showConv ? fmtUsd(v.costConv) + (v.multiDay ? ' conv' : '') : null;
    const lastTotal = v.costLast
      ? (Number(v.costLast.in) || 0) + (Number(v.costLast.cache) || 0) + (Number(v.costLast.cw) || 0) + (Number(v.costLast.out) || 0)
      : 0;
    let convNode = convText;
    if (showConv && v.lastCallCostly && lastTotal > 0) {
      const wrap = el('span', 'cost-wrap');
      wrap.append(el('span', 'tx', convText), costlyBadge(lastTotal, v.lastCallKey, look));
      convNode = wrap;
    }
    const parts = [convNode];
    if (today != null) parts.push(dim(fmtUsd(today) + ' today'));
    const scope = v.isSub
      ? '此子代理累计'
      : v.includesSubagents === false
        ? '当前对话累计（不含子代理）'
        : '当前对话累计（含子代理）';
    let tipText = '按公开 API 标准价折算（非订阅实际扣费）\n' + scope + '：' + fmtUsdFine(v.costConv);
    if (v.multiDay && v.costConvToday != null) {
      tipText += '（其中今日 ' + fmtUsdFine(v.costConvToday) + '）';
    }
    if (today != null) {
      tipText += '\n今日全部对话：' + fmtUsdFine(today) + (todayCli > 0 ? '（含 agy CLI ' + fmtUsdFine(todayCli) + '）' : '');
    }
    if (v.costLast) {
      tipText += '\n最近一次调用：' + fmtUsdFine(lastTotal) + '（输入 ' + fmtUsdFine(v.costLast.in) + ' · 缓存读 ' + fmtUsdFine(v.costLast.cache) +
        (v.costLast.cw > 0 ? ' · 缓存写 ' + fmtUsdFine(v.costLast.cw) : '') +
        ' · 输出 ' + fmtUsdFine(v.costLast.out) + '）';
    }
    if (v.lastCallCostly && lastTotal > 0) {
      const m = v.lastCallMiss;
      if (m) {
        tipText += '\n\n⚠ 高价提醒：最近一次调用缓存未命中（约 ' + Number(m.tok || 0).toLocaleString() +
          ' token 未走缓存，走缓存可省约 ' + fmtUsdFine(m.usd) + '）\n原因：' + missExplain(m);
      } else {
        tipText += '\n\n⚠ 高价提醒：缓存已命中，本次费用较高主要来自长上下文或大量输出';
      }
    } else if (v.lastCallMiss) {
      const m = v.lastCallMiss;
      tipText += '\n最近一次缓存未命中：多花约 ' + fmtUsdFine(m.usd) + '（' + missExplain(m) + '）';
    }
    if (v.priceName) tipText += '\n匹配价目：' + v.priceName;
    if (priceSource) tipText += '\n价格来源：' + (priceSource.startsWith('online') ? '在线价格表 ' + priceSource : '内置价格表');
    return seg('cost', icon('cost', look), parts, tipText);
  };

  const gitSeg = (g, path, look) => {
    if (!g || !g.branch) return null;
    const parts = [g.branch + (g.dirty ? '*' : '')];
    if (g.ahead || g.behind) parts.push((g.ahead ? '↑' + g.ahead : '') + (g.behind ? '↓' + g.behind : ''));
    if (g.added || g.deleted) parts.push(dim('+' + g.added + ' −' + g.deleted));
    const tipText = 'Git 仓库：' + (path || g.branch) + (g.dirty ? '\n有未提交修改' : '\n工作区干净') +
      (g.ahead || g.behind ? '\n领先 ' + g.ahead + ' · 落后 ' + g.behind + ' 个提交' : '') +
      (g.added || g.deleted ? '\n相对 HEAD 变更：+' + g.added + ' −' + g.deleted + ' 行' : '');
    return seg('git', icon('git', look), parts, tipText);
  };

  const quotaOn = (sg) => (q) => (q.window === '5h' ? sg.quota_5h !== false : q.window === 'weekly' ? sg.quota_7d !== false : true);
  const quotaSegs = (qs, look) => (qs || []).map((q) => {
    const key = q.used >= 0.9 ? 'crit' : q.used >= 0.7 ? 'warn' : 'quota';
    const parts = [meter(q.used, look), pct(q.used), fmtReset(q.resetMs)];
    if (q.window === 'weekly' && q.pace != null && Math.abs(q.pace) >= 0.01) {
      parts.push(el('span', q.pace > 0 ? 'pace-hi' : 'pace-ok', (q.pace > 0 ? '↑' : '↓') + Math.round(Math.abs(q.pace) * 100) + '%'));
    }
    const label = q.window === '5h' ? '5h' : q.window === 'weekly' ? '7d' : q.window;
    const tipText = (q.window === '5h' ? '5 小时额度' : '每周额度') + '已用 ' + (q.used * 100).toFixed(1) + '%' +
      (q.resetMs ? '\n重置时间：' + new Date(q.resetMs).toLocaleString() : '') +
      (q.pace != null ? '\n相对匀速进度：' + (q.pace > 0 ? '+' : '') + Math.round(q.pace * 100) + '%' : '');
    return seg(key, el('span', 'tag', label), parts, tipText);
  });

  // ---------------------------------------------------------------- subagent tree rows
  const subNameSeg = (a, look, treeChar) => {
    const role = a.subRole && a.subRole !== a.subType ? clip(a.subRole, 28) : '';
    const extra = [];
    if (a.ttftMs != null) extra.push('TTFT ' + (a.ttftMs / 1000).toFixed(1) + 's');
    if (a.tps != null) extra.push(Math.round(a.tps) + ' tok/s');
    const tipText = '子代理：' + (a.subType || 'subagent') + (a.subRole ? ' · ' + a.subRole : '') +
      '\nID：' + a.id + (extra.length ? '\n最近调用：' + extra.join(' · ') : '');
    const s = seg('agent', icon('agent', look), [a.subType || 'subagent', role], tipText);
    if (treeChar) s.prepend(el('span', 'tree', treeChar));
    return s;
  };

  const subCostSeg = (a, look) => {
    if (!(a.cost > 0)) return null;
    return seg('cost', icon('cost', look), [fmtUsd(a.cost)], '该子代理累计折算花费：' + fmtUsdFine(a.cost) + '（' + (a.calls || 0) + ' 次调用）');
  };

  const subStateSeg = (a, look) => {
    const tl = Array.isArray(a.timeline) && a.timeline.length ? '\n\n最近步骤：\n' + a.timeline.join('\n') : '';
    if (a.running) {
      if (a.waiting) {
        const tipText = '等待你批准' + (a.currentStep ? '：' + a.currentStep : '') + tl;
        const g = seg('warn', el('span', 'pulse'), ['waiting'], tipText);
        g.classList.add('sst');
        return g;
      }
      const t0 = a.startedMs || 0;
      const text = t0 ? fmtDur(Date.now() - t0) : 'running';
      const tipText = (a.currentStep ? '当前步骤：' + a.currentStep : '正在运行') +
        (t0 ? '\n已运行：' + fmtDur(Date.now() - t0) : '') + tl;
      const g = seg('run', el('span', 'pulse'), [text], tipText);
      g.classList.add('sst');
      const tx = t0 ? g.querySelector('.tx') : null;
      if (tx) tx.dataset.agyT0 = String(t0);
      return g;
    }
    const dur = a.startedMs && a.endedMs && a.endedMs > a.startedMs ? fmtDur(a.endedMs - a.startedMs) : '';
    const pieces = [dur ? 'done ' + dur : 'done', a.stepTotal ? a.stepTotal + ' steps' : ''];
    const tipText = '子代理已完成' + (dur ? '（用时 ' + dur + '）' : '') + tl;
    const g = seg('muted', icon('done', look), pieces, tipText);
    g.classList.add('sst');
    return g;
  };

  const line = (items, cls) => {
    const l = el('div', 'ln' + (cls ? ' ' + cls : ''));
    l.append(...items.filter(Boolean));
    return l.childElementCount ? l : null;
  };

  // Aligns columns across multiple subagent rows (excluding the trailing flexible status column).
  // Clears all, then reads all, then writes all, so the host page lays out once per call rather
  // than once per cell.
  const alignCols = (bar) => {
    if (bar.querySelectorAll('.ln.sub').length < 2) return;
    const cells = bar.querySelectorAll(':scope .ln.sub .seg[data-col]');
    cells.forEach((s) => { s.style.minWidth = ''; });
    const widths = new Map();
    cells.forEach((s) => {
      if (s.classList.contains('sst') || s.classList.contains('ph')) return;
      const c = s.dataset.col;
      const w = Math.ceil(s.getBoundingClientRect().width);
      if (w > (widths.get(c) || 0)) widths.set(c, w);
    });
    cells.forEach((s) => {
      if (s.classList.contains('sst')) return;
      const w = widths.get(s.dataset.col);
      if (w) s.style.minWidth = w + 'px';
    });
  };

  // Live 1-second clock tick for running subagent elapsed times without full DOM redraws.
  const tickClocks = () => {
    document.querySelectorAll('.' + BAR + ' [data-agy-t0]').forEach((tx) => {
      const t0 = Number(tx.dataset.agyT0);
      if (t0 > 0) tx.textContent = fmtDur(Date.now() - t0);
    });
  };

  // ---------------------------------------------------------------- build
  const selectedModel = (box) => {
    const t = (box && box.querySelector('[data-testid="model-selector-trigger"]')) ||
      document.querySelector('[data-testid="model-selector-trigger"]');
    return t ? t.textContent.trim() : '';
  };
  const homeQuota = (sel) => {
    const groups = (data && data.quotaGroups) || [];
    let best = null, bestLen = 0;
    for (const g of groups) {
      const names = String(g.models || '').replace(/^[^:]*:/, '').split(',').map((s) => s.trim()).filter(Boolean);
      for (const n of names) {
        if (sel.startsWith(n) && n.length > bestLen) { best = g; bestLen = n.length; }
      }
    }
    const g = best || groups[0];
    return g ? g.buckets : [];
  };

  const build = (paneId, box, look, optsExtra) => {
    const previewCompact = !!(optsExtra && optsExtra.compactCard);
    if (!data) return [line([seg('muted', null, ['waiting for agy-statusline sidecar…'])])];
    const o = data.opts || {};
    const sg = o.seg || {};
    const on = (k) => sg[k] !== false;
    const v = paneId && data.views ? data.views[paneId] : null;
    const git = v && on('git') ? gitSeg(v.git, v.workspace, look) : null;
    if (!v || !v.main || !v.main.calls) {
      const items = [];
      const m = selectedModel(box);
      if (m && on('model')) items.push(seg('model', icon('model', look), [m]));
      items.push(git);
      if (v && v.main && v.main.running && on('running')) items.push(runSeg(false));
      items.push(...quotaSegs(homeQuota(m).filter(quotaOn(sg)), look));
      if (on('today') && data.today) items.push(seg('cost', icon('cost', look), [dim(fmtUsd(data.today) + ' today')]));
      return [line(items)].filter(Boolean);
    }
    const a = v.main;
    const showToday = !previewCompact && on('today') && !v.isSub;
    const l1 = [
      v.isSub ? subNameSeg(a, look, '') : null,
      on('model') ? modelSeg(a, look, false) : null,
      git,
      on('context') ? ctxSeg(a, look) : null,
      on('cache') ? cacheSeg(a, look) : null,
      on('cost') || showToday ? costSeg(v, showToday ? data.today : null, data.todayCli || 0, on('cost'), look, data.priceSource) : null,
      !previewCompact && on('running') ? runSeg(!a.running) : null,
    ];
    const l2 = [...callSegs(a, sg, look), ...quotaSegs((v.quota || []).filter(quotaOn(sg)), look)];
    const lines = [line(l1), line(l2)];

    if (!previewCompact && on('subagents') && Array.isArray(v.subs) && v.subs.length) {
      const realSubs = v.subs.filter((s) => !s.id.startsWith('+'));
      const moreSub = v.subs.find((s) => s.id.startsWith('+'));
      // Fixed 6 columns: name, model, context, cache, cost, status.
      const matrix = realSubs.map((s, idx) => {
        const isLast = idx === realSubs.length - 1 && !moreSub;
        return [
          subNameSeg(s, look, isLast ? '└' : '├'),
          on('model') ? modelSeg(s, look, true) : null,
          on('context') ? ctxSeg(s, look) : null,
          on('cache') ? cacheSeg(s, look) : null,
          on('cost') ? subCostSeg(s, look) : null,
          on('running') || !s.running ? subStateSeg(s, look) : null,
        ];
      });
      const hasCol = matrix.length ? matrix[0].map((_, c) => matrix.some((r) => r[c] != null)) : [];
      for (const row of matrix) {
        const cells = [];
        row.forEach((cell, c) => {
          if (!hasCol[c]) return;
          const node = cell || el('span', 'seg ph');
          node.dataset.col = String(c);
          cells.push(node);
        });
        lines.push(line(cells, 'sub'));
      }
      if (moreSub) {
        const m = seg('muted', null, [moreSub.id + ' more subagents']);
        m.prepend(el('span', 'tree', '└'));
        lines.push(line([m], 'sub'));
      }
    }
    if (!previewCompact && data.error) {
      lines.push(line([seg('warn', null, [String(data.error).slice(0, 120)], 'agy-statusline sidecar error')]));
    }
    return lines.filter(Boolean);
  };

  // ---------------------------------------------------------------- mount & style
  // `inputCount` is the number of chat inputs on the page, computed once per render by the caller.
  const paneId = (box, inputCount) => {
    const own = box.closest(PANE);
    if (own) return own.getAttribute('data-cascade-id');
    let e = box.parentElement;
    for (let i = 0; i < 8 && e; i++, e = e.parentElement) {
      const d = e.querySelector('[data-testid="subagent-tab-conversation"][data-cascade-id]');
      if (d) return d.getAttribute('data-cascade-id');
    }
    if (inputCount !== 1) return null;
    const m = UUID.exec(location.pathname);
    return m ? m[1] : null;
  };

  const isDark = () => {
    const c = getComputedStyle(document.body).backgroundColor.match(/\d+(\.\d+)?/g);
    if (!c || (c.length === 4 && Number(c[3]) === 0)) return matchMedia('(prefers-color-scheme: dark)').matches;
    const [r, g, b] = c.map(Number);
    return 0.2126 * r + 0.7152 * g + 0.0722 * b < 128;
  };

  const style = el('style');
  style.id = BAR + '-style';
  style.textContent = `
${PAL_CSS}
.${BAR}{display:flex;flex-direction:column;gap:2px;padding:0 .9rem .35rem;
 font-family:"Google Sans Code","JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,"PingFang SC",monospace;
 font-size:12px;line-height:1.65;font-weight:500;font-variant-numeric:tabular-nums;color:var(--k-muted);cursor:default;user-select:text}
.${BAR}[hidden]{display:none}
.${BAR} .ln{display:flex;flex-wrap:wrap;align-items:center;row-gap:2px;margin-left:-1.6em;clip-path:inset(-6px -6px -6px 1.6em)}
.${BAR} .ln.sub{margin-left:-.5em;font-size:11.5px}
.${BAR} .seg{display:inline-flex;align-items:center;white-space:nowrap;color:var(--kc);margin-left:1.6em;position:relative}
.${BAR} .seg::before{content:"|";color:var(--sep);position:absolute;left:-1.05em;top:0;bottom:0;display:flex;align-items:center;font-weight:400}
.${BAR} .ic{display:inline-flex;align-items:center;justify-content:center;width:13px;height:13px;margin-right:.38em;flex:none}
.${BAR} .ic svg{width:100%;height:100%;display:block}
.${BAR} .ic.uni{width:auto;font-size:12px;font-weight:700;line-height:1}
.${BAR}[data-icons="off"] .ic{display:none}
.${BAR} .tag{font-size:10.5px;font-weight:700;margin-right:.42em;opacity:.9}
.${BAR} .dot{margin:0 .42em;opacity:.55}
.${BAR} .real{opacity:.72;font-weight:400}
.${BAR} .real.odd{opacity:1;font-weight:600;color:var(--k-warn)}
.${BAR} .comp{display:inline-flex;align-items:center;color:var(--k-warn);font-weight:600;font-size:11px;padding:0 .38em;border-radius:4px;line-height:1.45;background:color-mix(in srgb,var(--k-warn) 16%,transparent)}
.${BAR} .comp .cyc{margin-right:.12em;font-weight:700}
.${BAR} .cost-wrap{display:inline-flex;align-items:center}
.${BAR} .costly{display:inline-flex;align-items:center;color:var(--k-crit);font-weight:700;font-size:11px;padding:0 .38em;border-radius:4px;line-height:1.45;background:color-mix(in srgb,var(--k-crit) 16%,transparent);margin-left:.34em}
.${BAR} .costly .ex{margin-right:.08em;font-weight:800}
.${BAR} .costly.flash{animation:agyCostlyFlash .72s ease-in-out 5}
@keyframes agyCostlyFlash{0%,100%{opacity:1}50%{opacity:.25}}
.${BAR} .pace-ok{color:var(--k-run)} .${BAR} .pace-hi{color:var(--k-warn)}
.${BAR} .tree{color:var(--k-muted);margin-right:.4em;font-weight:400;opacity:.8}
.${BAR} .cells{display:inline-flex;gap:2px;margin-right:.18em}
.${BAR} .cells i{display:block;width:5px;height:10px;border-radius:1.5px;background:var(--cell-off)}
.${BAR} .cells i.on{background:var(--kc)}
.${BAR} .meter.thin{display:inline-block;width:32px;height:5px;border-radius:3px;background:var(--cell-off);overflow:hidden;margin-right:.22em}
.${BAR} .meter.thin i{display:block;height:100%;border-radius:3px;background:var(--kc)}
.${BAR} .meter.ring{display:inline-flex;width:13px;height:13px;margin-right:.18em}
.${BAR} .meter.ring svg,.${BAR} .gauge svg{width:100%;height:100%;display:block;transform:rotate(-90deg)}
.${BAR} .trk{fill:none;stroke:var(--cell-off);stroke-width:3.6}
.${BAR} .arc{fill:none;stroke:var(--kc);stroke-width:3.6;stroke-linecap:round}
.${BAR} .pulse{width:7px;height:7px;border-radius:50%;background:var(--kc);margin-right:.42em;animation:agyBarPulse 1.2s ease-in-out infinite}
@keyframes agyBarPulse{0%,100%{opacity:1}50%{opacity:.25}}
.${BAR} .ln > .seg.k-run,.${BAR} .seg.sst{flex:1 1 0;min-width:0;max-width:max-content;overflow:hidden}
.${BAR} .ln > .seg.k-run > *,.${BAR} .seg.sst > *{flex-shrink:0}
.${BAR}[data-skin="plain"] .ln > .seg.k-run,.${BAR}[data-skin="plain"] .seg.sst{margin-left:.55em;padding-left:1.05em}
.${BAR}[data-skin="plain"] .ln > .seg.k-run::before,.${BAR}[data-skin="plain"] .seg.sst::before{left:0}
.${BAR} .seg.k-run.idle{visibility:hidden;pointer-events:none}
.${BAR} .ln.sub > .seg[data-col]{box-sizing:border-box}
.${BAR} .ln.sub > .seg.ph::before{visibility:hidden}
.${BAR} .ln.sub > .seg.ph{background:none!important;clip-path:none!important}
.${BAR}[data-skin="pill"] .ln{margin-left:0;clip-path:none;gap:3px 5px}
.${BAR}[data-skin="pill"] .ln.sub{margin-left:.4em}
.${BAR}[data-skin="pill"] .seg{margin-left:0;padding:0 .58em;border-radius:999px;line-height:1.6;background:color-mix(in srgb,var(--kc) 13%,transparent)}
.${BAR}[data-skin="pill"] .seg::before{content:none}
.${BAR}[data-skin="powerline"] .ln{margin-left:0;clip-path:none;row-gap:3px}
.${BAR}[data-skin="powerline"] .ln.sub{margin-left:.4em}
.${BAR}[data-skin="powerline"] .seg{margin-left:-.3em;padding:0 .92em 0 1.02em;line-height:1.72;background:var(--kc);color:var(--pl-ink);clip-path:polygon(0 0,calc(100% - .58em) 0,100% 50%,calc(100% - .58em) 100%,0 100%,.58em 50%)}
.${BAR}[data-skin="powerline"] .seg:first-child{margin-left:0;padding-left:.68em;border-radius:5px 0 0 5px;clip-path:polygon(0 0,calc(100% - .58em) 0,100% 50%,calc(100% - .58em) 100%,0 100%)}
.${BAR}[data-skin="powerline"] .seg::before{content:none}
.${BAR}[data-skin="powerline"] .seg .pace-ok,.${BAR}[data-skin="powerline"] .seg .pace-hi,.${BAR}[data-skin="powerline"] .seg .real.odd,.${BAR}[data-skin="powerline"] .seg .tree{color:inherit;font-weight:700}
.${BAR}[data-skin="powerline"] .seg .real{opacity:.85}
.${BAR}[data-skin="powerline"] .cells i,.${BAR}[data-skin="powerline"] .meter.thin{background:color-mix(in srgb,var(--pl-ink) 30%,transparent)}
.${BAR}[data-skin="powerline"] .cells i.on,.${BAR}[data-skin="powerline"] .meter.thin i,.${BAR}[data-skin="powerline"] .pulse{background:var(--pl-ink)}
.${BAR}[data-skin="powerline"] .trk{stroke:color-mix(in srgb,var(--pl-ink) 30%,transparent)}
.${BAR}[data-skin="powerline"] .arc{stroke:var(--pl-ink)}
.${BAR}[data-skin="powerline"] .comp,.${BAR}[data-skin="powerline"] .costly{color:inherit;background:color-mix(in srgb,var(--pl-ink) 22%,transparent)}
.${BAR} .k-model{--kc:var(--k-model)} .${BAR} .k-context{--kc:var(--k-context)} .${BAR} .k-cache{--kc:var(--k-cache)}
.${BAR} .k-timing{--kc:var(--k-timing)} .${BAR} .k-tokens{--kc:var(--k-tokens)} .${BAR} .k-quota{--kc:var(--k-quota)}
.${BAR} .k-git{--kc:var(--k-git)} .${BAR} .k-agent{--kc:var(--k-agent)} .${BAR} .k-cost{--kc:var(--k-cost)} .${BAR} .k-warn{--kc:var(--k-warn)}
.${BAR} .k-crit{--kc:var(--k-crit)} .${BAR} .k-run{--kc:var(--k-run)} .${BAR} .k-muted{--kc:var(--k-muted)}
.${TIP}{position:fixed;z-index:2147483600;pointer-events:none;box-sizing:border-box;max-width:min(420px,calc(100vw - 16px));
 padding:.38rem .62rem .42rem;border:1px solid var(--sep);border-radius:8px;background:var(--p-bg);box-shadow:0 6px 20px rgba(0,0,0,.18);
 font-family:"Google Sans Code","JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,"PingFang SC",monospace;
 font-size:11.5px;line-height:1.55;font-weight:500;color:var(--k-text);white-space:pre-line;overflow-wrap:anywhere}
`;

  const applyLookAttrs = (elNode, look, dark) => {
    elNode.classList.toggle('dark', !!dark);
    elNode.setAttribute('data-pal', look.palette);
    elNode.setAttribute('data-skin', look.skin);
    elNode.setAttribute('data-icons', look.icons);
  };

  const bars = () => {
    const boxes = document.querySelectorAll(INPUT);
    return Array.from(boxes).map((box) => {
      let bar = box.previousElementSibling;
      if (!bar || !bar.classList.contains(BAR)) {
        bar = el('div', BAR);
        bar.setAttribute('data-agy-no-i18n', '');
        box.parentElement.insertBefore(bar, box);
        bar._fresh = true;
      }
      return { bar, box, id: paneId(box, boxes.length) };
    });
  };

  const render = (force) => {
    if (!document.body) return;
    if (!style.isConnected) (document.head || document.documentElement).appendChild(style);
    const dark = isDark();
    const enabled = !data || !data.opts || data.opts.enabled !== false;
    const look = normLook(data && data.opts && data.opts.look);
    const list = bars();
    paneIds = [...new Set(list.map((x) => x.id).filter(Boolean))].sort();
    for (const { bar, box, id } of list) {
      bar.hidden = !enabled;
      applyLookAttrs(bar, look, dark);
      if (!force && !bar._fresh && bar._for === id) continue;
      bar._fresh = false;
      bar._for = id;
      bar.replaceChildren(...build(id, box, look, null));
      alignCols(bar);
    }
    tipAfterRender();
  };

  const onHide = () => hideTip();
  const bindTipListeners = () => {
    document.addEventListener('mousemove', onTipMove, true);
    document.addEventListener('mousedown', onHide, true);
    document.addEventListener('wheel', onHide, true);
    document.addEventListener('scroll', onHide, true);
    document.addEventListener('keydown', onHide, true);
    document.addEventListener('mouseleave', onHide);
    window.addEventListener('blur', onHide);
  };
  const unbindTipListeners = () => {
    document.removeEventListener('mousemove', onTipMove, true);
    document.removeEventListener('mousedown', onHide, true);
    document.removeEventListener('wheel', onHide, true);
    document.removeEventListener('scroll', onHide, true);
    document.removeEventListener('keydown', onHide, true);
    document.removeEventListener('mouseleave', onHide);
    window.removeEventListener('blur', onHide);
  };

  const start = () => {
    bindTipListeners();
    render(true);
    let ticks = 0;
    timer = setInterval(() => {
      ticks++;
      tickClocks();
      render(ticks % 30 === 0);
    }, 1000);
  };

  window.__agyBar = {
    PALETTES: Object.entries(PALETTE_DEFS).map(([id, [name, desc]]) => ({ id, name, desc })),
    missExplain,
    update(payload) { data = payload; render(true); },
    destroy() {
      clearInterval(timer);
      hideTip();
      unbindTipListeners();
      if (tip.el) tip.el.remove();
      document.querySelectorAll('.' + BAR).forEach((b) => b.remove());
      style.remove();
      delete window.__agyBar;
    },
    panes() { return { panes: paneIds, hasData: !!data }; },
    // Used by the side panel for its sticky live preview and the 8 Theme gallery cards.
    renderInto(container, payload, id, dark, lookOverride, optsExtra) {
      if (!style.isConnected) (document.head || document.documentElement).appendChild(style);
      const prev = data;
      data = payload;
      const look = normLook(lookOverride || (payload && payload.opts && payload.opts.look));
      container.classList.add(BAR);
      applyLookAttrs(container, look, dark);
      container.replaceChildren(...build(id, null, look, optsExtra || null));
      alignCols(container);
      data = prev;
      tipAfterRender();
    },
  };

  if (document.body) start();
  else document.addEventListener('DOMContentLoaded', start, { once: true });
})();
