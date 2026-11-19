// In-page statusline renderer, injected by `agy-statusline sidecar` over CDP.
// No data logic lives here: the sidecar pushes ready-made numbers with window.__agyBar.update(payload)
// and polls window.__agyBar.panes() to learn which conversation each chat input belongs to.
// Re-injecting replaces the previous instance.
(function () {
  'use strict';
  if (window.__agyBar) { try { window.__agyBar.destroy(); } catch (e) {} }

  const BAR = 'agy-statusline-bar';
  const INPUT = '[data-testid="agent-input-box"]';
  // The main pane's ancestor carries data-cascade-id; a subagent tab keeps it on a nearby sibling.
  // Sidebar rows also carry data-cascade-id, so only these two elements are trusted.
  const PANE = '[data-testid="conversation-view"][data-cascade-id], [data-testid="subagent-tab-conversation"][data-cascade-id]';
  const UUID = /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/;

  let data = null;
  let paneIds = [];
  let timer = 0;

  // ---------------------------------------------------------------- formatting
  const fmtTok = (n) => {
    if (n == null) return '–';
    if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1).replace(/\.0$/, '') + 'M';
    if (n >= 1e3) return Math.round(n / 1e3) + 'k';
    return String(n);
  };
  const fmtUsd = (x) => '$' + (x >= 100 ? x.toFixed(0) : x >= 10 ? x.toFixed(1) : x.toFixed(2));
  const pad = (n) => String(n).padStart(2, '0');
  const fmtReset = (ms) => {
    if (!ms) return '';
    const d = new Date(ms);
    const hm = pad(d.getHours()) + ':' + pad(d.getMinutes());
    if (ms - Date.now() < 20 * 3600e3) return hm;
    return ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][d.getDay()] + ' ' + hm;
  };
  const pct = (f) => Math.round(f * 100) + '%';

  // ---------------------------------------------------------------- DOM helpers
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const SVG = {
    model: '<path d="M12 2l2.4 7.6L22 12l-7.6 2.4L12 22l-2.4-7.6L2 12l7.6-2.4z"/>',
    cache: '<ellipse cx="12" cy="5.5" rx="8" ry="3"/><path d="M4 8.5c0 1.7 3.6 3 8 3s8-1.3 8-3V12c0 1.7-3.6 3-8 3s-8-1.3-8-3zm0 6c0 1.7 3.6 3 8 3s8-1.3 8-3V18c0 1.7-3.6 3-8 3s-8-1.3-8-3z"/>',
    ttft: '<path d="M6 2h12v5l-4 5 4 5v5H6v-5l4-5-4-5zm2 2v2.3l4 5 4-5V4z"/>',
    speed: '<path d="M13 2L4 14h7l-1 8 9-12h-7z"/>',
    agent: '<path d="M9 2h6v3h3a2 2 0 012 2v11a2 2 0 01-2 2H6a2 2 0 01-2-2V7a2 2 0 012-2h3zm0 8a1.5 1.5 0 100 3 1.5 1.5 0 000-3zm6 0a1.5 1.5 0 100 3 1.5 1.5 0 000-3z"/>',
    git: '<path d="M6 3a3 3 0 011 5.83v6.34A3 3 0 115 15.17V8.83A3 3 0 016 3zm12 3a3 3 0 01-1.02 5.83A6 6 0 0111 17h-.17a3 3 0 110-2H11a4 4 0 004-3.83A3 3 0 0118 6z"/>',
    done: '<path d="M9 16.2l-4.2-4.2L3.4 13.4 9 19 21 7l-1.4-1.4z"/>',
    cost: '<circle cx="12" cy="12" r="10"/><text x="12" y="16.5" text-anchor="middle" font-size="13" font-weight="700" font-family="Arial,sans-serif" fill="var(--bg-cut)">$</text>',
  };
  const icon = (name) => {
    const s = el('span', 'ic');
    s.innerHTML = '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">' + SVG[name] + '</svg>';
    return s;
  };
  const pie = (f) => {
    const s = el('span', 'ic pie');
    s.style.setProperty('--p', String(Math.max(0, Math.min(1, f || 0)) * 360) + 'deg');
    return s;
  };
  const cells = (f) => {
    const box = el('span', 'cells');
    const on = Math.round(Math.max(0, Math.min(1, f)) * 10);
    for (let i = 0; i < 10; i++) box.append(el('i', i < on ? 'on' : ''));
    return box;
  };
  // A segment: colour key, leading icon node, then text parts joined by a dim dot.
  const seg = (key, lead, parts, title) => {
    const s = el('span', 'seg k-' + key);
    if (lead) s.append(lead);
    parts.filter((p) => p != null && p !== '').forEach((p, i) => {
      if (i) s.append(el('span', 'dot', '·'));
      s.append(typeof p === 'string' ? el('span', '', p) : p);
    });
    if (title) s.title = title;
    return s;
  };
  const dim = (t, cls) => el('span', 'real' + (cls ? ' ' + cls : ''), t);

  // ---------------------------------------------------------------- segments
  const modelSeg = (a) => {
    if (!a.model) return null;
    const parts = [a.model];
    const fam = a.model.split(' (')[0].toLowerCase().replace(/\s+/g, '-');
    if (a.respModel && !fam.includes(a.respModel.toLowerCase()) && !a.respModel.toLowerCase().includes(fam)) {
      parts.push(dim(a.respModel));
    }
    return seg('model', icon('model'), parts, 'Model shown in Antigravity · model the API reported');
  };
  const ctxSeg = (a) => {
    if (a.ctxUsed == null) return null;
    const f = a.ctxMax ? a.ctxUsed / a.ctxMax : 0;
    const key = f >= 0.9 ? 'crit' : f >= 0.75 ? 'warn' : 'context';
    const parts = [a.ctxMax ? pct(f) : null, fmtTok(a.ctxUsed) + (a.ctxMax ? ' / ' + fmtTok(a.ctxMax) : '')];
    if (a.compactions) parts.push(el('span', 'comp', 'compacted ' + a.compactions + '×'));
    return seg(key, pie(f), parts, 'Context window: estimated tokens in the next request / limit');
  };
  const cacheSeg = (a) => (a.cacheHit == null ? null
    : seg('cache', icon('cache'), [fmtTok(a.cacheRead), pct(a.cacheHit) + ' hit'], 'Last call: tokens read from prompt cache · share of the prompt'));
  const callSegs = (a, sg) => {
    const out = [];
    if (a.ttftMs != null && sg.ttft !== false) out.push(seg('timing', icon('ttft'), [(a.ttftMs / 1000).toFixed(1) + 's'], 'Last call: time to first token'));
    if (a.tps != null && sg.speed !== false) out.push(seg('tokens', icon('speed'), [Math.round(a.tps) + ' tok/s'], 'Last call: output tokens / streaming time (' + fmtTok(a.outTokens) + ' tokens)'));
    return out;
  };
  const runSeg = () => {
    const s = el('span', 'seg k-run');
    s.append(el('span', 'pulse'), el('span', '', 'running'));
    return s;
  };
  const costSeg = (v, today, showConv) => {
    const parts = [showConv ? fmtUsd(v.costConv) : null];
    if (today != null) parts.push(dim(fmtUsd(today) + ' today'));
    let title = 'Estimated at public API list prices (not what the subscription charges)\nThis conversation incl. subagents: ' + fmtUsd(v.costConv);
    if (v.costLast) {
      title += '\nLast call: input ' + fmtUsd(v.costLast.in) + ' · cache ' + fmtUsd(v.costLast.cache) + ' · output ' + fmtUsd(v.costLast.out);
    }
    if (v.priceName) title += '\nPriced as ' + v.priceName;
    return seg('cost', icon('cost'), parts, title);
  };
  const gitSeg = (g, path) => {
    if (!g || !g.branch) return null;
    const parts = [g.branch + (g.dirty ? '*' : '')];
    if (g.ahead || g.behind) parts.push((g.ahead ? '↑' + g.ahead : '') + (g.behind ? '↓' + g.behind : ''));
    if (g.added || g.deleted) parts.push(dim('+' + g.added + ' −' + g.deleted));
    const title = 'Git: ' + (path || '') + (g.dirty ? '\nUncommitted changes' : '') +
      (g.ahead || g.behind ? '\nAhead ' + g.ahead + ' · behind ' + g.behind + ' of upstream' : '') +
      (g.added || g.deleted ? '\nLines vs HEAD: +' + g.added + ' −' + g.deleted : '');
    return seg('git', icon('git'), parts, title);
  };
  // Hides the 5h or weekly bucket when its segment is switched off.
  const quotaOn = (seg) => (q) => (q.window === '5h' ? seg.quota_5h !== false : q.window === 'weekly' ? seg.quota_7d !== false : true);
  const quotaSegs = (qs) => (qs || []).map((q) => {
    const key = q.used >= 0.9 ? 'crit' : q.used >= 0.7 ? 'warn' : 'quota';
    const parts = [cells(q.used), pct(q.used), fmtReset(q.resetMs)];
    if (q.window === 'weekly' && q.pace != null && Math.abs(q.pace) >= 0.01) {
      parts.push(el('span', q.pace > 0 ? 'pace-hi' : 'pace-ok', (q.pace > 0 ? '↑' : '↓') + Math.round(Math.abs(q.pace) * 100) + '%'));
    }
    const label = q.window === '5h' ? '5h' : q.window === 'weekly' ? '7d' : q.window;
    const title = (q.window === '5h' ? '5-hour' : 'Weekly') + ' quota used, resets ' + new Date(q.resetMs || 0).toLocaleString() +
      (q.pace != null ? '\nvs. even pace: ' + (q.pace > 0 ? '+' : '') + Math.round(q.pace * 100) + '%' : '');
    return seg(key, el('span', 'tag', label), parts, title);
  });
  const agentSeg = (a) => seg('agent', icon('agent'), [a.subType || 'subagent', a.subRole && a.subRole !== a.subType ? a.subRole : ''], 'Subagent ' + a.id);
  const stateSeg = (a) => {
    if (a.running) return runSeg();
    if (!a.lastAt) return null;
    const d = new Date(a.lastAt);
    return seg('muted', icon('done'), ['done ' + pad(d.getHours()) + ':' + pad(d.getMinutes())]);
  };

  const line = (items, cls) => {
    const l = el('div', 'ln' + (cls ? ' ' + cls : ''));
    l.append(...items.filter(Boolean));
    return l.childElementCount ? l : null;
  };

  // ---------------------------------------------------------------- build
  // The model picker inside this input box; with two panes side by side each box has its own.
  const selectedModel = (box) => {
    const t = (box && box.querySelector('[data-testid="model-selector-trigger"]')) ||
      document.querySelector('[data-testid="model-selector-trigger"]');
    return t ? t.textContent.trim() : '';
  };
  // The selector reads e.g. "Gemini 4 Neon Extra" (model + effort); a group lists
  // "Models within this group: Gemini 4 Neon, Gemini 4 Carbon, …". Match the longest listed name it starts with.
  const homeQuota = (sel) => {
    const groups = (data && data.quotaGroups) || [];
    let best = null;
    let bestLen = 0;
    for (const g of groups) {
      const names = String(g.models || '').replace(/^[^:]*:/, '').split(',').map((s) => s.trim()).filter(Boolean);
      for (const n of names) {
        if (sel.startsWith(n) && n.length > bestLen) { best = g; bestLen = n.length; }
      }
    }
    const g = best || groups[0];
    return g ? g.buckets : [];
  };

  const build = (paneId, box) => {
    if (!data) return [line([seg('muted', null, ['waiting for agy-statusline sidecar…'])])];
    const o = data.opts || {};
    const sg = o.seg || {};
    const on = (k) => sg[k] !== false;
    const v = paneId && data.views ? data.views[paneId] : null;
    const git = v && on('git') ? gitSeg(v.git, v.workspace) : null;
    if (!v || !v.main || !v.main.calls) {
      const items = [];
      const m = selectedModel(box);
      if (m && on('model')) items.push(seg('model', icon('model'), [m]));
      items.push(git);
      if (v && v.main && v.main.running && on('running')) items.push(runSeg());
      items.push(...quotaSegs(homeQuota(m).filter(quotaOn(sg))));
      if (on('today') && data.today) items.push(seg('cost', icon('cost'), [dim(fmtUsd(data.today) + ' today')]));
      return [line(items)].filter(Boolean);
    }
    const a = v.main;
    const showToday = on('today') && !v.isSub;
    const l1 = [v.isSub ? agentSeg(a) : null, on('model') ? modelSeg(a) : null, git,
      on('context') ? ctxSeg(a) : null, on('cache') ? cacheSeg(a) : null,
      on('cost') || showToday ? costSeg(v, showToday ? data.today : null, on('cost')) : null,
      a.running && on('running') ? runSeg() : null];
    const l2 = [...callSegs(a, sg), ...quotaSegs((v.quota || []).filter(quotaOn(sg)))];
    const lines = [line(l1), line(l2)];
    if (on('subagents')) {
      for (const s of v.subs || []) {
        if (s.id.startsWith('+')) { lines.push(line([seg('muted', null, [s.id + ' more subagents'])], 'sub')); continue; }
        lines.push(line([agentSeg(s), on('model') ? modelSeg(s) : null, on('context') ? ctxSeg(s) : null,
          on('cache') ? cacheSeg(s) : null, ...callSegs(s, sg), on('running') || !s.running ? stateSeg(s) : null], 'sub'));
      }
    }
    if (data.error) lines.push(line([seg('warn', null, [String(data.error).slice(0, 120)], 'agy-statusline sidecar error')]));
    return lines.filter(Boolean);
  };

  // ---------------------------------------------------------------- mount
  const paneId = (box) => {
    const own = box.closest(PANE);
    if (own) return own.getAttribute('data-cascade-id');
    let e = box.parentElement;
    for (let i = 0; i < 8 && e; i++, e = e.parentElement) {
      const d = e.querySelector('[data-testid="subagent-tab-conversation"][data-cascade-id]');
      if (d) return d.getAttribute('data-cascade-id');
    }
    // The URL names the conversation only when one pane is open; with more it can't say which box it is.
    if (document.querySelectorAll(INPUT).length !== 1) return null;
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
.${BAR}{--k-model:#5c64d6;--k-context:#8839ef;--k-cache:#1b8fbf;--k-timing:#c4566a;--k-tokens:#158a90;
 --k-quota:#e0620d;--k-warn:#c9800f;--k-crit:#d20f39;--k-run:#40a02b;--k-muted:#8c8fa1;--k-agent:#df8e1d;--k-cost:#c03d8e;--k-git:#3f8f5a;
 --sep:#c9c6bc;--cell-off:#e6e2d8;--bg-cut:#fff;
 display:flex;flex-direction:column;gap:1px;padding:0 .9rem .35rem;
 font-family:"Google Sans Code","JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,"PingFang SC",monospace;
 font-size:12px;line-height:1.65;font-weight:500;font-variant-numeric:tabular-nums;color:var(--k-muted);cursor:default;user-select:text}
.${BAR}.dark{--k-model:#b4befe;--k-context:#cba6f7;--k-cache:#89dceb;--k-timing:#f2cdcd;--k-tokens:#94e2d5;
 --k-quota:#fab387;--k-warn:#f9e2af;--k-crit:#f38ba8;--k-run:#a6e3a1;--k-muted:#7f849c;--k-agent:#f9e2af;--k-cost:#f5c2e7;--k-git:#a6d189;
 --sep:#585b70;--cell-off:#45475a;--bg-cut:#1e1e2e}
.${BAR}[hidden]{display:none}
.${BAR} .ln{display:flex;flex-wrap:wrap;align-items:center;row-gap:1px;margin-left:-1.6em;clip-path:inset(-6px -6px -6px 1.6em)}
.${BAR} .ln.sub{margin-left:-.5em;font-size:11.5px}
.${BAR} .seg{display:inline-flex;align-items:center;white-space:nowrap;color:var(--kc);margin-left:1.6em;position:relative}
.${BAR} .seg::before{content:"|";color:var(--sep);position:absolute;left:-1.05em;top:0;bottom:0;display:flex;align-items:center;font-weight:400}
.${BAR} .ic{display:inline-flex;width:13px;height:13px;margin-right:.4em;flex:none}
.${BAR} .ic svg{width:100%;height:100%;display:block}
.${BAR} .pie{border-radius:50%;background:conic-gradient(var(--kc) var(--p),var(--cell-off) 0)}
.${BAR} .tag{font-size:10.5px;font-weight:700;margin-right:.45em;opacity:.9}
.${BAR} .dot{margin:0 .45em;opacity:.55}
.${BAR} .real{opacity:.72;font-weight:400}
.${BAR} .comp{color:var(--k-warn);font-weight:600}
.${BAR} .pace-ok{color:var(--k-run)} .${BAR} .pace-hi{color:var(--k-warn)}
.${BAR} .cells{display:inline-flex;gap:2px;margin-right:.1em}
.${BAR} .cells i{display:block;width:5px;height:10px;border-radius:1.5px;background:var(--cell-off)}
.${BAR} .cells i.on{background:var(--kc)}
.${BAR} .pulse{width:7px;height:7px;border-radius:50%;background:var(--kc);margin-right:.45em;animation:agyBarPulse 1.2s ease-in-out infinite}
@keyframes agyBarPulse{0%,100%{opacity:1}50%{opacity:.25}}
.${BAR} .k-model{--kc:var(--k-model)} .${BAR} .k-context{--kc:var(--k-context)} .${BAR} .k-cache{--kc:var(--k-cache)}
.${BAR} .k-timing{--kc:var(--k-timing)} .${BAR} .k-tokens{--kc:var(--k-tokens)} .${BAR} .k-quota{--kc:var(--k-quota)}
.${BAR} .k-git{--kc:var(--k-git)} .${BAR} .k-agent{--kc:var(--k-agent)} .${BAR} .k-cost{--kc:var(--k-cost)} .${BAR} .k-warn{--kc:var(--k-warn)}
.${BAR} .k-crit{--kc:var(--k-crit)} .${BAR} .k-run{--kc:var(--k-run)} .${BAR} .k-muted{--kc:var(--k-muted)}
`;

  const bars = () => Array.from(document.querySelectorAll(INPUT)).map((box) => {
    let bar = box.previousElementSibling;
    if (!bar || !bar.classList.contains(BAR)) {
      bar = el('div', BAR);
      bar.setAttribute('data-agy-no-i18n', '');
      box.parentElement.insertBefore(bar, box);
      bar._fresh = true;
    }
    return { bar, box, id: paneId(box) };
  });

  const render = (force) => {
    if (!document.body) return;
    if (!style.isConnected) (document.head || document.documentElement).appendChild(style);
    const dark = isDark();
    const enabled = !data || !data.opts || data.opts.enabled !== false;
    const list = bars();
    paneIds = [...new Set(list.map((x) => x.id).filter(Boolean))].sort();
    for (const { bar, box, id } of list) {
      bar.hidden = !enabled;
      bar.classList.toggle('dark', dark);
      if (!force && !bar._fresh && bar._for === id) continue;
      bar._fresh = false;
      bar._for = id;
      bar.replaceChildren(...build(id, box));
    }
  };

  const start = () => {
    render(true);
    let ticks = 0;
    timer = setInterval(() => { ticks++; render(ticks % 30 === 0); }, 1000);
  };

  window.__agyBar = {
    update(payload) { data = payload; render(true); },
    destroy() {
      clearInterval(timer);
      document.querySelectorAll('.' + BAR).forEach((b) => b.remove());
      style.remove();
      delete window.__agyBar;
    },
    panes() { return { panes: paneIds, hasData: !!data }; },
    // Used by the side panel to draw the same bar into its own header.
    renderInto(container, payload, id, dark) {
      if (!style.isConnected) (document.head || document.documentElement).appendChild(style);
      data = payload;
      container.classList.add(BAR);
      container.classList.toggle('dark', !!dark);
      container.replaceChildren(...build(id));
    },
  };

  if (document.body) start();
  else document.addEventListener('DOMContentLoaded', start, { once: true });
})();
