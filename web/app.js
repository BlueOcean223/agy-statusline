// Side panel (AUX_PANE). Everything comes from GET /api/panel, built by the same live engine that
// feeds the in-page bar; the header strip is drawn by bar.js so both look identical.
(() => {
  const $ = (id) => document.getElementById(id);
  const convSelect = $('conv-select');
  const livePill = $('live-pill');
  const liveLabel = $('live-label');
  const stripEl = $('ccline-strip');
  const turnsCountEl = $('turns-count');
  const panelOverview = $('panel-overview');
  const panelTurns = $('panel-turns');
  const panelSettings = $('panel-settings');

  const params = new URLSearchParams(location.search);
  const hostConvId = window.sidecar?.conversationId || params.get('conversationId') || '';
  let selectedConvId = hostConvId;
  let activeTab = 'overview';
  let settingsDirty = false;
  let lastJson = '';
  let last = null;

  // Light/dark from the host's --background luminance.
  function syncScheme() {
    const probe = document.createElement('span');
    probe.style.cssText = 'position:absolute;visibility:hidden;color:var(--background, #18181b)';
    document.documentElement.appendChild(probe);
    const c = getComputedStyle(probe).color;
    probe.remove();
    const nums = (c.match(/[\d.]+/g) || []).map(Number);
    if (nums.length >= 3) {
      const scale = c.startsWith('color(') ? 1 : 255;
      const [r, g, b] = nums.slice(0, 3).map((v) => v / scale);
      document.documentElement.dataset.theme = 0.2126 * r + 0.7152 * g + 0.0722 * b < 0.5 ? 'dark' : 'light';
    }
  }
  syncScheme();
  window.addEventListener('message', (e) => {
    if (e.data?.type === 'theme-change') requestAnimationFrame(() => { syncScheme(); if (last) renderStrip(last); });
  });

  for (const btn of document.querySelectorAll('.tab')) {
    btn.addEventListener('click', () => {
      activeTab = btn.dataset.tab;
      for (const b of document.querySelectorAll('.tab')) b.classList.toggle('active', b === btn);
      panelOverview.classList.toggle('hidden', activeTab !== 'overview');
      panelTurns.classList.toggle('hidden', activeTab !== 'turns');
      panelSettings.classList.toggle('hidden', activeTab !== 'settings');
      settingsDirty = false;
      if (last) renderPanels(last, true);
    });
  }

  convSelect.addEventListener('change', () => {
    selectedConvId = convSelect.value;
    lastJson = '';
    poll();
  });

  // ---------------------------------------------------------------- formatting
  function esc(s) {
    return String(s ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  function fmtTok(n) {
    const v = Number(n) || 0;
    const a = Math.abs(v);
    if (a >= 1e6) return (v / 1e6).toFixed(a >= 1e7 ? 1 : 2).replace(/\.?0+$/, '') + 'M';
    if (a >= 1e3) return (v / 1e3).toFixed(a >= 1e5 ? 0 : 1).replace(/\.0$/, '') + 'k';
    return String(Math.round(v));
  }
  function fmtUsd(n) {
    const v = Number(n) || 0;
    if (v === 0) return '$0.00';
    if (v < 0.01) return '$' + v.toFixed(4);
    if (v < 1) return '$' + v.toFixed(3);
    return '$' + v.toFixed(2);
  }
  const pct = (f, d = 1) => (f == null ? '—' : (f * 100).toFixed(d) + '%');
  const pad = (n) => String(n).padStart(2, '0');
  const hms = (ms) => { const d = new Date(ms); return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`; };
  function fmtDur(sec) {
    if (!(sec > 0)) return '—';
    const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = Math.floor(sec % 60);
    return h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
  }
  function fmtLeft(ms) {
    const sec = Math.max(0, (ms - Date.now()) / 1000);
    const d = Math.floor(sec / 86400);
    return d ? `${d}d ${Math.floor((sec % 86400) / 3600)}h` : fmtDur(sec);
  }

  function sparkSvg(series, colorCss) {
    if (!Array.isArray(series) || series.length < 2) return '';
    const w = 180, h = 22;
    const min = Math.min(...series), max = Math.max(...series);
    const span = Math.max(1, max - min);
    const pts = series.map((v, i) => `${((i / (series.length - 1)) * w).toFixed(1)},${(h - 2 - ((v - min) / span) * (h - 4)).toFixed(1)}`).join(' ');
    return `<svg class="spark-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polyline fill="none" stroke="${colorCss}" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" points="${pts}"/></svg>`;
  }
  const kv = (k, v) => `<div class="kv-row"><span class="kv-k">${k}</span><span class="kv-v">${v}</span></div>`;

  // ---------------------------------------------------------------- render
  function renderStrip(p) {
    if (!window.__agyBar || !p.bar) return;
    window.__agyBar.renderInto(stripEl, p.bar, p.id, document.documentElement.dataset.theme === 'dark');
  }

  function renderOverview(p) {
    const { ctx, cache, tokens: t, speed } = p;
    const ctxPct = ctx.max ? (ctx.used / ctx.max) * 100 : 0;
    const ctxColor = ctxPct >= 85 ? 'var(--k-crit)' : ctxPct >= 65 ? 'var(--k-warn)' : 'var(--k-context)';

    const quotaRows = (p.quota || []).map((q, i) => {
      const label = q.window === '5h' ? '5-Hour Window' : q.window === 'weekly' ? 'Weekly Window' : q.window;
      const arrow = q.pace == null || Math.abs(q.pace) < 0.01 ? '' : q.pace > 0 ? ` ↑${Math.round(q.pace * 100)}%` : ` ↓${Math.round(-q.pace * 100)}%`;
      return `
        <div class="kv-row"${i ? ' style="margin-top:8px"' : ''}>
          <span class="kv-k">${label}</span>
          <span class="kv-v">${pct(q.used, 0)} · reset ${q.resetMs ? fmtLeft(q.resetMs) : '—'}${arrow}</span>
        </div>
        <div class="meter-dual">
          <div class="meter"><div class="meter-fill" style="width:${(q.used * 100).toFixed(1)}%;background:var(--k-quota)"></div></div>
          <div class="meter sub"><div class="meter-fill" style="width:${((q.elapsed || 0) * 100).toFixed(1)}%;background:var(--fg-muted)"></div></div>
        </div>`;
    }).join('');

    const subs = p.subagents || [];
    const subagentsSection = subs.length ? `
      <div class="section">
        <div class="sec-head"><span class="sec-title">Subagents (${subs.length})</span><span class="sec-value">${fmtUsd(p.subCost)}</span></div>
        ${subs.map((s) => `<div class="sub-row">
          <div>
            <span>${esc(s.type || 'subagent')}${s.role ? ' · ' + esc(s.role) : ''}</span>
            <span class="sub-meta"> · ${esc(s.model || '—')} · ${fmtTok(s.tokens)} tok · ${fmtUsd(s.cost)} · ${s.running ? 'running' : 'done'}</span>
          </div>
          <button type="button" class="link-btn btn-open-conv" data-conv="${esc(s.id)}">Open</button>
        </div>`).join('')}
      </div>` : '';

    panelOverview.innerHTML = `
      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Context Window</span>
          <span class="sec-value">${ctxPct.toFixed(1)}% · ${fmtTok(ctx.used)} / ${fmtTok(ctx.max)}</span>
        </div>
        <div class="meter"><div class="meter-fill" style="width:${Math.min(100, ctxPct).toFixed(1)}%;background:${ctxColor}"></div></div>
        <div class="kv-grid">
          ${kv('Remaining', fmtTok(ctx.left))}
          ${kv('Compactions', ctx.compactions + '×')}
          ${kv('Last Call Δ', (ctx.delta >= 0 ? '+' : '') + fmtTok(ctx.delta))}
          ${kv('Est. Calls Left', ctx.turnsLeft ? '~' + ctx.turnsLeft : '—')}
        </div>
        ${ctx.series.length >= 2 ? `<div class="spark-wrap"><span class="kv-k" style="font-size:11px">Trend</span>${sparkSvg(ctx.series, 'var(--k-context)')}</div>` : ''}
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Prompt Cache</span><span class="sec-value">${pct(cache.lastHit)} hit</span></div>
        <div class="meter"><div class="meter-fill" style="width:${((cache.lastHit || 0) * 100).toFixed(1)}%;background:var(--k-cache)"></div></div>
        <div class="kv-grid">
          ${kv('Session Avg Hit', pct(cache.sessionHit))}
          ${kv(`Warmth (${Math.round(cache.ttlSec / 60)}m TTL)`, cache.warmLeftSec ? fmtDur(cache.warmLeftSec) : 'cold')}
          ${kv('Low-hit Calls', cache.misses + '×')}
          ${kv('Saved vs. Uncached', fmtUsd(cache.savedUsd))}
        </div>
      </div>

      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Tokens &amp; Cost${t.includesSubagents ? ' (incl. subagents)' : ''}</span>
          <span class="sec-value">${fmtTok(t.total)} · ${fmtUsd(t.cost)}</span>
        </div>
        <div class="kv-grid">
          ${kv('Uncached Input', `${fmtTok(t.fresh)} (${fmtUsd(t.costIn)})`)}
          ${kv('Cache Read', `${fmtTok(t.cache)} (${fmtUsd(t.costCache)})`)}
          ${kv('Model Output', `${fmtTok(t.out)} (${fmtUsd(t.costOut)})`)}
          ${kv('Burn Rate', t.ratePerHr != null ? fmtUsd(t.ratePerHr) + '/h' : '—')}
          ${kv('Today, All Convs', fmtUsd(t.todayCost))}
          ${kv('Today Tokens', fmtTok(t.todayTokens))}
        </div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Speed &amp; Timing</span><span class="sec-value">${speed.lastTps ? speed.lastTps.toFixed(1) + ' tok/s' : '—'}</span></div>
        <div class="kv-grid">
          ${kv('Avg Speed', speed.avgTps ? speed.avgTps.toFixed(1) + ' tok/s' : '—')}
          ${kv('Last TTFT', speed.lastTtftMs ? (speed.lastTtftMs / 1000).toFixed(1) + 's' : '—')}
          ${kv('Avg TTFT', speed.avgTtftMs ? (speed.avgTtftMs / 1000).toFixed(1) + 's' : '—')}
          ${kv('Session Wall', fmtDur(speed.wallSec))}
        </div>
        ${speed.series.length >= 2 ? `<div class="spark-wrap"><span class="kv-k" style="font-size:11px">TPS</span>${sparkSvg(speed.series, 'var(--k-timing)')}</div>` : ''}
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Quota (Antigravity)</span><span class="sec-value">used · window elapsed</span></div>
        ${quotaRows || '<div class="form-hint">quota not available</div>'}
      </div>

      ${subagentsSection}
    `;

    for (const btn of panelOverview.querySelectorAll('.btn-open-conv')) {
      btn.addEventListener('click', () => {
        const cid = btn.dataset.conv;
        if (window.sidecar?.ui?.toggleConversation) window.sidecar.ui.toggleConversation(cid);
        else { selectedConvId = cid; lastJson = ''; poll(); }
      });
    }
  }

  function renderTurns(p) {
    const turns = p.turns || [];
    if (!turns.length) {
      panelTurns.innerHTML = '<div style="padding:24px 0;color:var(--fg-muted);font-size:12px">暂无模型调用记录</div>';
      return;
    }
    panelTurns.innerHTML = turns.slice().reverse().map((t) => `
      <div class="turn-row">
        <div class="turn-left">
          <span class="turn-step">#${t.step ?? '–'} · ${hms(t.at)} · ctx ${fmtTok(t.ctx)} (${Math.round(t.hit * 100)}% hit)</span>
          <span class="turn-label">${esc(t.label)}</span>
        </div>
        <div class="turn-right">
          <div>${fmtUsd(t.cost)} · +${fmtTok(t.out)} out</div>
          <div style="color:var(--fg-muted)">${t.tps ? Math.round(t.tps) + ' tok/s' : '—'}${t.ttftMs ? ' · ' + (t.ttftMs / 1000).toFixed(1) + 's TTFT' : ''}</div>
        </div>
      </div>`).join('');
  }

  function renderSettings(p, force = false) {
    if (settingsDirty && !force) return;
    const c = p.config;
    const opt = (v, label) => `<option value="${v}" ${c.context_limit_override === v ? 'selected' : ''}>${label}</option>`;
    panelSettings.innerHTML = `
      <div class="section">
        <div class="sec-head"><span class="sec-title">输入框状态栏 (In-page Statusline)</span></div>
        <div class="form-hint" id="live-status" style="margin-bottom:8px;line-height:1.6">…</div>
        <div class="form-row">
          <div class="form-label"><span>Show Statusline Above Input</span><span class="form-hint">需要 Antigravity 带 --remote-debugging-port 启动</span></div>
          <input id="cfg-bar-on" type="checkbox" ${c.bar_enabled ? 'checked' : ''} />
        </div>
        <div class="form-row"><div class="form-label"><span>Cost (conversation · today)</span></div><input id="cfg-bar-cost" type="checkbox" ${c.bar_show_cost ? 'checked' : ''} /></div>
        <div class="form-row"><div class="form-label"><span>5h / Weekly Quota</span></div><input id="cfg-bar-quota" type="checkbox" ${c.bar_show_quota ? 'checked' : ''} /></div>
        <div class="form-row"><div class="form-label"><span>Running Subagent Rows</span></div><input id="cfg-bar-subs" type="checkbox" ${c.bar_show_subagents ? 'checked' : ''} /></div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Numbers (config.toml)</span></div>
        <div class="form-row">
          <div class="form-label"><span>Context Limit</span><span class="form-hint">默认用后台报告的上限（maxContextTokens）</span></div>
          <select id="cfg-ctx-win" class="field-select">
            ${opt(0, '后台报告的上限')}${opt(200000, '200K')}${opt(256000, '256K')}${opt(512000, '512K')}${opt(1000000, '1M')}
          </select>
        </div>
        <div class="form-row">
          <div class="form-label"><span>Prompt Cache TTL (s)</span><span class="form-hint">Overview 里“缓存热度”的倒计时</span></div>
          <input id="cfg-ttl" type="number" step="30" min="30" class="field-input" value="${c.cache_ttl_sec}" />
        </div>
        <div class="form-row">
          <div class="form-label"><span>Include Subagents in Totals</span><span class="form-hint">面板 Tokens &amp; Cost 是否计入子代理</span></div>
          <input id="cfg-sub" type="checkbox" ${c.include_subagents ? 'checked' : ''} />
        </div>
        <div class="form-hint" style="margin-top:6px">模型单价在 config.toml 的 [[models]] 里改，按顺序匹配“实际模型名 + 显示名”。</div>
        <div style="margin-top:12px;display:flex;justify-content:flex-end">
          <button id="btn-save-cfg" type="button" class="primary-btn">Save to config.toml</button>
        </div>
      </div>
    `;
    panelSettings.querySelectorAll('input, select').forEach((el) => {
      el.addEventListener('focus', () => { settingsDirty = true; });
      el.addEventListener('change', () => { settingsDirty = true; });
    });
    panelSettings.querySelector('#btn-save-cfg').addEventListener('click', async () => {
      const payload = {
        context_limit_override: Number($('cfg-ctx-win').value) || 0,
        cache_ttl_sec: Number($('cfg-ttl').value) || 300,
        include_subagents: $('cfg-sub').checked,
        bar_enabled: $('cfg-bar-on').checked,
        bar_show_cost: $('cfg-bar-cost').checked,
        bar_show_quota: $('cfg-bar-quota').checked,
        bar_show_subagents: $('cfg-bar-subs').checked,
      };
      const f = window.sidecar?.fetch ? window.sidecar.fetch.bind(window.sidecar) : fetch;
      const res = await f('/api/config', { method: 'POST', body: JSON.stringify(payload) });
      if (res.ok) {
        settingsDirty = false;
        lastJson = '';
        await poll();
      }
    });
    updateLiveStatus();
  }

  async function updateLiveStatus() {
    const box = document.getElementById('live-status');
    if (!box) return;
    try {
      const j = await (await fetch('/api/live-status', { cache: 'no-store' })).json();
      box.innerHTML = `CDP: ${esc(j.cdp || '—')}<br>language_server: ${esc(j.ls || '—')}` +
        (j.panes && j.panes.length ? `<br>showing: ${j.panes.map((x) => esc(String(x).slice(0, 8))).join(', ')}` : '');
    } catch (e) {
      box.textContent = 'live status unavailable';
    }
  }

  function renderPanels(p, forceSettings = false) {
    turnsCountEl.textContent = p.turns?.length ? String(p.turns.length) : '';
    if (activeTab === 'overview') renderOverview(p);
    else if (activeTab === 'turns') renderTurns(p);
    else if (activeTab === 'settings') renderSettings(p, forceSettings);
  }

  function renderEmpty(msg) {
    stripEl.replaceChildren();
    panelOverview.innerHTML = `<div style="padding:24px 0;color:var(--fg-muted);font-size:12px">${esc(msg)}</div>`;
  }

  async function poll() {
    try {
      const qs = new URLSearchParams();
      if (selectedConvId) qs.set('conversationId', selectedConvId);
      const res = await fetch(`/api/panel?${qs}`, { cache: 'no-store' });
      if (!res.ok) return;
      const text = await res.text();
      if (text === lastJson) return;
      lastJson = text;
      const p = JSON.parse(text);
      if (!p.id) { renderEmpty(p.error || '还没有对话'); return; }
      last = p;

      const recents = p.recent || [];
      const autoLabel = (hostConvId ? 'Current Pane · ' : 'Auto · ') + (recents[0]?.title || '');
      convSelect.innerHTML = `<option value="${esc(hostConvId)}">${esc(autoLabel)}</option>` +
        recents.filter((c) => c.id !== hostConvId).map((c) =>
          `<option value="${esc(c.id)}" ${c.id === selectedConvId ? 'selected' : ''}>${esc(c.title)}</option>`).join('');

      livePill.classList.toggle('running', !!p.running);
      liveLabel.textContent = p.activity || (p.running ? 'Running' : 'Idle');

      renderStrip(p);
      renderPanels(p, false);
    } catch (err) {
      console.error('statusline poll failed:', err);
    }
  }

  poll();
  setInterval(poll, 1500);
})();
