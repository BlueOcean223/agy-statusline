// Side panel (AUX_PANE): Overview, Turns, Usage (daily chart + drill-down + quota forecast),
// Theme (Option B: 8 live mini-bar gallery cards + layered look controls), and Settings (segments,
// Chinese UI, cross-platform debug relaunch, self-update, and online data sync).
(() => {
  const $ = (id) => document.getElementById(id);
  const convSelect = $('conv-select');
  const livePill = $('live-pill');
  const liveLabel = $('live-label');
  const stripEl = $('status-strip');
  const turnsCountEl = $('turns-count');
  const updateDot = $('update-dot');
  const panelOverview = $('panel-overview');
  const panelTurns = $('panel-turns');
  const panelUsage = $('panel-usage');
  const panelTheme = $('panel-theme');
  const panelSettings = $('panel-settings');

  const params = new URLSearchParams(location.search);
  const hostConvId = window.sidecar?.conversationId || params.get('conversationId') || '';
  let selectedConvId = hostConvId;
  let activeTab = 'overview';

  // Settings / Theme draft merged onto `last.config` for instant live previews before debounced save.
  let draft = null;
  let saveTimer = 0;
  let lastJson = '';
  let last = null;

  // Usage tab state
  let usageSpan = 7;
  let usageData = null;
  let usageLoading = false;
  let usageDayDate = null;
  let usageDayData = null;
  let usageHoverCard = null;
  let usageDayRefreshTimer = 0;

  // Update center state
  let updateStatus = null;
  let updatePollTimer = 0;

  // ---------------------------------------------------------------- scheme sync
  function syncScheme() {
    if (!getComputedStyle(document.documentElement).getPropertyValue('--background').trim()) {
      document.documentElement.dataset.theme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      return;
    }
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
  const isDarkTheme = () => document.documentElement.dataset.theme === 'dark';
  syncScheme();
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    syncScheme();
    if (last) {
      renderStrip(last);
      if (activeTab === 'theme') renderTheme(last, true);
    }
  });
  window.addEventListener('message', (e) => {
    if (e.data?.type === 'theme-change') {
      requestAnimationFrame(() => {
        syncScheme();
        if (last) {
          renderStrip(last);
          if (activeTab === 'theme') renderTheme(last, true);
        }
      });
    }
  });

  // ---------------------------------------------------------------- tabs
  for (const btn of document.querySelectorAll('.tab')) {
    btn.addEventListener('click', () => {
      activeTab = btn.dataset.tab;
      for (const b of document.querySelectorAll('.tab')) b.classList.toggle('active', b === btn);
      panelOverview.classList.toggle('hidden', activeTab !== 'overview');
      panelTurns.classList.toggle('hidden', activeTab !== 'turns');
      panelUsage.classList.toggle('hidden', activeTab !== 'usage');
      panelTheme.classList.toggle('hidden', activeTab !== 'theme');
      panelSettings.classList.toggle('hidden', activeTab !== 'settings');
      hideHoverCard();
      if (activeTab === 'usage') loadUsage();
      if (activeTab === 'settings') refreshUpdateStatus();
      if (last) renderPanels(last, true);
    });
  }

  convSelect.addEventListener('change', () => {
    selectedConvId = convSelect.value;
    lastJson = '';
    poll();
  });

  // ---------------------------------------------------------------- helpers & formatting
  const postJson = (path, body) => {
    const f = window.sidecar?.fetch ? window.sidecar.fetch.bind(window.sidecar) : fetch;
    return f(path, { method: 'POST', body: JSON.stringify(body || {}) });
  };
  function esc(s) {
    return String(s ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  function fmtTok(n) {
    const v = Number(n) || 0;
    const a = Math.abs(v);
    if (a >= 1e9) return (v / 1e9).toFixed(2).replace(/\.?0+$/, '') + 'B';
    if (a >= 1e6) return (v / 1e6).toFixed(a >= 1e7 ? 1 : 2).replace(/\.?0+$/, '') + 'M';
    if (a >= 1e3) return (v / 1e3).toFixed(a >= 1e5 ? 0 : 1).replace(/\.0$/, '') + 'k';
    return String(Math.round(v));
  }
  function fmtUsd(n) {
    const v = Number(n) || 0;
    if (v === 0) return '$0.00';
    if (v < 0.01) return '$' + v.toFixed(4);
    if (v < 1) return '$' + v.toFixed(3);
    if (v >= 100) return '$' + Math.round(v).toLocaleString('en-US');
    return '$' + v.toFixed(2);
  }
  function fmtUsdShort(n) {
    const v = Number(n) || 0;
    if (v === 0) return '$0';
    if (v < 1) return '$' + v.toFixed(2);
    if (v >= 100) return '$' + Math.round(v);
    return '$' + v.toFixed(1);
  }
  const pct = (f, d = 1) => (f == null ? '—' : (f * 100).toFixed(d) + '%');
  const pad = (n) => String(n).padStart(2, '0');
  const hms = (ms) => { const d = new Date(ms); return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`; };
  const hm = (ms) => { const d = new Date(ms); return `${pad(d.getHours())}:${pad(d.getMinutes())}`; };
  const mdHm = (ms) => { const d = new Date(ms); return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`; };
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
  function fmtAgo(ms) {
    if (!ms) return '尚未';
    const sec = Math.max(0, Math.floor((Date.now() - ms) / 1000));
    if (sec < 60) return `${sec} 秒前`;
    if (sec < 3600) return `${Math.floor(sec / 60)} 分钟前`;
    if (sec < 86400) return `${Math.floor(sec / 3600)} 小时前`;
    return `${Math.floor(sec / 86400)} 天前`;
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

  // Consistent color per model name in charts & legends
  const MODEL_COLORS = [
    'var(--k-model)', 'var(--k-context)', 'var(--k-cache)', 'var(--k-quota)',
    'var(--k-cost)', 'var(--k-git)', 'var(--k-timing)', 'var(--k-tokens)',
  ];
  function modelColor(name, legend) {
    const idx = (legend || []).findIndex((x) => x.name === name);
    return MODEL_COLORS[(idx >= 0 ? idx : 7) % MODEL_COLORS.length];
  }

  // Active config (merging pending draft edits)
  function activeConfig(p) {
    const base = (p && p.config) || {};
    return draft ? { ...base, ...draft } : base;
  }

  function scheduleSave(patch) {
    draft = { ...(draft || {}), ...patch };
    if (last) {
      renderStrip(last);
      if (activeTab === 'theme') renderTheme(last, true);
    }
    setSaveState('保存中…');
    clearTimeout(saveTimer);
    saveTimer = setTimeout(async () => {
      const sent = draft;
      try {
        const res = await postJson('/api/config', sent);
        if (!res.ok) throw new Error('HTTP ' + res.status);
        setSaveState('已保存到 config.toml');
        if (draft === sent) draft = null;
        lastJson = '';
        await poll();
      } catch (e) {
        setSaveState('保存失败：' + e.message, true);
      }
    }, 350);
  }
  function setSaveState(text, bad) {
    const n = $('save-state') || $('theme-save-state');
    if (n) { n.textContent = text; n.classList.toggle('bad', !!bad); }
  }

  // ---------------------------------------------------------------- sticky status strip preview
  function renderStrip(p) {
    if (!window.__agyBar || !p.bar) return;
    const c = activeConfig(p);
    if (c.bar_enabled === false) {
      stripEl.classList.remove('agy-statusline-bar');
      stripEl.innerHTML = '<div class="strip-off">状态栏已关闭</div>';
      return;
    }
    const bar = {
      ...p.bar,
      opts: { ...(p.bar.opts || {}), enabled: true, seg: c.bar_segments || {}, look: c.bar_look || {} },
    };
    window.__agyBar.renderInto(stripEl, bar, p.id, isDarkTheme(), c.bar_look || null, null);
  }

  // ---------------------------------------------------------------- Overview tab
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
          ${t.cacheWrite > 0 ? kv('Cache Write', `${fmtTok(t.cacheWrite)} (${fmtUsd(t.costCacheWrite)})`) : ''}
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

  // ---------------------------------------------------------------- Turns tab
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

  // ---------------------------------------------------------------- Usage tab
  function showHoverCard(html, ev) {
    if (!usageHoverCard) {
      usageHoverCard = document.createElement('div');
      usageHoverCard.className = 'hover-card';
      document.body.appendChild(usageHoverCard);
    }
    usageHoverCard.innerHTML = html;
    usageHoverCard.style.display = 'block';
    const w = usageHoverCard.offsetWidth, h = usageHoverCard.offsetHeight;
    const x = Math.min(window.innerWidth - w - 8, Math.max(8, ev.clientX + 12));
    const y = ev.clientY - h - 10 > 8 ? ev.clientY - h - 10 : Math.min(window.innerHeight - h - 8, ev.clientY + 14);
    usageHoverCard.style.left = x + 'px';
    usageHoverCard.style.top = y + 'px';
  }
  function hideHoverCard() {
    if (usageHoverCard) usageHoverCard.style.display = 'none';
  }

  async function loadUsage(silent = false) {
    if (usageLoading) return;
    usageLoading = true;
    if (!silent && !usageData) {
      panelUsage.innerHTML = '<div style="padding:20px 0;color:var(--fg-muted);font-size:12px">加载每日用量数据中…</div>';
    }
    try {
      const res = await fetch(`/api/usage?days=${usageSpan}`, { cache: 'no-store' });
      if (res.ok) {
        usageData = await res.json();
        if (activeTab === 'usage' && !usageDayDate) renderUsage();
      }
    } catch (e) {
      if (!usageData) panelUsage.innerHTML = `<div class="form-hint err">加载失败：${esc(e.message)}</div>`;
    } finally {
      usageLoading = false;
    }
  }

  async function openUsageDay(date, silent = false) {
    usageDayDate = date;
    clearInterval(usageDayRefreshTimer);
    if (!silent) {
      panelUsage.innerHTML = `<div style="padding:20px 0;color:var(--fg-muted);font-size:12px">加载 ${esc(date)} 调用明细…</div>`;
    }
    try {
      const res = await fetch(`/api/usage/day?date=${encodeURIComponent(date)}`, { cache: 'no-store' });
      if (!res.ok) throw new Error('HTTP ' + res.status);
      const next = await res.json();
      const changed = JSON.stringify(next) !== JSON.stringify(usageDayData);
      usageDayData = next;
      if (activeTab === 'usage' && usageDayDate === date && (!silent || changed)) {
        const scroller = panelUsage.querySelector('.u-table-wrap.calls');
        const prevTop = scroller ? scroller.scrollTop : 0;
        renderUsageDay();
        const after = panelUsage.querySelector('.u-table-wrap.calls');
        if (after && silent) after.scrollTop = prevTop;
      }
      // Silently refresh today's breakdown every 30s without jumping scroll.
      const todayKey = usageData?.today?.date;
      if (date === todayKey) {
        usageDayRefreshTimer = setInterval(() => {
          if (activeTab === 'usage' && usageDayDate === date) openUsageDay(date, true);
        }, 30000);
      }
    } catch (e) {
      panelUsage.innerHTML = `<div class="usage-bar"><button class="pill-btn" id="btn-usage-back">← 返回用量</button></div><div class="form-hint err">读取明细失败：${esc(e.message)}</div>`;
      $('btn-usage-back')?.addEventListener('click', closeUsageDay);
    }
  }

  function closeUsageDay() {
    usageDayDate = null;
    usageDayData = null;
    clearInterval(usageDayRefreshTimer);
    renderUsage();
  }

  function renderUsageDay() {
    const d = usageDayData;
    if (!d) return;
    const t = d.totals || {};
    const calls = d.calls || [];
    const rows = calls.map((c) => {
      const cwTxt = c.cw > 0 ? `+${fmtTok(c.cw)}w` : '';
      return `<tr>
        <td>${hm(c.at)}</td>
        <td class="c-conv" title="${esc(c.convTitle)}">${esc(c.convTitle)}</td>
        <td class="c-mod" title="${esc(c.model)}">${esc(c.model)}</td>
        <td class="c-cost">${fmtUsd(c.usd)}</td>
        <td>${fmtTok(c.in)}/${fmtTok(c.out)}</td>
        <td class="c-hit">${fmtTok(c.cr)}${cwTxt} (${c.hit != null ? Math.round(c.hit * 100) + '%' : '—'})</td>
        <td>${c.tps ? Math.round(c.tps) + '/s' : '—'}${c.ttftMs ? ' · ' + (c.ttftMs / 1000).toFixed(1) + 's' : ''}</td>
      </tr>`;
    }).join('');

    panelUsage.innerHTML = `
      <div class="usage-bar">
        <button class="pill-btn" id="btn-usage-back">← 返回用量</button>
        <span class="sec-title">${esc(d.date)} 调用明细</span>
        <span class="sec-value">${d.totalCalls || 0} 次 · ${fmtUsd(t.usd)}</span>
      </div>
      <div class="kpi-grid">
        <div class="kpi-card"><div class="kpi-label">当日花费</div><div class="kpi-val" style="color:var(--k-cost)">${fmtUsd(t.usd)}</div></div>
        <div class="kpi-card"><div class="kpi-label">调用次数</div><div class="kpi-val" style="color:var(--k-model)">${d.totalCalls || 0}</div></div>
        <div class="kpi-card"><div class="kpi-label">Token 总数</div><div class="kpi-val" style="color:var(--k-tokens)">${fmtTok(t.tokens)}</div></div>
        <div class="kpi-card"><div class="kpi-label">缓存命中</div><div class="kpi-val" style="color:var(--k-cache)">${pct(t.hit, 0)}</div></div>
      </div>
      <div class="u-table-wrap calls">
        <table class="u-table">
          <thead><tr><th>时间</th><th>对话</th><th>模型</th><th>花费</th><th>入/出</th><th>缓存</th><th>速度/首字</th></tr></thead>
          <tbody>${rows || '<tr><td colspan="7" style="text-align:center;color:var(--fg-muted);padding:16px">当天没有调用记录</td></tr>'}</tbody>
        </table>
      </div>
      ${d.truncated ? `<div class="form-hint" style="margin-top:6px">仅展示最新 ${d.shown} 条，顶部合计按全部 ${d.totalCalls} 次调用计算。</div>` : ''}
    `;
    $('btn-usage-back').addEventListener('click', closeUsageDay);
  }

  function renderUsage() {
    if (usageDayDate && usageDayData) { renderUsageDay(); return; }
    const u = usageData;
    if (!u) { loadUsage(); return; }
    const td = u.today || {};
    const days = u.days || [];
    const legend = u.legend || [];
    const totals = u.totals || {};
    const maxUsd = Math.max(0.01, ...days.map((d) => Number(d.usd) || 0));
    const peakDate = days.reduce((best, d) => ((d.usd || 0) > (best?.usd || 0) ? d : best), null)?.date;
    const avgPct = totals.avgUsd > 0 ? Math.min(100, (totals.avgUsd / maxUsd) * 100) : 0;

    const acct = u.account;
    const acctTxt = acct ? `${acct.masked}${acct.tier ? ' · ' + acct.tier.replace(/^Google AI\s*/i, '') : ''}` : '每日用量统计';

    const barsHtml = days.map((d, idx) => {
      const showVal = (usageSpan <= 7 && d.usd > 0) || (usageSpan > 7 && d.usd > 0 && (d.isToday || d.date === peakDate));
      const parts = (d.models || []).slice().reverse().map((m, mi, arr) => {
        const h = Math.max(2, (m.usd / maxUsd) * 100);
        const col = modelColor(m.name, legend);
        const isTop = mi === arr.length - 1;
        return `<div class="chart-part${isTop ? ' top' : ''}" data-day-idx="${idx}" data-model="${esc(m.name)}" style="height:${h.toFixed(2)}%;--mc:${col}"></div>`;
      }).join('');
      const valTag = showVal ? `<div class="chart-val">${fmtUsdShort(d.usd)}</div>` : '';
      return `<div class="chart-col" data-day-idx="${idx}" data-date="${esc(d.date)}">${parts}${valTag}</div>`;
    }).join('');

    const labelsHtml = days.map((d, idx) => {
      const shortD = d.date.slice(5);
      const show = usageSpan <= 7 || d.isToday || idx === 0 || idx % 5 === 0;
      return `<span class="${d.isToday ? 'today' : ''}">${show ? (d.isToday ? '今天' : shortD) : ''}</span>`;
    }).join('');

    const legendHtml = legend.map((m) => {
      const col = modelColor(m.name, legend);
      return `<span class="legend-item" data-legend="${esc(m.name)}" style="--mc:${col}"><i></i><span>${esc(m.name)}</span><span class="v">${fmtUsdShort(m.usd)}</span></span>`;
    }).join('');

    const tableRows = days.slice().reverse().map((d) => {
      const qg = d.qGemini != null ? `G +${(d.qGemini * 100).toFixed(1)}%` : '';
      const qc = d.qClaude != null ? `C +${(d.qClaude * 100).toFixed(1)}%` : '';
      const qTxt = [qg, qc].filter(Boolean).join(' ') || '—';
      return `<tr class="clickable${d.isToday ? ' today' : ''}" data-date="${esc(d.date)}">
        <td>${esc(d.date.slice(5))}${d.bf ? '<span class="bf-tag" title="从对话库补算">补</span>' : ''}</td>
        <td class="c-cost">${fmtUsd(d.usd)}</td>
        <td>${d.calls || 0}</td>
        <td>${fmtTok(d.tokens)}</td>
        <td class="c-hit">${d.hit != null ? Math.round(d.hit * 100) + '%' : '—'}</td>
        <td class="c-q">${qTxt}</td>
      </tr>`;
    }).join('');

    const forecastsHtml = (u.forecasts || []).map((f) => {
      const capParts = [];
      if (f.capUsd) capParts.push(`$${Number(f.capUsd).toLocaleString('en-US')}`);
      if (f.capTokens) capParts.push(`${fmtTok(f.capTokens)} tok`);
      const capStr = capParts.length ? `cap ≈ ${capParts.join(' / ')} (rough)` : 'cap 样本收集中';
      const use24 = f.proj24h != null;
      const proj = use24 ? f.proj24h : f.projWeek;
      const fullAt = use24 ? f.fullAt24h : f.fullAtWeek;
      const estStr = fullAt
        ? `预计耗尽 ${mdHm(fullAt)}`
        : proj != null
          ? `重置时约 ${Math.round(proj * 100)}%`
          : '—';
      const basis = use24 ? `按近 ${Math.round(f.spanH)}h 速度` : `按本周均速（已过 ${Math.round(f.elapsedH)}h）`;
      return `<div class="forecast-card" title="本周期本机已计 ${fmtUsd(f.cycleUsd)} / ${fmtTok(f.cycleTokens)} tok；多设备或家庭组共享时估值会有偏差">
        <div class="forecast-head">
          <span>${esc(f.label)} 每周额度 · ${Math.round(f.used * 100)}% used</span>
          <span>${estStr}</span>
        </div>
        <div class="forecast-meta">${capStr} · 重置 ${mdHm(f.resetMs)} · ${basis}</div>
      </div>`;
    }).join('');

    panelUsage.innerHTML = `
      <div class="usage-bar">
        <span class="acct-badge" title="${esc(acctTxt)}">${esc(acctTxt)}</span>
        <div class="btn-group">
          <button class="pill-btn ${usageSpan === 7 ? 'on' : ''}" data-span="7">7天</button>
          <button class="pill-btn ${usageSpan === 30 ? 'on' : ''}" data-span="30">30天</button>
          <button class="pill-btn" id="btn-usage-refresh" title="刷新">↻</button>
        </div>
      </div>

      <div class="kpi-grid">
        <div class="kpi-card"><div class="kpi-label">今日花费</div><div class="kpi-val" style="color:var(--k-cost)">${fmtUsd(td.usd)}</div></div>
        <div class="kpi-card"><div class="kpi-label">今日调用</div><div class="kpi-val" style="color:var(--k-model)">${td.calls || 0}</div></div>
        <div class="kpi-card"><div class="kpi-label">今日 Token</div><div class="kpi-val" style="color:var(--k-tokens)">${fmtTok(td.tokens)}</div></div>
        <div class="kpi-card"><div class="kpi-label">缓存命中</div><div class="kpi-val" style="color:var(--k-cache)">${pct(td.hit, 0)}</div></div>
      </div>

      <div class="chart-wrap">
        <div class="chart-area">
          <div class="chart-bars" id="usage-bars">
            ${barsHtml}
            ${avgPct > 0 ? `<div class="chart-avg" style="bottom:${avgPct.toFixed(1)}%" title="活跃日均 ${fmtUsd(totals.avgUsd)}"></div>` : ''}
          </div>
        </div>
        <div class="chart-labels">${labelsHtml}</div>
      </div>
      <div class="chart-legend">${legendHtml || '<span class="form-hint">暂无模型花费数据</span>'}</div>

      <div class="u-table-wrap">
        <table class="u-table">
          <thead><tr><th>日期</th><th>花费</th><th>调用</th><th>Token</th><th>命中</th><th>周额度 Δ</th></tr></thead>
          <tbody>${tableRows}</tbody>
        </table>
      </div>
      <div class="form-hint" style="margin-top:5px">点击任意日期查看当天逐次调用明细 · 近 ${u.span} 天合计 ${fmtUsd(totals.usd)}（日均 ${fmtUsd(totals.avgUsd)}）· 价格来源：${esc(u.priceSource || 'bundled')}</div>

      ${forecastsHtml ? `<div class="forecast-list">${forecastsHtml}</div>` : ''}
    `;

    // Bind span buttons & refresh
    panelUsage.querySelectorAll('[data-span]').forEach((b) => {
      b.addEventListener('click', () => {
        usageSpan = Number(b.dataset.span);
        usageData = null;
        loadUsage();
      });
    });
    $('btn-usage-refresh')?.addEventListener('click', () => loadUsage(false));

    // Bind chart interactions
    const barsEl = $('usage-bars');
    panelUsage.querySelectorAll('.chart-col').forEach((col) => {
      col.addEventListener('click', () => openUsageDay(col.dataset.date));
      col.addEventListener('mousemove', (ev) => {
        const d = days[Number(col.dataset.dayIdx)];
        if (!d) return;
        const part = ev.target.closest('.chart-part');
        if (part) {
          const m = (d.models || []).find((x) => x.name === part.dataset.model);
          if (m) {
            const share = d.usd > 0 ? Math.round((m.usd / d.usd) * 100) : 0;
            showHoverCard(
              `<div class="hc-title">${esc(d.date)} · ${esc(m.name)}</div>` +
              `<div class="hc-row"><i style="--mc:${modelColor(m.name, legend)}"></i><span>${fmtUsd(m.usd)}</span><span class="val dim">占 ${share}%</span></div>` +
              `<div class="dim">${m.calls} 次 · ${fmtTok(m.tokens)} tok · 命中 ${pct(m.hit, 0)}</div>`,
              ev
            );
            return;
          }
        }
        const rows = (d.models || []).slice(0, 5).map((m) =>
          `<div class="hc-row"><i style="--mc:${modelColor(m.name, legend)}"></i><span>${esc(m.name)}</span><span class="val">${fmtUsd(m.usd)}</span></div>`
        ).join('');
        showHoverCard(
          `<div class="hc-title">${esc(d.date)}${d.bf ? '（补算）' : ''} · ${fmtUsd(d.usd)}</div>` +
          (rows || '<div class="dim">无调用</div>') +
          `<div class="dim" style="margin-top:2px">${d.calls || 0} 次 · ${fmtTok(d.tokens)} tok · 点击看明细</div>`,
          ev
        );
      });
      col.addEventListener('mouseleave', hideHoverCard);
    });

    // Legend hover highlights matching model across all days
    panelUsage.querySelectorAll('.legend-item').forEach((item) => {
      item.addEventListener('mouseenter', () => {
        const name = item.dataset.legend;
        barsEl?.classList.add('focus');
        barsEl?.querySelectorAll('.chart-part').forEach((p) => p.classList.toggle('hl', p.dataset.model === name));
      });
      item.addEventListener('mouseleave', () => {
        barsEl?.classList.remove('focus');
        barsEl?.querySelectorAll('.chart-part.hl').forEach((p) => p.classList.remove('hl'));
      });
    });

    // Table row hover highlights corresponding bar; click drills down into per-day calls
    panelUsage.querySelectorAll('.u-table tbody tr[data-date]').forEach((tr) => {
      const date = tr.dataset.date;
      const barCol = panelUsage.querySelector(`.chart-col[data-date="${date}"]`);
      tr.addEventListener('mouseenter', () => barCol?.classList.add('hl-row'));
      tr.addEventListener('mouseleave', () => barCol?.classList.remove('hl-row'));
      tr.addEventListener('click', () => openUsageDay(date));
    });
  }

  // ---------------------------------------------------------------- Theme tab (Option B)
  const DEFAULT_LOOK = { palette: 'cometix', skin: 'plain', bars: 'thin', density: 'compact', icons: 'solid' };

  function renderTheme(p, force = false) {
    if (!window.__agyBar) return;
    if (!force && panelTheme.childElementCount) return;
    const c = activeConfig(p);
    const look = { ...DEFAULT_LOOK, ...(c.bar_look || {}) };
    const pals = window.__agyBar.PALETTES || [];

    const rowHtml = (label, key, opts) => `
      <div class="look-row">
        <span class="look-row-label">${label}</span>
        <div class="btn-group">
          ${opts.map(([v, txt]) => `<button type="button" class="pill-btn ${look[key] === v ? 'on' : ''}" data-look-key="${key}" data-look-val="${v}">${txt}</button>`).join('')}
        </div>
      </div>`;

    panelTheme.innerHTML = `
      <div class="section">
        <div class="sec-head">
          <span class="sec-title">配色画廊（实时副本）</span>
          <span id="theme-save-state" class="save-state"></span>
          <button type="button" id="btn-look-reset" class="pill-btn">恢复默认</button>
        </div>
        <div class="theme-gallery">
          ${pals.map((pal) => `
            <button type="button" class="theme-card ${look.palette === pal.id ? 'on' : ''}" data-pal-id="${esc(pal.id)}">
              <div class="theme-card-head">
                <span class="theme-card-name">${esc(pal.name)}</span>
                <span class="theme-card-tag">${look.palette === pal.id ? '当前使用' : esc(pal.desc)}</span>
              </div>
              <div class="theme-card-preview" data-preview-pal="${esc(pal.id)}"></div>
            </button>`).join('')}
        </div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">外形与细节</span></div>
        <div class="look-controls">
          ${rowHtml('外形样式', 'skin', [['plain', 'Plain'], ['pill', 'Pill'], ['powerline', 'Powerline']])}
          ${rowHtml('额度进度条', 'bars', [['thin', '细条'], ['cells', '10格'], ['ring', '圆环'], ['none', '隐藏']])}
          ${rowHtml('信息密度', 'density', [['compact', '紧凑'], ['full', '完整']])}
          ${rowHtml('图标样式', 'icons', [['solid', '实心'], ['outline', '线条'], ['unicode', '字符'], ['off', '关闭']])}
        </div>
        <div class="form-hint" style="margin-top:8px">所有配色均含浅色与深色两版，随 Antigravity 明暗主题自动切换；点选后自动保存并同步到所有窗口。</div>
      </div>
    `;

    // Render live mini statusbar into each of the 8 gallery cards
    const barPayload = p.bar ? {
      ...p.bar,
      opts: { ...(p.bar.opts || {}), enabled: true, seg: c.bar_segments || {} },
    } : null;
    const dark = isDarkTheme();
    if (barPayload) {
      panelTheme.querySelectorAll('[data-preview-pal]').forEach((box) => {
        const palId = box.dataset.previewPal;
        window.__agyBar.renderInto(box, barPayload, p.id, dark, { ...look, palette: palId }, { compactCard: true });
      });
    }

    const updateLook = (patch) => {
      const cur = { ...DEFAULT_LOOK, ...(activeConfig(last).bar_look || {}) };
      scheduleSave({ bar_look: { ...cur, ...patch } });
    };
    panelTheme.querySelectorAll('[data-pal-id]').forEach((btn) => {
      btn.addEventListener('click', () => updateLook({ palette: btn.dataset.palId }));
    });
    panelTheme.querySelectorAll('[data-look-key]').forEach((btn) => {
      btn.addEventListener('click', () => updateLook({ [btn.dataset.lookKey]: btn.dataset.lookVal }));
    });
    $('btn-look-reset')?.addEventListener('click', () => scheduleSave({ bar_look: { ...DEFAULT_LOOK } }));
  }

  // ---------------------------------------------------------------- Settings tab
  const SEGMENTS = [
    ['model', '模型'], ['git', 'Git'], ['context', '上下文'], ['cache', '缓存命中'],
    ['cost', '对话花费'], ['today', '今日花费'], ['ttft', '首字延迟'], ['speed', '输出速度'],
    ['quota_5h', '5 小时额度'], ['quota_7d', '7 天额度'], ['running', '运行状态'], ['subagents', '子代理行'],
  ];

  function readSettingsForm() {
    const seg = {};
    for (const [k] of SEGMENTS) seg[k] = $('seg-' + k).checked;
    return {
      bar_enabled: $('cfg-bar-on').checked,
      bar_segments: seg,
      ui_zh: $('cfg-zh').checked,
      relaunch_mode: $('cfg-relaunch-mode').value,
      online_updates: $('cfg-online-upd').checked,
      online_data_sync: $('cfg-data-sync').checked,
      context_limit_override: Number($('cfg-ctx-win').value) || 0,
      cache_ttl_sec: Math.max(30, Number($('cfg-ttl').value) || 300),
      include_subagents: $('cfg-sub').checked,
    };
  }

  function onSettingsChange() {
    const patch = readSettingsForm();
    panelSettings.querySelector('.seg-grid')?.classList.toggle('off', !patch.bar_enabled);
    scheduleSave(patch);
  }

  function renderSettings(p, force = false) {
    if (!force && panelSettings.childElementCount) {
      updateLiveStatus();
      renderUpdateCenter();
      return;
    }
    const c = activeConfig(p);
    const sg = c.bar_segments || {};
    const opt = (v, label) => `<option value="${v}" ${c.context_limit_override === v ? 'selected' : ''}>${label}</option>`;
    const rOpt = (v, label) => `<option value="${v}" ${(c.relaunch_mode || 'never') === v ? 'selected' : ''}>${label}</option>`;
    const segBox = ([k, label]) =>
      `<label class="seg-check"><input id="seg-${k}" type="checkbox" ${sg[k] !== false ? 'checked' : ''} /><span>${label}</span></label>`;

    panelSettings.innerHTML = `
      <div class="section">
        <div class="sec-head"><span class="sec-title">输入框状态栏</span><span id="save-state" class="save-state"></span></div>
        <div class="form-row">
          <div class="form-label"><span>显示在输入框上方</span><span class="form-hint">改动立刻反映在顶部预览并自动保存，配色与外形在 Theme 页设置</span></div>
          <input id="cfg-bar-on" type="checkbox" ${c.bar_enabled !== false ? 'checked' : ''} />
        </div>
        <div class="seg-grid ${c.bar_enabled !== false ? '' : 'off'}">${SEGMENTS.map(segBox).join('')}</div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">中文界面与调试端口</span></div>
        <div class="form-row">
          <div class="form-label"><span>中文界面</span><span class="form-hint">把 Antigravity 界面文字译为中文（不动对话和代码）；打开或关闭均立刻生效，无需刷新窗口。词典来自 agy-zh-pack</span></div>
          <input id="cfg-zh" type="checkbox" ${c.ui_zh ? 'checked' : ''} />
        </div>
        <div class="form-row">
          <div class="form-label"><span>未带调试端口启动时</span><span class="form-hint">先核验主进程确实未带 --remote-debugging-port 再触发，防止端口繁忙时误重启</span></div>
          <select id="cfg-relaunch-mode" class="field-select">
            ${rOpt('never', '不处理（默认）')}${rOpt('ask', '弹框询问')}${rOpt('auto', '自动重开')}
          </select>
        </div>
        <div class="form-hint" id="live-status" style="margin:8px 0;line-height:1.6">…</div>
        <div class="restart-row">
          <button id="btn-restart" type="button" class="secondary-btn">以调试模式重启 Antigravity</button>
          <span id="restart-hint" class="form-hint"></span>
        </div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">更新中心</span></div>
        <div class="form-row">
          <div class="form-label"><span>自动检查插件新版本</span><span class="form-hint">每 6 小时检查 GitHub Releases，发现新版后可在下方一键校验并更新</span></div>
          <input id="cfg-online-upd" type="checkbox" ${c.online_updates !== false ? 'checked' : ''} />
        </div>
        <div id="plugin-update-box" class="update-box">加载版本信息中…</div>

        <div class="form-row" style="margin-top:8px">
          <div class="form-label"><span>在线同步词典与价格表</span><span class="form-hint">每 6 小时按 commit 同步 agy-zh-pack 最新中文词典与官方模型单价</span></div>
          <input id="cfg-data-sync" type="checkbox" ${c.online_data_sync !== false ? 'checked' : ''} />
        </div>
        <div id="data-sync-box" class="update-box">加载同步状态中…</div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">计算口径 (config.toml)</span></div>
        <div class="form-row">
          <div class="form-label"><span>上下文上限</span><span class="form-hint">默认用后台报告的上限（maxContextTokens）</span></div>
          <select id="cfg-ctx-win" class="field-select">
            ${opt(0, '后台报告的上限')}${opt(200000, '200K')}${opt(256000, '256K')}${opt(512000, '512K')}${opt(1000000, '1M')}
          </select>
        </div>
        <div class="form-row">
          <div class="form-label"><span>Prompt 缓存 TTL（秒）</span><span class="form-hint">Overview 里“缓存热度”的倒计时</span></div>
          <input id="cfg-ttl" type="number" step="30" min="30" class="field-input" value="${c.cache_ttl_sec || 300}" />
        </div>
        <div class="form-row">
          <div class="form-label"><span>总计包含子代理</span><span class="form-hint">状态栏与面板的对话总计是否计入子代理</span></div>
          <input id="cfg-sub" type="checkbox" ${c.include_subagents !== false ? 'checked' : ''} />
        </div>
        <div class="form-hint" style="margin-top:6px">模型单价（含 Claude 缓存写入与分时调价）可在 config.toml 的 [[models]] 中自定义。</div>
      </div>
    `;

    panelSettings.querySelectorAll('input[type=checkbox], select').forEach((el) => el.addEventListener('change', onSettingsChange));
    $('cfg-ttl').addEventListener('change', onSettingsChange);
    $('btn-restart').addEventListener('click', onRestart);
    updateLiveStatus();
    renderUpdateCenter();
    if (!updateStatus) refreshUpdateStatus();
  }

  // ---------------------------------------------------------------- Update Center logic
  async function refreshUpdateStatus() {
    try {
      const res = await fetch('/api/update/status', { cache: 'no-store' });
      if (!res.ok) return;
      updateStatus = await res.json();
      updateDot?.classList.toggle('hidden', !updateStatus.available);
      if (activeTab === 'settings') renderUpdateCenter();
      if (updateStatus.installing && !updatePollTimer) {
        updatePollTimer = setInterval(async () => {
          await refreshUpdateStatus();
          if (!updateStatus?.installing) {
            clearInterval(updatePollTimer);
            updatePollTimer = 0;
          }
        }, 1200);
      }
    } catch (e) {}
  }

  function renderUpdateCenter() {
    const pBox = $('plugin-update-box');
    const dBox = $('data-sync-box');
    if (!pBox || !dBox) return;
    if (!updateStatus) {
      pBox.textContent = '加载版本信息中…';
      dBox.textContent = '加载同步状态中…';
      return;
    }
    const s = updateStatus;
    const rel = s.release || {};
    const inst = s.install || {};
    const ds = s.dataSync || {};

    let verBadge = `当前 <b>v${esc(s.current)}</b>`;
    if (s.available) {
      verBadge += ` · <span class="ok">发现新版本 v${esc(rel.latest)}</span>`;
    } else if (rel.latest) {
      verBadge += ` · 已是最新`;
    }

    let actionBtns = `<button type="button" id="btn-check-upd" class="secondary-btn" ${s.checking || s.installing ? 'disabled' : ''}>${s.checking ? '检查中…' : '检查更新'}</button>`;
    if (s.available && s.canSelfUpdate) {
      actionBtns += ` <button type="button" id="btn-install-upd" class="secondary-btn primary" ${s.installing ? 'disabled' : ''}>${s.installing ? '更新中…' : '一键更新到 v' + esc(rel.latest)}</button>`;
    }

    let statusLine = '';
    if (inst.status && inst.status !== 'idle') {
      const map = {
        downloading: '正在下载安装包…',
        verifying: '正在校验 SHA256SUMS…',
        installing: '正在备份并替换插件文件…',
        installed: `已安装 v${esc(inst.version)}，sidecar 将自动重启生效`,
        failed: `更新失败：${esc(inst.error || '未知错误')}`,
      };
      statusLine = `<div class="${inst.status === 'failed' ? 'err' : 'ok'}">${map[inst.status] || esc(inst.status)}</div>`;
    }
    if (s.available && s.devCheckout) {
      statusLine += `<div class="bad">当前运行在开发仓库目录（含 .git），请用 git pull &amp;&amp; cargo build 更新</div>`;
    }
    if (rel.error) {
      statusLine += `<div class="err">检查失败：${esc(rel.error)}</div>`;
    }
    const relLink = rel.html_url ? ` · <a href="${esc(rel.html_url)}" target="_blank" rel="noreferrer">查看 Release</a>` : '';
    pBox.innerHTML = `
      <div class="update-row">
        <span>${verBadge}</span>
        <span class="btn-group">${actionBtns}</span>
      </div>
      <div class="update-meta">
        ${statusLine}
        上次检查：${fmtAgo(rel.checked_at)}${relLink}
      </div>`;

    const dsLine = ds.synced_at
      ? `上次同步：${fmtAgo(ds.synced_at)}（${esc(ds.source_host)} @ ${esc(ds.commit)}）<br>词典 ${ds.dict_entries} 条 · 规则 ${ds.rule_entries} 条 · 价格表 ${esc(ds.prices_updated || '—')}（${ds.prices_models || 0} 个模型）`
      : '尚未在线同步（当前使用内置词典与价格表）';
    dBox.innerHTML = `
      <div class="update-row">
        <span>${s.onlineDataSync ? '在线数据同步已开启' : '在线数据同步已关闭'}</span>
        <button type="button" id="btn-sync-data" class="secondary-btn" ${s.syncingData ? 'disabled' : ''}>${s.syncingData ? '同步中…' : '立即同步'}</button>
      </div>
      <div class="update-meta">
        ${ds.error ? `<div class="err">同步失败：${esc(ds.error)}</div>` : ''}
        ${dsLine}
      </div>`;

    $('btn-check-upd')?.addEventListener('click', async () => {
      const btn = $('btn-check-upd');
      btn.disabled = true; btn.textContent = '检查中…';
      try {
        await postJson('/api/update/check', {});
      } catch (e) {}
      await refreshUpdateStatus();
    });
    $('btn-install-upd')?.addEventListener('click', async () => {
      const btn = $('btn-install-upd');
      btn.disabled = true; btn.textContent = '启动更新…';
      try {
        const res = await postJson('/api/update/install', {});
        const j = await res.json().catch(() => ({}));
        if (!res.ok) alert(j.error || '更新失败');
      } catch (e) {
        alert('更新失败：' + e.message);
      }
      await refreshUpdateStatus();
    });
    $('btn-sync-data')?.addEventListener('click', async () => {
      const btn = $('btn-sync-data');
      btn.disabled = true; btn.textContent = '同步中…';
      try {
        await postJson('/api/data-sync', {});
      } catch (e) {}
      await refreshUpdateStatus();
      lastJson = '';
      poll();
    });
  }

  // ---------------------------------------------------------------- debug relaunch button
  let restartArmed = 0;
  async function onRestart() {
    const btn = $('btn-restart');
    const hint = $('restart-hint');
    if (!restartArmed) {
      let running = 0;
      try { running = (await (await fetch('/api/live-status', { cache: 'no-store' })).json()).running || 0; } catch (e) {}
      btn.textContent = '确认重启';
      btn.classList.add('danger');
      hint.textContent = running ? `${running} 个对话正在运行，重启会中断它们` : '会关闭所有 Antigravity 窗口后带调试参数重新打开';
      restartArmed = setTimeout(() => {
        restartArmed = 0;
        btn.textContent = '以调试模式重启 Antigravity';
        btn.classList.remove('danger');
        hint.textContent = '';
      }, 6000);
      return;
    }
    clearTimeout(restartArmed);
    restartArmed = 0;
    btn.disabled = true;
    try {
      const res = await postJson('/api/restart-debug', {});
      const j = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(j.error || 'HTTP ' + res.status);
      hint.textContent = '正在退出 Antigravity，稍后自动重新打开…';
    } catch (e) {
      btn.disabled = false;
      btn.textContent = '以调试模式重启 Antigravity';
      btn.classList.remove('danger');
      hint.textContent = '重启失败：' + e.message;
    }
  }

  async function updateLiveStatus() {
    const box = document.getElementById('live-status');
    if (!box) return;
    try {
      const j = await (await fetch('/api/live-status', { cache: 'no-store' })).json();
      const state = j.debug ? '<span class="ok">调试端口已连接，状态栏和中文界面可用</span>'
        : '<span class="bad">没连上调试端口：状态栏和中文界面需要 Antigravity 以调试模式启动</span>';
      const flag = j.relaunch?.lastFlag ? ` · 进程校验: ${esc(j.relaunch.lastFlag)}` : '';
      box.innerHTML = state + `<br>CDP: ${esc(j.cdp || '—')}${flag}<br>language_server: ${esc(j.ls || '—')}` +
        (j.panes && j.panes.length ? `<br>showing: ${j.panes.map((x) => esc(String(x).slice(0, 8))).join(', ')}` : '');
    } catch (e) {
      box.textContent = 'live status unavailable';
    }
  }

  // ---------------------------------------------------------------- main poll loop
  function renderPanels(p, force = false) {
    turnsCountEl.textContent = p.turns?.length ? String(p.turns.length) : '';
    if (activeTab === 'overview') renderOverview(p);
    else if (activeTab === 'turns') renderTurns(p);
    else if (activeTab === 'usage') { if (!usageData) loadUsage(); else if (force) renderUsage(); }
    else if (activeTab === 'theme') renderTheme(p, force);
    else if (activeTab === 'settings') renderSettings(p, force);
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
  refreshUpdateStatus();
  setInterval(poll, 1500);
  setInterval(refreshUpdateStatus, 60000);
  setInterval(() => { if (activeTab === 'usage' && !usageDayDate) loadUsage(true); }, 30000);
})();
