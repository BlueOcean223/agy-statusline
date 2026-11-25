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
  let usageSpan = 7; // 7 | 30 | 'all'
  let usageData = null;
  let usageLoading = false;
  let usageDayDate = null;
  let usageDayData = null;
  let usageHoverCard = null;
  let usageDayRefreshTimer = 0;
  let dayView = 'calls'; // 'calls' | 'conv'
  let daySrc = 'all'; // 'all' | 'desktop' | 'cli'
  let dayModel = '';
  let dayConv = '';
  let dayOnlyMiss = false;
  let dayLimit = 600;

  // Checkpoint viewer state (persists across 1.5s polls while open)
  let ckptOpenConvId = null;
  let ckptData = null;
  let ckptLoading = false;
  let ckptReqSeq = 0;

  // Live status cache (for temporary English toggle button state)
  let liveStatus = null;

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
    if (ckptOpenConvId && ckptOpenConvId !== selectedConvId) {
      ckptReqSeq++;
      ckptOpenConvId = null;
      ckptData = null;
      ckptLoading = false;
    }
    lastJson = '';
    poll();
  });

  // ---------------------------------------------------------------- helpers & formatting
  const postJson = (path, body) => {
    const f = window.sidecar?.fetch ? window.sidecar.fetch.bind(window.sidecar) : fetch;
    return f(path, { method: 'POST', body: JSON.stringify(body || {}) });
  };
  const secureGet = (path) => {
    const f = window.sidecar?.fetch ? window.sidecar.fetch.bind(window.sidecar) : fetch;
    return f(path, { method: 'GET', cache: 'no-store' });
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

  const MISS_CAUSE = {
    user: ['your pause', '你隔了 1 分钟以上才发下一条消息'],
    turn: ['agent pause', '一轮中间停了 1 分钟以上（长命令、等你批准、等子代理）'],
    wait: ['pause', '隔了 1 分钟以上才发下一次请求（agy CLI）'],
    compact: ['compaction', '上下文刚被压缩或截短'],
    model: ['model switch', '换了模型（新模型的缓存是空的）'],
    other: ['other', '间隔不到 1 分钟也没命中（服务端缓存未接上或前缀变化）'],
  };
  const MISS_ORDER = ['user', 'turn', 'wait', 'compact', 'model', 'other'];
  function fmtIdle(ms) {
    const n = Math.max(0, Number(ms) || 0);
    if (n < 60e3) return Math.round(n / 1000) + ' 秒';
    if (n < 3600e3) return Math.round(n / 60e3) + ' 分钟';
    return (n / 3600e3).toFixed(1) + ' 小时';
  }
  function missNote(m) {
    if (!m) return '';
    const why = m.cause === 'user' ? `你隔了 ${fmtIdle(m.idleMs)} 才发下一条消息`
      : m.cause === 'turn' ? `一轮中间停了 ${fmtIdle(m.idleMs)}（长命令、等你批准、等子代理）`
        : m.cause === 'wait' ? `隔了 ${fmtIdle(m.idleMs)} 才发下一次请求（agy CLI）`
          : m.cause === 'model' ? `换了模型（上一大调用：${m.fromModel || '其他模型'}）`
            : m.cause === 'other' ? `离上一次调用结束仅隔 ${fmtIdle(m.idleMs)} 也未命中（服务端缓存未接上或前缀变化）`
              : (MISS_CAUSE[m.cause]?.[1] || m.cause);
    return `缓存未命中：约 ${Number(m.tok || 0).toLocaleString()} token 未走缓存，多花约 ${fmtUsd(m.usd)}\n原因：${why}`;
  }
  function formatResetTip(resets) {
    if (!Array.isArray(resets) || !resets.length) return '';
    return resets.map((r) =>
      `↺ 提前重置（${r.name || r.bucketId}）：${mdHm(r.atMs)} 重置前已用 ${(r.fromUsed * 100).toFixed(1)}%，原定 ${mdHm(r.wasResetMs)} 重置`
    ).join('\n');
  }
  const md = (ms) => { const d = new Date(ms); return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`; };

  async function toggleCheckpointViewer(convId) {
    if (ckptOpenConvId === convId) {
      ckptReqSeq++;
      ckptOpenConvId = null;
      ckptData = null;
      ckptLoading = false;
      if (last) renderOverview(last);
      return;
    }
    const seq = ++ckptReqSeq;
    ckptOpenConvId = convId;
    ckptData = null;
    ckptLoading = true;
    if (last) renderOverview(last);
    try {
      const res = await secureGet(`/api/checkpoint?conversationId=${encodeURIComponent(convId)}`);
      const next = res.ok ? await res.json() : { err: 'HTTP ' + res.status };
      if (seq !== ckptReqSeq || ckptOpenConvId !== convId) return;
      ckptData = next;
    } catch (e) {
      if (seq !== ckptReqSeq || ckptOpenConvId !== convId) return;
      ckptData = { err: e.message };
    } finally {
      if (seq === ckptReqSeq && ckptOpenConvId === convId) {
        ckptLoading = false;
        if (last && activeTab === 'overview') renderOverview(last);
      }
    }
  }

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

    const ckptBtn = ctx.compactions > 0
      ? ` <button type="button" class="link-btn" id="btn-view-ckpt">${ckptOpenConvId === p.id ? '收起摘要' : '查看摘要'}</button>`
      : '';
    let ckptBoxHtml = '';
    if (ckptOpenConvId === p.id) {
      if (ckptLoading) {
        ckptBoxHtml = '<div class="ckpt-box"><div class="ckpt-meta">读取压缩摘要中…</div></div>';
      } else if (ckptData?.err) {
        ckptBoxHtml = `<div class="ckpt-box"><div class="ckpt-head"><span>上下文压缩查看器</span><button type="button" class="link-btn" id="btn-close-ckpt">×</button></div><div class="ckpt-meta">${esc(ckptData.err)}</div></div>`;
      } else if (ckptData) {
        const atStr = ckptData.at ? mdHm(ckptData.at) : '—';
        ckptBoxHtml = `
          <div class="ckpt-box">
            <div class="ckpt-head">
              <span>最近一次压缩摘要（共 ${ckptData.n} 次 · #${ckptData.step} · ${atStr}）</span>
              <button type="button" class="link-btn" id="btn-close-ckpt">×</button>
            </div>
            <div class="ckpt-meta">截断前用户请求 ${ckptData.requests} 条 · Artifacts ${ckptData.artifacts} 个 · ${esc(ckptData.log || '')}</div>
            <pre class="ckpt-summary">${esc(ckptData.summary || '（日志中未找到 <summary> 区块）')}</pre>
          </div>`;
      }
    }

    const ms = cache.missSummary || { count: cache.misses || 0, usd: 0, by: {} };
    const missPillsHtml = ms.count > 0
      ? `<div class="miss-pills">${MISS_ORDER.filter((k) => ms.by && ms.by[k]).map((k) => {
          const b = ms.by[k];
          const [shortLbl, desc] = MISS_CAUSE[k] || [k, k];
          return `<span class="miss-chip" title="${esc(desc)}：${b.count} 次，多花约 ${fmtUsd(b.usd)}">${esc(shortLbl)} ${b.count}× ${fmtUsd(b.usd)}</span>`;
        }).join('')}</div>`
      : '';
    const todayVal = t.todayCliCost > 0
      ? `${fmtUsd(t.todayCost)} <span style="color:var(--fg-muted);font-size:10.5px">(含 CLI ${fmtUsd(t.todayCliCost)})</span>`
      : fmtUsd(t.todayCost);

    panelOverview.innerHTML = `
      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Context Window</span>
          <span class="sec-value">${ctxPct.toFixed(1)}% · ${fmtTok(ctx.used)} / ${fmtTok(ctx.max)}</span>
        </div>
        <div class="meter"><div class="meter-fill" style="width:${Math.min(100, ctxPct).toFixed(1)}%;background:${ctxColor}"></div></div>
        <div class="kv-grid">
          ${kv('Remaining', fmtTok(ctx.left))}
          ${kv('Compactions', `${ctx.compactions}×${ckptBtn}`)}
          ${kv('Last Call Δ', (ctx.delta >= 0 ? '+' : '') + fmtTok(ctx.delta))}
          ${kv('Est. Calls Left', ctx.turnsLeft ? '~' + ctx.turnsLeft : '—')}
        </div>
        ${ctx.series.length >= 2 ? `<div class="spark-wrap"><span class="kv-k" style="font-size:11px">Trend</span>${sparkSvg(ctx.series, 'var(--k-context)')}</div>` : ''}
        ${ckptBoxHtml}
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Prompt Cache</span><span class="sec-value">${pct(cache.lastHit)} hit</span></div>
        <div class="meter"><div class="meter-fill" style="width:${((cache.lastHit || 0) * 100).toFixed(1)}%;background:var(--k-cache)"></div></div>
        <div class="kv-grid">
          ${kv('Session Avg Hit', pct(cache.sessionHit))}
          ${kv(`Warmth (${Math.round(cache.ttlSec / 60)}m TTL)`, cache.warmLeftSec ? fmtDur(cache.warmLeftSec) : 'cold')}
          ${kv('Cache Misses', ms.count > 0 ? `<span style="color:var(--k-warn)">${ms.count}× · +${fmtUsd(ms.usd)}</span>` : '0×')}
          ${kv('Saved vs. Uncached', fmtUsd(cache.savedUsd))}
        </div>
        ${missPillsHtml}
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
          ${kv('Today, All Convs', todayVal)}
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

    $('btn-view-ckpt')?.addEventListener('click', () => toggleCheckpointViewer(p.id));
    $('btn-close-ckpt')?.addEventListener('click', () => toggleCheckpointViewer(p.id));
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
    panelTurns.innerHTML = turns.slice().reverse().map((t) => {
      const m = t.miss;
      const missTag = m
        ? ` <span class="miss-chip" title="${esc(missNote(m))}">miss +${fmtUsd(m.usd)} · ${esc(MISS_CAUSE[m.cause]?.[0] || m.cause)}</span>`
        : '';
      const costCls = t.costly ? 'turn-costly' : '';
      const costTip = t.costly
        ? (m ? `高价调用提醒 · ${missNote(m)}` : '高价调用提醒：缓存已命中，主要来自长上下文或大量输出')
        : '';
      return `
      <div class="turn-row">
        <div class="turn-left">
          <span class="turn-step">#${t.step ?? '–'} · ${hms(t.at)} · ctx ${fmtTok(t.ctx)} (${Math.round(t.hit * 100)}% hit)${missTag}</span>
          <span class="turn-label">${esc(t.label)}</span>
        </div>
        <div class="turn-right">
          <div class="${costCls}"${costTip ? ` title="${esc(costTip)}"` : ''}>${t.costly ? '!' : ''}${fmtUsd(t.cost)} · +${fmtTok(t.out)} out</div>
          <div style="color:var(--fg-muted)">${t.tps ? Math.round(t.tps) + ' tok/s' : '—'}${t.ttftMs ? ' · ' + (t.ttftMs / 1000).toFixed(1) + 's TTFT' : ''}</div>
        </div>
      </div>`;
    }).join('');
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

  async function openUsageDay(date, silent = false, opts = null) {
    if (opts) {
      dayView = opts.view || 'calls';
      daySrc = opts.src || 'all';
      dayModel = opts.model || '';
      dayConv = opts.conv || '';
      dayOnlyMiss = !!opts.onlyMiss;
      dayLimit = opts.limit ?? 600;
    } else if (!silent && usageDayDate !== date) {
      dayView = 'calls';
      daySrc = 'all';
      dayModel = '';
      dayConv = '';
      dayOnlyMiss = false;
      dayLimit = 600;
    }
    usageDayDate = date;
    clearInterval(usageDayRefreshTimer);
    if (!silent) {
      panelUsage.innerHTML = `<div style="padding:20px 0;color:var(--fg-muted);font-size:12px">加载 ${esc(date)} 调用明细…</div>`;
    }
    try {
      const res = await fetch(`/api/usage/day?date=${encodeURIComponent(date)}&limit=${dayLimit}`, { cache: 'no-store' });
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
    const ms = d.missSummary || { count: 0, usd: 0, by: {} };
    const allCalls = d.calls || [];
    const byConv = d.byConv || [];
    const models = d.models || [];
    const legendRef = usageData?.legend || models;

    const filteredCalls = allCalls.filter((c) => {
      if (daySrc !== 'all' && c.source !== daySrc) return false;
      if (dayModel && c.model !== dayModel) return false;
      if (dayConv && c.convId !== dayConv) return false;
      if (dayOnlyMiss && !c.miss) return false;
      return true;
    });

    const filteredConvs = byConv.filter((c) => {
      if (daySrc !== 'all' && c.source !== daySrc) return false;
      if (dayModel && !(c.models || []).includes(dayModel)) return false;
      if (dayOnlyMiss && !(c.missCount > 0)) return false;
      return true;
    });

    const convTitle = dayConv
      ? (byConv.find((x) => x.convId === dayConv)?.convTitle || dayConv.slice(0, 8))
      : '';

    const srcBtns = d.cliCalls > 0
      ? `<div class="btn-group">
          <button class="pill-btn ${daySrc === 'all' ? 'on' : ''}" data-dsrc="all">全部</button>
          <button class="pill-btn ${daySrc === 'desktop' ? 'on' : ''}" data-dsrc="desktop">桌面端</button>
          <button class="pill-btn ${daySrc === 'cli' ? 'on' : ''}" data-dsrc="cli">CLI (${d.cliCalls})</button>
        </div>`
      : '';

    const modelChips = models.length >= 2
      ? `<div class="btn-group" style="flex-wrap:wrap">${models.slice(0, 6).map((m) => {
          const col = modelColor(m.name, legendRef);
          const on = dayModel === m.name;
          return `<button class="pill-btn ${on ? 'on' : ''}" data-dmod="${esc(m.name)}"><i class="model-dot" style="--mc:${col}"></i>${esc(m.name)}</button>`;
        }).join('')}</div>`
      : '';

    const missToggle = ms.count > 0
      ? `<button class="pill-btn ${dayOnlyMiss ? 'on' : ''}" id="btn-day-miss" title="只看缓存未命中的调用">仅未命中 (${ms.count})</button>`
      : '';

    const convChip = dayConv
      ? `<button class="pill-btn on" id="btn-clear-conv" title="取消对话筛选">对话: ${esc(convTitle.slice(0, 18))} ×</button>`
      : '';

    let tableHtml = '';
    if (dayView === 'conv') {
      const convRows = filteredConvs.map((c) => {
        const cliTag = c.source === 'cli' ? '<span class="cli-tag">CLI</span>' : '';
        const spanTxt = c.firstAt === c.lastAt ? hm(c.firstAt) : `${hm(c.firstAt)}–${hm(c.lastAt)}`;
        const missCell = c.missCount > 0
          ? `<td class="c-miss-usd" title="缓存未命中 ${c.missCount} 次，多花约 ${fmtUsd(c.missUsd)}">+${fmtUsd(c.missUsd)} (${c.missCount})</td>`
          : `<td style="color:var(--fg-faint)">—</td>`;
        return `<tr class="clickable" data-pick-conv="${esc(c.convId)}">
          <td class="c-conv" title="${esc(c.convTitle)}">${cliTag}${esc(c.convTitle)}</td>
          <td>${c.calls}</td>
          <td class="c-cost">${fmtUsd(c.usd)}</td>
          <td>${Math.round((c.share || 0) * 100)}%</td>
          <td>${fmtTok(c.tokens)}</td>
          <td class="c-hit">${c.hit != null ? Math.round(c.hit * 100) + '%' : '—'}</td>
          ${missCell}
          <td>${spanTxt}</td>
          <td class="c-mod" title="${esc((c.models || []).join(', '))}">${esc((c.models || []).slice(0, 2).join(', '))}</td>
        </tr>`;
      }).join('');
      tableHtml = `
        <table class="u-table">
          <thead><tr><th>对话</th><th>次数</th><th>花费</th><th>占比</th><th>Token</th><th>命中</th><th>Miss</th><th>时段</th><th>模型</th></tr></thead>
          <tbody>${convRows || '<tr><td colspan="9" style="text-align:center;color:var(--fg-muted);padding:16px">没有符合筛选条件的对话</td></tr>'}</tbody>
        </table>`;
    } else {
      const rows = filteredCalls.map((c) => {
        const cwTxt = c.cw > 0 ? `+${fmtTok(c.cw)}w` : '';
        const cliTag = c.source === 'cli' ? '<span class="cli-tag">CLI</span>' : '';
        const missTip = c.miss ? missNote(c.miss) : '';
        const costTip = c.costly
          ? (c.miss ? `高价调用 · ${missTip}` : '高价调用：缓存已命中，主要来自长上下文或大量输出')
          : missTip;
        return `<tr>
          <td>${hm(c.at)}</td>
          <td class="c-conv" title="${esc(c.convTitle)}">${cliTag}${esc(c.convTitle)}</td>
          <td class="c-mod" title="${esc(c.model)}">${esc(c.model)}</td>
          <td class="c-cost${c.costly ? ' c-costly' : ''}"${costTip ? ` title="${esc(costTip)}"` : ''}>${c.costly ? '!' : ''}${fmtUsd(c.usd)}</td>
          <td>${fmtTok(c.in)}/${fmtTok(c.out)}</td>
          <td class="c-hit${c.miss ? ' c-miss' : ''}"${missTip ? ` title="${esc(missTip)}"` : ''}>${fmtTok(c.cr)}${cwTxt} (${c.hit != null ? Math.round(c.hit * 100) + '%' : '—'})</td>
          <td>${c.tps ? Math.round(c.tps) + '/s' : '—'}${c.ttftMs ? ' · ' + (c.ttftMs / 1000).toFixed(1) + 's' : ''}</td>
        </tr>`;
      }).join('');
      tableHtml = `
        <table class="u-table">
          <thead><tr><th>时间</th><th>对话</th><th>模型</th><th>花费</th><th>入/出</th><th>缓存</th><th>速度/首字</th></tr></thead>
          <tbody>${rows || '<tr><td colspan="7" style="text-align:center;color:var(--fg-muted);padding:16px">没有符合筛选条件的调用记录</td></tr>'}</tbody>
        </table>`;
    }

    const loadMoreHtml = dayView === 'calls' && d.truncated
      ? `<div class="load-more-row">
          <span class="form-hint">当前已加载最新 ${d.shown} / ${d.totalCalls} 条（顶部合计与按对话汇总均覆盖全部）</span>
          <div class="btn-group">
            <button type="button" class="pill-btn" id="btn-more-600">再显示 600 条</button>
            <button type="button" class="pill-btn" id="btn-more-all">显示全部 (${d.totalCalls})</button>
          </div>
        </div>`
      : '';

    const missBadge = ms.count > 0
      ? ` <span class="miss-chip" title="当日缓存未命中 ${ms.count} 次，多花约 ${fmtUsd(ms.usd)}">miss +${fmtUsd(ms.usd)}</span>`
      : '';

    panelUsage.innerHTML = `
      <div class="usage-bar">
        <button class="pill-btn" id="btn-usage-back">← 返回用量</button>
        <span class="sec-title">${esc(d.date)} 调用明细</span>
        <span class="sec-value">${d.totalCalls || 0} 次 · ${fmtUsd(t.usd)}${missBadge}</span>
      </div>
      <div class="kpi-grid">
        <div class="kpi-card"><div class="kpi-label">当日花费</div><div class="kpi-val" style="color:var(--k-cost)">${fmtUsd(t.usd)}</div></div>
        <div class="kpi-card"><div class="kpi-label">调用次数</div><div class="kpi-val" style="color:var(--k-model)">${d.totalCalls || 0}</div></div>
        <div class="kpi-card"><div class="kpi-label">Token 总数</div><div class="kpi-val" style="color:var(--k-tokens)">${fmtTok(t.tokens)}</div></div>
        <div class="kpi-card"><div class="kpi-label">缓存命中</div><div class="kpi-val" style="color:var(--k-cache)">${pct(t.hit, 0)}</div></div>
      </div>
      <div class="calls-toolbar">
        <div class="btn-group">
          <button class="pill-btn ${dayView === 'calls' ? 'on' : ''}" data-dview="calls">逐次调用</button>
          <button class="pill-btn ${dayView === 'conv' ? 'on' : ''}" data-dview="conv">按对话 (${byConv.length})</button>
        </div>
        ${srcBtns}
        ${missToggle}
        ${convChip}
        ${modelChips}
      </div>
      <div class="u-table-wrap calls">${tableHtml}</div>
      ${loadMoreHtml}
    `;

    $('btn-usage-back').addEventListener('click', closeUsageDay);
    panelUsage.querySelectorAll('[data-dview]').forEach((b) => {
      b.addEventListener('click', () => { dayView = b.dataset.dview; renderUsageDay(); });
    });
    panelUsage.querySelectorAll('[data-dsrc]').forEach((b) => {
      b.addEventListener('click', () => { daySrc = b.dataset.dsrc; renderUsageDay(); });
    });
    panelUsage.querySelectorAll('[data-dmod]').forEach((b) => {
      b.addEventListener('click', () => {
        dayModel = dayModel === b.dataset.dmod ? '' : b.dataset.dmod;
        renderUsageDay();
      });
    });
    $('btn-day-miss')?.addEventListener('click', () => { dayOnlyMiss = !dayOnlyMiss; renderUsageDay(); });
    $('btn-clear-conv')?.addEventListener('click', () => { dayConv = ''; renderUsageDay(); });
    panelUsage.querySelectorAll('[data-pick-conv]').forEach((tr) => {
      tr.addEventListener('click', () => {
        dayConv = tr.dataset.pickConv;
        dayView = 'calls';
        renderUsageDay();
      });
    });
    $('btn-more-600')?.addEventListener('click', () => {
      dayLimit = (d.shown || 600) + 600;
      openUsageDay(d.date, false, { view: dayView, src: daySrc, model: dayModel, conv: dayConv, onlyMiss: dayOnlyMiss, limit: dayLimit });
    });
    $('btn-more-all')?.addEventListener('click', () => {
      dayLimit = 0;
      openUsageDay(d.date, false, { view: dayView, src: daySrc, model: dayModel, conv: dayConv, onlyMiss: dayOnlyMiss, limit: 0 });
    });
  }

  function weekBuckets(days) {
    const out = [];
    for (let i = 0; i < days.length; i += 7) {
      const slice = days.slice(i, i + 7);
      const mMap = new Map();
      let usd = 0, calls = 0, tokens = 0;
      const resets = [];
      for (const d of slice) {
        usd += Number(d.usd) || 0;
        calls += d.calls || 0;
        tokens += d.tokens || 0;
        if (d.resets?.length) resets.push(...d.resets);
        for (const m of (d.models || [])) {
          const prev = mMap.get(m.name) || { name: m.name, usd: 0, calls: 0, tokens: 0, in: 0, cr: 0 };
          prev.usd += Number(m.usd) || 0;
          prev.calls += m.calls || 0;
          prev.tokens += m.tokens || 0;
          prev.in += m.in || 0;
          prev.cr += m.cr || 0;
          mMap.set(m.name, prev);
        }
      }
      const models = [...mMap.values()]
        .map((m) => ({ ...m, hit: m.cr + m.in > 0 ? m.cr / (m.cr + m.in) : null }))
        .sort((a, b) => b.usd - a.usd);
      const first = slice[0].date;
      const lastD = slice[slice.length - 1].date;
      out.push({
        weekly: true,
        date: lastD,
        rangeLabel: first === lastD ? first : `${first.slice(5)} ~ ${lastD.slice(5)}`,
        isToday: slice.some((d) => d.isToday),
        usd,
        calls,
        tokens,
        resets,
        models,
      });
    }
    return out;
  }

  function renderUsage() {
    if (usageDayDate && usageDayData) { renderUsageDay(); return; }
    const u = usageData;
    if (!u) { loadUsage(); return; }
    const td = u.today || {};
    const days = u.days || [];
    const legend = u.legend || [];
    const totals = u.totals || {};
    const isWeekly = days.length > 62;
    const barItems = isWeekly ? weekBuckets(days) : days;
    const maxUsd = Math.max(0.01, ...barItems.map((d) => Number(d.usd) || 0));
    const peakDate = barItems.reduce((best, d) => ((d.usd || 0) > (best?.usd || 0) ? d : best), null)?.date;
    const activeWeeks = isWeekly ? barItems.filter((w) => (Number(w.usd) || 0) > 0) : [];
    const avgVal = isWeekly
      ? (activeWeeks.length ? activeWeeks.reduce((s, w) => s + (Number(w.usd) || 0), 0) / activeWeeks.length : 0)
      : (totals.avgUsd || 0);
    const avgPct = avgVal > 0 ? Math.min(100, (avgVal / maxUsd) * 100) : 0;

    const acct = u.account;
    const acctTxt = acct ? `${acct.masked}${acct.tier ? ' · ' + acct.tier.replace(/^Google AI\s*/i, '') : ''}` : '每日用量统计';

    const barsHtml = barItems.map((d, idx) => {
      const showVal = (!isWeekly && barItems.length <= 7 && d.usd > 0) || (d.usd > 0 && (d.isToday || d.date === peakDate));
      const parts = (d.models || []).slice().reverse().map((m, mi, arr) => {
        const h = Math.max(2, (m.usd / maxUsd) * 100);
        const col = modelColor(m.name, legend);
        const isTop = mi === arr.length - 1;
        return `<div class="chart-part${isTop ? ' top' : ''}" data-day-idx="${idx}" data-model="${esc(m.name)}" style="height:${h.toFixed(2)}%;--mc:${col}"></div>`;
      }).join('');
      const valTag = showVal ? `<div class="chart-val">${fmtUsdShort(d.usd)}</div>` : '';
      return `<div class="chart-col" data-day-idx="${idx}" data-date="${esc(d.date)}">${parts}${valTag}</div>`;
    }).join('');

    const step = barItems.length <= 7 ? 1 : Math.max(1, Math.ceil(barItems.length / 7));
    const labelsHtml = barItems.map((d, idx) => {
      const shortD = d.date.slice(5);
      const show = barItems.length <= 7 || d.isToday || idx === 0 || idx % step === 0;
      const rstMark = d.resets?.length ? '↺' : '';
      const txt = show ? (d.isToday ? '今天' : shortD) + rstMark : rstMark;
      return `<span class="${d.isToday ? 'today' : ''}" title="${esc(formatResetTip(d.resets))}">${txt}</span>`;
    }).join('');

    const legendHtml = legend.map((m) => {
      const col = modelColor(m.name, legend);
      return `<span class="legend-item" data-legend="${esc(m.name)}" style="--mc:${col}"><i></i><span>${esc(m.name)}</span><span class="v">${fmtUsdShort(m.usd)}</span></span>`;
    }).join('');

    const tableRows = days.slice().reverse().map((d) => {
      const qg = d.qGemini != null ? `G +${(d.qGemini * 100).toFixed(1)}%` : '';
      const qc = d.qClaude != null ? `C +${(d.qClaude * 100).toFixed(1)}%` : '';
      const rstTip = formatResetTip(d.resets);
      const rstBadge = d.resets?.length ? ` <span class="reset-tag" title="${esc(rstTip)}">↺</span>` : '';
      const qTxt = ([qg, qc].filter(Boolean).join(' ') || '—') + rstBadge;
      const cliDot = d.cliCalls > 0 ? `<span class="bf-tag" title="含 ${d.cliCalls} 次 agy CLI 调用">CLI</span>` : '';
      return `<tr class="clickable${d.isToday ? ' today' : ''}" data-date="${esc(d.date)}">
        <td>${esc(d.date.slice(5))}${d.bf ? '<span class="bf-tag" title="从对话库补算">补</span>' : ''}${cliDot}</td>
        <td class="c-cost">${fmtUsd(d.usd)}</td>
        <td>${d.calls || 0}</td>
        <td>${fmtTok(d.tokens)}</td>
        <td class="c-hit">${d.hit != null ? Math.round(d.hit * 100) + '%' : '—'}</td>
        <td class="c-q">${qTxt}</td>
      </tr>`;
    }).join('');

    const mt = u.missToday;
    let missBannerHtml = '';
    if (mt && mt.count > 0) {
      const sharePct = mt.totalUsd > 0 ? ` (${Math.round((mt.usd / mt.totalUsd) * 100)}%)` : '';
      const causes = MISS_ORDER
        .filter((k) => mt.by && mt.by[k] && mt.by[k].usd >= 0.05)
        .map((k) => `${MISS_CAUSE[k][0]} ${fmtUsd(mt.by[k].usd)}`)
        .join(' · ');
      const tipLines = MISS_ORDER
        .filter((k) => mt.by && mt.by[k])
        .map((k) => `${MISS_CAUSE[k][1]}：${mt.by[k].count} 次，多花约 ${fmtUsd(mt.by[k].usd)}`)
        .join('\n');
      missBannerHtml = `
        <div class="miss-banner" id="btn-miss-today" title="今天缓存未命中 ${mt.count} 次，多花约 ${fmtUsd(mt.usd)}：\n${esc(tipLines)}\n\n点击查看今天未命中的调用明细">
          <span>今日缓存未命中 +${fmtUsd(mt.usd)}${sharePct}${causes ? ' · ' + esc(causes) : ''}</span>
          <span>查看 →</span>
        </div>`;
    }

    const forecastsHtml = (u.forecasts || []).map((f) => {
      const capParts = [];
      if (f.capUsd) capParts.push(`$${Number(f.capUsd).toLocaleString('en-US')}`);
      if (f.capTokens) capParts.push(`${fmtTok(f.capTokens)} tok`);
      const capQual = f.capExact ? '按周期起点' : '按整日估算';
      const capStr = capParts.length ? `cap ≈ ${capParts.join(' / ')} (${capQual})` : 'cap 样本收集中';
      const use24 = f.proj24h != null;
      const proj = use24 ? f.proj24h : f.projWeek;
      const fullAt = use24 ? f.fullAt24h : f.fullAtWeek;
      const estStr = fullAt
        ? `预计耗尽 ${mdHm(fullAt)}`
        : proj != null
          ? `重置时约 ${Math.round(proj * 100)}%`
          : '—';
      const basis = use24 ? `按近 ${Math.round(f.spanH)}h 速度` : `按本周均速（已过 ${Math.round(f.elapsedH)}h）`;
      const earlyBadge = f.earlyReset
        ? `<span class="reset-tag" title="本周期始于提前重置：重置前已用 ${(f.earlyReset.fromUsed * 100).toFixed(1)}%，原定 ${mdHm(f.earlyReset.wasResetMs)} 重置">↺ 提前重置</span>`
        : '';
      const pastHtml = Array.isArray(f.pastCycles) && f.pastCycles.length
        ? `<div class="past-cycles">历史周期：${f.pastCycles.map((c) => {
            const range = `${md(c.startMs)}~${md(c.endMs)}`;
            const cap = c.capUsd ? `cap≈$${Number(c.capUsd).toLocaleString('en-US')}` : fmtUsd(c.usd);
            return `${range}${c.early ? '↺' : ''} ${cap} (${Math.round(c.used * 100)}%)`;
          }).join(' · ')}</div>`
        : '';
      return `<div class="forecast-card" title="本周期（${f.startMs ? mdHm(f.startMs) : '—'} 起）本机已计 ${fmtUsd(f.cycleUsd)} / ${fmtTok(f.cycleTokens)} tok；多设备共享时估值会有偏差">
        <div class="forecast-head">
          <span>${esc(f.label)} 每周额度 · ${Math.round(f.used * 100)}% used${earlyBadge}</span>
          <span>${estStr}</span>
        </div>
        <div class="forecast-meta">${capStr} · 重置 ${mdHm(f.resetMs)} · ${basis}</div>
        ${pastHtml}
      </div>`;
    }).join('');

    const spanLabel = usageSpan === 'all' ? `全部 ${u.spanDays || days.length} 天` : `近 ${u.span} 天`;
    const cliNote = u.hasCli ? ' · 含 agy CLI' : '';

    panelUsage.innerHTML = `
      <div class="usage-bar">
        <span class="acct-badge" title="${esc(acctTxt)}">${esc(acctTxt)}</span>
        <div class="btn-group">
          <button class="pill-btn ${usageSpan === 7 ? 'on' : ''}" data-span="7">7天</button>
          <button class="pill-btn ${usageSpan === 30 ? 'on' : ''}" data-span="30">30天</button>
          <button class="pill-btn ${usageSpan === 'all' ? 'on' : ''}" data-span="all">全部</button>
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
            ${avgPct > 0 ? `<div class="chart-avg" style="bottom:${avgPct.toFixed(1)}%" title="${isWeekly ? '周均' : '活跃日均'} ${fmtUsd(avgVal)}"></div>` : ''}
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
      <div class="form-hint" style="margin-top:5px">点击任意日期查看当天逐次调用明细${isWeekly ? '（柱图按周合并）' : ''} · ${spanLabel}合计 ${fmtUsd(totals.usd)}（日均 ${fmtUsd(totals.avgUsd)}）${cliNote} · 价格：${esc(u.priceSource || 'bundled')}</div>

      ${missBannerHtml}
      ${forecastsHtml ? `<div class="forecast-list">${forecastsHtml}</div>` : ''}
    `;

    panelUsage.querySelectorAll('[data-span]').forEach((b) => {
      b.addEventListener('click', () => {
        usageSpan = b.dataset.span === 'all' ? 'all' : Number(b.dataset.span);
        usageData = null;
        loadUsage();
      });
    });
    $('btn-usage-refresh')?.addEventListener('click', () => loadUsage(false));
    $('btn-miss-today')?.addEventListener('click', () => {
      if (td.date) openUsageDay(td.date, false, { view: 'calls', onlyMiss: true });
    });

    const barsEl = $('usage-bars');
    panelUsage.querySelectorAll('.chart-col').forEach((col) => {
      col.addEventListener('click', () => openUsageDay(col.dataset.date));
      col.addEventListener('mousemove', (ev) => {
        const d = barItems[Number(col.dataset.dayIdx)];
        if (!d) return;
        const titleLabel = d.weekly ? `周 ${d.rangeLabel}` : `${d.date}${d.bf ? '（补算）' : ''}`;
        const part = ev.target.closest('.chart-part');
        if (part) {
          const m = (d.models || []).find((x) => x.name === part.dataset.model);
          if (m) {
            const share = d.usd > 0 ? Math.round((m.usd / d.usd) * 100) : 0;
            showHoverCard(
              `<div class="hc-title">${esc(titleLabel)} · ${esc(m.name)}</div>` +
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
        const rstHtml = d.resets?.length ? `<div style="color:var(--k-warn);margin-top:2px">${esc(formatResetTip(d.resets))}</div>` : '';
        showHoverCard(
          `<div class="hc-title">${esc(titleLabel)} · ${fmtUsd(d.usd)}</div>` +
          (rows || '<div class="dim">无调用</div>') +
          `<div class="dim" style="margin-top:2px">${d.calls || 0} 次 · ${fmtTok(d.tokens)} tok · 点击看明细</div>` +
          rstHtml,
          ev
        );
      });
      col.addEventListener('mouseleave', hideHoverCard);
    });

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
      costly_call_usd: Math.max(0, Number($('cfg-costly').value) || 0),
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
          <div class="form-label"><span>中文界面（含 Swarm 面板）</span><span class="form-hint">把 Antigravity 与 Swarm 界面文字译为中文（不动对话和代码）；打开或关闭均立刻生效，无需刷新窗口。词典来自 agy-zh-pack</span></div>
          <input id="cfg-zh" type="checkbox" ${c.ui_zh ? 'checked' : ''} />
        </div>
        <div class="form-row">
          <div class="form-label"><span>临时切回英文</span><span class="form-hint">在任意窗口按 ${/Mac/i.test(navigator.platform || navigator.userAgent) ? 'Ctrl+Option+E' : 'Ctrl+Alt+E'} 也可切换（所有窗口与 Swarm 同步，不改设置、刷新恢复中文）</span></div>
          <button id="btn-toggle-en" type="button" class="secondary-btn" ${c.ui_zh ? '' : 'disabled'}>${liveStatus?.zhPaused ? '切回中文' : '临时切回英文'}</button>
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
          <div class="form-label"><span>高价调用提醒阈值 ($)</span><span class="form-hint">单次调用达到该折算金额在状态栏和明细标红提醒；填 0 关闭</span></div>
          <input id="cfg-costly" type="number" step="0.1" min="0" class="field-input" value="${c.costly_call_usd ?? 0.5}" />
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
    $('cfg-costly').addEventListener('change', onSettingsChange);
    $('btn-restart').addEventListener('click', onRestart);
    $('btn-toggle-en')?.addEventListener('click', async () => {
      const btn = $('btn-toggle-en');
      if (!btn || btn.disabled) return;
      btn.disabled = true;
      try {
        const res = await postJson('/api/i18n/toggle-english', {});
        const j = await res.json().catch(() => ({}));
        if (liveStatus) liveStatus.zhPaused = !!j.paused;
      } catch (e) {}
      await updateLiveStatus();
    });
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
    const enBtn = document.getElementById('btn-toggle-en');
    if (!box && !enBtn) return;
    try {
      const j = await (await fetch('/api/live-status', { cache: 'no-store' })).json();
      liveStatus = j;
      if (enBtn) {
        const zhOn = $('cfg-zh') ? $('cfg-zh').checked : !!j.uiZh;
        enBtn.disabled = !zhOn || !j.debug;
        enBtn.textContent = j.zhPaused ? '切回中文' : '临时切回英文';
      }
      if (!box) return;
      const state = j.debug ? '<span class="ok">调试端口已连接，状态栏和中文界面可用</span>'
        : '<span class="bad">没连上调试端口：状态栏和中文界面需要 Antigravity 以调试模式启动</span>';
      const flag = j.relaunch?.lastFlag ? ` · 进程校验: ${esc(j.relaunch.lastFlag)}` : '';
      const swarm = j.swarmFrames ? ` · Swarm 面板: ${j.swarmFrames}` : '';
      const zhNote = j.zhPaused ? ' · <span class="bad">当前临时英文</span>' : '';
      box.innerHTML = state + zhNote + `<br>CDP: ${esc(j.cdp || '—')}${swarm}${flag}<br>language_server: ${esc(j.ls || '—')}` +
        (j.panes && j.panes.length ? `<br>showing: ${j.panes.map((x) => esc(String(x).slice(0, 8))).join(', ')}` : '');
    } catch (e) {
      if (box) box.textContent = 'live status unavailable';
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
