// Side panel (AUX_PANE) — Antigravity Tonal Quiet UI:
// Overview (dual rings, token split bar, dedicated speed card, quota + projected forecast, subagents),
// Turns, Usage (today KPIs, range-scoped stacked bar chart + daily table, standalone calendar heatmap,
// day drill-down, weekly quota forecasts), Theme (8 live palettes + look controls), and Settings
// (unified health/update banner, M3 switches, segment pill cloud, collapsible tips, debug relaunch).
(() => {
  const $ = (id) => document.getElementById(id);
  const convSelect = $('conv-select');
  const livePill = $('live-pill');
  const liveLabel = $('live-label');
  const stripBox = $('strip-box');
  const stripEl = $('status-strip');
  const toggleStripBtn = $('btn-toggle-strip');
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
  let stripCollapsed = false;

  // Settings / Theme draft merged onto `last.config` for instant live previews before debounced save.
  let draft = null;
  let saveTimer = 0;
  let lastJson = '';
  let last = null;

  // Settings local UI state (persists across renders)
  let openTip = '';
  let diagOpen = false;

  // Usage tab + standalone calendar heatmap state
  let usageSpan = 7; // 7 | 30 | 'all'
  let usageData = null;
  let usageLoading = false;
  let heatmapData = null;
  let heatmapLoading = false;
  let hmMetric = 'usd'; // 'usd' | 'tokens' | 'calls'
  let usageDayDate = null;
  let usageDayData = null;
  let usageHoverCard = null;
  let usageDayRefreshTimer = 0;
  let usageDayReqSeq = 0;
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

  // Live status & update center caches
  let liveStatus = null;
  let updateStatus = null;
  let updatePollTimer = 0;

  // ---------------------------------------------------------------- scheme sync
  function syncScheme() {
    if (!getComputedStyle(document.documentElement).getPropertyValue('--background').trim()) {
      document.documentElement.dataset.theme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      return;
    }
    const probe = document.createElement('span');
    probe.style.cssText = 'position:absolute;visibility:hidden;color:var(--background, #15171b)';
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

  // ---------------------------------------------------------------- header controls & tabs
  function setStripCollapsed(collapsed) {
    stripCollapsed = !!collapsed;
    stripBox?.classList.toggle('hidden', stripCollapsed);
    if (toggleStripBtn) toggleStripBtn.textContent = stripCollapsed ? '预览 ▸' : '预览 ▾';
  }
  toggleStripBtn?.addEventListener('click', () => setStripCollapsed(!stripCollapsed));

  function switchTab(tab) {
    activeTab = tab;
    for (const b of document.querySelectorAll('.tab')) b.classList.toggle('active', b.dataset.tab === tab);
    panelOverview.classList.toggle('hidden', activeTab !== 'overview');
    panelTurns.classList.toggle('hidden', activeTab !== 'turns');
    panelUsage.classList.toggle('hidden', activeTab !== 'usage');
    panelTheme.classList.toggle('hidden', activeTab !== 'theme');
    panelSettings.classList.toggle('hidden', activeTab !== 'settings');
    hideHoverCard();
    // Auto-expand statusbar preview when entering Theme tab so live edits are immediately visible.
    if (activeTab === 'theme' && stripCollapsed) setStripCollapsed(false);
    if (activeTab === 'usage') {
      loadUsage();
      if (!heatmapData) loadHeatmapData(true);
    }
    if (activeTab === 'settings') {
      refreshUpdateStatus();
      updateLiveStatus();
    }
    if (last) renderPanels(last, true);
  }
  for (const btn of document.querySelectorAll('.tab')) {
    btn.addEventListener('click', () => switchTab(btn.dataset.tab));
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
  const md = (ms) => { const d = new Date(ms); return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`; };
  const dateKeyLocal = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  function parseLocalDate(str) {
    const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(String(str || ''));
    return m ? new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3])) : new Date();
  }
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
  function ringSvg(percent, colorCss) {
    const clamped = Math.max(0, Math.min(100, Number(percent) || 0));
    const r = 19, circ = 2 * Math.PI * r;
    const dash = (circ * clamped) / 100;
    return `<svg class="ring-svg" viewBox="0 0 48 48" aria-hidden="true">
      <circle cx="24" cy="24" r="${r}" fill="none" stroke="var(--cell-off)" stroke-width="4.8"/>
      <circle cx="24" cy="24" r="${r}" fill="none" stroke="${colorCss}" stroke-width="4.8" stroke-linecap="round" stroke-dasharray="${dash.toFixed(1)} ${circ.toFixed(1)}"/>
    </svg>`;
  }
  function sparkAreaSvg(series, colorCss, gradId) {
    if (!Array.isArray(series) || series.length < 2) return '';
    const w = 260, h = 28;
    const min = Math.min(...series), max = Math.max(...series);
    const span = Math.max(1, max - min);
    const pts = series.map((v, i) => {
      const x = ((i / (series.length - 1)) * w).toFixed(1);
      const y = (h - 3 - ((v - min) / span) * (h - 6)).toFixed(1);
      return `${x},${y}`;
    }).join(' ');
    const areaPts = `${pts} ${w},${h} 0,${h}`;
    return `<svg class="spark-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" aria-hidden="true">
      <defs>
        <linearGradient id="${gradId}" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="${colorCss}" stop-opacity="0.26"/>
          <stop offset="100%" stop-color="${colorCss}" stop-opacity="0"/>
        </linearGradient>
      </defs>
      <polygon fill="url(#${gradId})" points="${areaPts}"/>
      <polyline fill="none" stroke="${colorCss}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" points="${pts}"/>
    </svg>`;
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
        setSaveState('已保存 ✓', false, true);
        if (draft === sent) draft = null;
        lastJson = '';
        await poll();
      } catch (e) {
        setSaveState('保存失败：' + e.message, true, false);
      }
    }, 350);
  }
  function setSaveState(text, bad = false, ok = false) {
    const n = $('save-state') || $('theme-save-state');
    if (n) {
      n.textContent = text;
      n.classList.toggle('bad', !!bad);
      n.classList.toggle('ok', !!ok);
    }
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

  // Shared helper: render Weekly Quota Forecast sub-cards (used in both Overview and Usage)
  function renderForecastSubcards(forecasts, compact = false) {
    if (!Array.isArray(forecasts) || !forecasts.length) return '';
    return forecasts.map((f) => {
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
          ? `预计重置时约 ${Math.round(proj * 100)}%`
          : '—';
      const basis = use24 ? `按近 ${Math.round(f.spanH)}h 速度` : `按本周均速（已过 ${Math.round(f.elapsedH)}h）`;
      const earlyBadge = f.earlyReset
        ? `<span class="reset-tag" title="本周期始于提前重置：重置前已用 ${(f.earlyReset.fromUsed * 100).toFixed(1)}%，原定 ${mdHm(f.earlyReset.wasResetMs)} 重置">↺ 提前重置</span>`
        : '';
      const usedPct = Math.max(0, Math.min(100, Math.round((Number(f.used) || 0) * 100)));
      const estColor = fullAt ? 'var(--warning)' : 'var(--k-quota)';
      const barColor = fullAt ? 'var(--warning)' : 'var(--k-quota)';
      const pastHtml = !compact && Array.isArray(f.pastCycles) && f.pastCycles.length
        ? `<div class="past-cycles">历史周期：${f.pastCycles.map((c) => {
            const range = `${md(c.startMs)}~${md(c.endMs)}`;
            const cap = c.capUsd ? `cap≈$${Number(c.capUsd).toLocaleString('en-US')}` : fmtUsd(c.usd);
            return `${range}${c.early ? '↺' : ''} ${cap} (${Math.round(c.used * 100)}%)`;
          }).join(' · ')}</div>`
        : '';
      const tip = `本周期（${f.startMs ? mdHm(f.startMs) : '—'} 起）本机已计 ${fmtUsd(f.cycleUsd)} / ${fmtTok(f.cycleTokens)} tok；多设备共享时估值会有偏差`;
      if (compact) {
        return `<div class="forecast-sub" title="${esc(tip)}">
          <div class="forecast-sub-head">
            <span style="color:${estColor}">${esc(f.label)} · ${estStr}${earlyBadge}</span>
            <span class="tonal-tag ${fullAt ? 'warn' : 'pri'}">${capParts.length ? 'cap ≈ ' + capParts.join(' / ') : '样本收集中'}</span>
          </div>
          <div class="forecast-sub-meta">已用 ${usedPct}% · 本周期已计 ${fmtUsd(f.cycleUsd)} · 重置 ${f.resetMs ? mdHm(f.resetMs) : '—'} · ${basis}</div>
        </div>`;
      }
      return `<div class="forecast-sub" title="${esc(tip)}">
        <div class="forecast-sub-head">
          <span style="color:var(--k-quota)">${esc(f.label)} 每周额度 · ${usedPct}% used${earlyBadge}</span>
          <span style="color:${fullAt ? 'var(--warning)' : 'var(--fg)'}">${estStr}</span>
        </div>
        <div class="q-track" style="margin:3px 0"><i style="width:${usedPct}%;background:${barColor}"></i></div>
        <div class="forecast-sub-meta">${capStr} · 重置 ${f.resetMs ? mdHm(f.resetMs) : '—'} · ${basis}</div>
        ${pastHtml}
      </div>`;
    }).join('');
  }

  // ---------------------------------------------------------------- Overview tab
  function renderOverview(p) {
    const { ctx, cache, tokens: t, speed } = p;
    const ctxPct = ctx.max ? (ctx.used / ctx.max) * 100 : 0;
    const ctxColor = ctxPct >= 85 ? 'var(--k-crit)' : ctxPct >= 65 ? 'var(--k-warn)' : 'var(--k-context)';
    const cacheHitPct = (cache.lastHit || 0) * 100;

    // Checkpoint viewer button & expandable summary box
    const ckptBtn = ctx.compactions > 0
      ? `<button type="button" class="pill-btn sm" id="btn-view-ckpt">${ckptOpenConvId === p.id ? '收起摘要' : `压缩 ${ctx.compactions}×`}</button>`
      : `<span style="color:var(--fg-faint)">压缩 0×</span>`;
    let ckptBoxHtml = '';
    if (ckptOpenConvId === p.id) {
      if (ckptLoading) {
        ckptBoxHtml = '<div class="ckpt-box"><div class="ckpt-meta">读取压缩摘要中…</div></div>';
      } else if (ckptData?.err) {
        ckptBoxHtml = `<div class="ckpt-box"><div class="ckpt-head"><span>上下文压缩查看器</span><button type="button" class="pill-btn sm" id="btn-close-ckpt">收起</button></div><div class="ckpt-meta">${esc(ckptData.err)}</div></div>`;
      } else if (ckptData) {
        const atStr = ckptData.at ? mdHm(ckptData.at) : '—';
        ckptBoxHtml = `
          <div class="ckpt-box">
            <div class="ckpt-head">
              <span>最近一次压缩摘要（共 ${ckptData.n} 次 · #${ckptData.step} · ${atStr}）</span>
              <button type="button" class="pill-btn sm" id="btn-close-ckpt">收起</button>
            </div>
            <div class="ckpt-meta">截断前用户请求 ${ckptData.requests} 条 · Artifacts ${ckptData.artifacts} 个 · ${esc(ckptData.log || '')}</div>
            <pre class="ckpt-summary">${esc(ckptData.summary || '（日志中未找到 <summary> 区块）')}</pre>
          </div>`;
      }
    }

    // Token distribution bar segments
    const totTok = Math.max(1, Number(t.total) || 0);
    const segBarParts = [
      [t.cache, 'var(--k-cache)', `缓存读取 ${fmtTok(t.cache)}`],
      [t.fresh, 'var(--k-model)', `未缓存输入 ${fmtTok(t.fresh)}`],
      [t.cacheWrite, 'var(--k-quota)', `缓存写入 ${fmtTok(t.cacheWrite)}`],
      [t.out, 'var(--k-cost)', `模型输出 ${fmtTok(t.out)}`],
    ].filter(([v]) => v > 0);
    const splitBarHtml = t.total > 0
      ? `<div class="split-bar">${segBarParts.map(([v, col, tip]) =>
          `<span style="width:${((v / totTok) * 100).toFixed(2)}%;background:${col}" title="${esc(tip)}"></span>`
        ).join('')}</div>`
      : '';

    const ms = cache.missSummary || { count: cache.misses || 0, usd: 0, by: {} };
    const missPillsHtml = ms.count > 0
      ? `<div class="miss-pills">
          <span class="miss-chip">缓存未命中 ${ms.count}× · +${fmtUsd(ms.usd)}</span>
          ${MISS_ORDER.filter((k) => ms.by && ms.by[k]).map((k) => {
            const b = ms.by[k];
            const [shortLbl, desc] = MISS_CAUSE[k] || [k, k];
            return `<span class="tonal-tag" title="${esc(desc)}：${b.count} 次，多花约 ${fmtUsd(b.usd)}">${esc(shortLbl)} ${b.count}× ${fmtUsd(b.usd)}</span>`;
          }).join('')}
        </div>`
      : '';

    const todayVal = t.todayCliCost > 0
      ? `<b>${fmtUsd(t.todayCost)}</b> <span style="color:var(--fg-muted);font-size:10.5px">(含 CLI ${fmtUsd(t.todayCliCost)})</span>`
      : `<b>${fmtUsd(t.todayCost)}</b>`;

    // Quota windows (dual track: used + elapsed) + embedded weekly forecasts
    const quotaRows = (p.quota || []).map((q) => {
      const label = q.window === '5h' ? '5 小时窗口' : q.window === 'weekly' ? '每周窗口' : q.window;
      const pacePill = q.pace == null || Math.abs(q.pace) < 0.01
        ? '<span class="tonal-tag" style="padding:1px 6px;font-size:10px">平稳</span>'
        : q.pace > 0
          ? `<span class="tonal-tag warn" style="padding:1px 6px;font-size:10px">↑${Math.round(q.pace * 100)}%</span>`
          : `<span class="tonal-tag ok" style="padding:1px 6px;font-size:10px">↓${Math.round(-q.pace * 100)}%</span>`;
      return `
        <div class="q-block">
          <div class="q-head">
            <span class="q-head-name">${esc(label)}</span>
            <span class="q-head-val">
              <b>${pct(q.used, 0)}</b>
              <span style="color:var(--fg-muted)">${q.resetMs ? fmtLeft(q.resetMs) + ' 后重置' : '—'}</span>
              ${pacePill}
            </span>
          </div>
          <div class="q-track"><i style="width:${Math.min(100, (q.used || 0) * 100).toFixed(1)}%;background:var(--k-quota)"></i></div>
          <div class="q-track sub"><i style="width:${Math.min(100, (q.elapsed || 0) * 100).toFixed(1)}%;background:var(--fg-muted)"></i></div>
        </div>`;
    }).join('');

    const forecasts = (usageData || heatmapData)?.forecasts || [];
    const ovForecastHtml = forecasts.length
      ? `<div class="forecast-stack">
          ${renderForecastSubcards(forecasts, true)}
          <div style="display:flex;justify-content:flex-end">
            <button type="button" class="pill-btn sm" id="btn-ov-go-usage">查看完整周期与用量 →</button>
          </div>
        </div>`
      : '';

    const subs = p.subagents || [];
    const subagentsSection = subs.length ? `
      <div class="sec-title"><span>子代理 (${subs.length})</span><span class="sec-meta">${fmtUsd(p.subCost)}</span></div>
      <div class="surface">
        ${subs.map((s) => `
          <div class="item">
            <div class="item-info">
              <div class="item-title">
                <span>${esc(s.type || 'subagent')}${s.role ? ' · ' + esc(s.role) : ''}</span>
                <span class="tonal-tag ${s.running ? 'ok' : ''}" style="font-size:10px">${s.running ? 'running' : 'done'}</span>
              </div>
              <div class="item-sub" style="font-family:var(--mono)">${esc(s.model || '—')} · ${fmtTok(s.tokens)} tok · ${fmtUsd(s.cost)}</div>
            </div>
            <button type="button" class="pill-btn sm btn-open-conv" data-conv="${esc(s.id)}">打开</button>
          </div>`).join('')}
      </div>` : '';

    const warmTxt = cache.warmLeftSec
      ? `<span class="ok">热 · ${fmtDur(cache.warmLeftSec)}</span>`
      : `<span>冷 · ${Math.round((cache.ttlSec || 300) / 60)}m TTL</span>`;
    const ctxLeftInfo = ctx.turnsLeft ? `剩 ${fmtTok(ctx.left)} · ~${ctx.turnsLeft} 轮` : `剩 ${fmtTok(ctx.left)}`;

    panelOverview.innerHTML = `
      <!-- Dual Ring Hero Cards -->
      <div class="ring-grid">
        <div class="ring-card">
          <div class="ring-top">
            ${ringSvg(ctxPct, ctxColor)}
            <div class="ring-info">
              <div class="ring-label">上下文窗口</div>
              <div class="ring-hero" style="color:${ctxColor}">${ctx.max ? ctxPct.toFixed(1) + '%' : '—'}</div>
              <div class="ring-sub">${fmtTok(ctx.used)}${ctx.max ? ' / ' + fmtTok(ctx.max) : ''}</div>
            </div>
          </div>
          <div class="ring-foot">
            <span>${ctxLeftInfo}</span>
            ${ckptBtn}
          </div>
        </div>

        <div class="ring-card">
          <div class="ring-top">
            ${ringSvg(cacheHitPct, 'var(--k-cache)')}
            <div class="ring-info">
              <div class="ring-label">Prompt 缓存</div>
              <div class="ring-hero" style="color:var(--k-cache)">${pct(cache.lastHit)}</div>
              <div class="ring-sub">${warmTxt}</div>
            </div>
          </div>
          <div class="ring-foot">
            <span>均值 ${pct(cache.sessionHit)}</span>
            <span class="ok">省 ${fmtUsd(cache.savedUsd)}</span>
          </div>
        </div>
      </div>

      ${ckptBoxHtml}

      <!-- Tokens & Cost Card -->
      <div class="sec-title">
        <span>费用与 Token 分布</span>
        <span class="sec-meta">${t.includesSubagents ? '含子代理 · ' : ''}${fmtTok(t.total)} tok</span>
      </div>
      <div class="surface surface-pad">
        <div class="hero-row">
          <span class="hero-num" style="color:var(--k-cost)">${fmtUsd(t.cost)}</span>
          <span class="hero-unit">今日全部 ${todayVal}</span>
        </div>
        ${splitBarHtml}
        <div class="kv-grid">
          ${kv('未缓存输入', `${fmtTok(t.fresh)} (${fmtUsd(t.costIn)})`)}
          ${kv('缓存读取', `${fmtTok(t.cache)} (${fmtUsd(t.costCache)})`)}
          ${t.cacheWrite > 0 ? kv('缓存写入', `${fmtTok(t.cacheWrite)} (${fmtUsd(t.costCacheWrite)})`) : ''}
          ${kv('模型输出', `${fmtTok(t.out)} (${fmtUsd(t.costOut)})`)}
          ${kv('消耗速率', t.ratePerHr != null ? fmtUsd(t.ratePerHr) + '/h' : '—')}
          ${kv('今日 Token', fmtTok(t.todayTokens))}
        </div>
        ${missPillsHtml}
      </div>

      <!-- Dedicated Speed & Timing Card -->
      <div class="sec-title">
        <span>速度与延迟</span>
        <span class="sec-meta">会话耗时 ${fmtDur(speed.wallSec)}</span>
      </div>
      <div class="surface surface-pad">
        <div class="hero-row" style="margin-bottom:10px">
          <div style="display:flex;align-items:baseline;gap:6px">
            <span class="hero-num" style="color:var(--k-tokens)">${speed.lastTps ? speed.lastTps.toFixed(1) : '—'}</span>
            <span class="hero-unit">tok/s 最近输出</span>
          </div>
          <span class="tonal-tag">TTFT ${speed.lastTtftMs ? (speed.lastTtftMs / 1000).toFixed(1) + 's' : '—'}</span>
        </div>
        <div class="kv-grid">
          ${kv('平均速度', speed.avgTps ? speed.avgTps.toFixed(1) + ' tok/s' : '—')}
          ${kv('最近首字', speed.lastTtftMs ? (speed.lastTtftMs / 1000).toFixed(1) + 's' : '—')}
          ${kv('平均首字', speed.avgTtftMs ? (speed.avgTtftMs / 1000).toFixed(1) + 's' : '—')}
          ${kv('上次上下文 Δ', (ctx.delta >= 0 ? '+' : '') + fmtTok(ctx.delta))}
        </div>
        ${speed.series.length >= 2 ? `<div class="spark-wrap"><span class="spark-lbl">TPS 趋势</span>${sparkAreaSvg(speed.series, 'var(--k-timing)', 'tps-grad')}</div>` : ''}
        ${ctx.series.length >= 2 ? `<div class="spark-wrap"><span class="spark-lbl">上下文增长</span>${sparkAreaSvg(ctx.series, 'var(--k-context)', 'ctx-grad')}</div>` : ''}
      </div>

      <!-- Antigravity Quota & Projected Consumption Card -->
      <div class="sec-title">
        <span>Antigravity 额度与预计消耗</span>
        <span class="sec-meta">已用 / 时间进度</span>
      </div>
      <div class="surface surface-pad">
        ${quotaRows || '<div class="form-hint">暂无额度数据</div>'}
        ${ovForecastHtml}
      </div>

      ${subagentsSection}
    `;

    $('btn-view-ckpt')?.addEventListener('click', () => toggleCheckpointViewer(p.id));
    $('btn-close-ckpt')?.addEventListener('click', () => toggleCheckpointViewer(p.id));
    $('btn-ov-go-usage')?.addEventListener('click', () => switchTab('usage'));
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
      panelTurns.innerHTML = '<div class="empty-msg">暂无模型调用记录</div>';
      return;
    }
    const total = p.turnsTotal ?? turns.length;
    const capText = total > turns.length ? `最近 ${turns.length} / 共 ${total} 条` : `共 ${total} 条`;
    panelTurns.innerHTML = `
      <div class="sec-title"><span>模型调用记录</span><span class="sec-meta">${capText}</span></div>
      <div class="surface">
        ${turns.slice().reverse().map((t) => {
          const m = t.miss;
          const missTag = m
            ? `<span class="miss-chip" title="${esc(missNote(m))}">miss +${fmtUsd(m.usd)} · ${esc(MISS_CAUSE[m.cause]?.[0] || m.cause)}</span>`
            : '';
          const costCls = t.costly ? 'turn-costly' : '';
          const costTip = t.costly
            ? (m ? `高价调用提醒 · ${missNote(m)}` : '高价调用提醒：缓存已命中，主要来自长上下文或大量输出')
            : '';
          return `
            <div class="item">
              <div class="item-info">
                <div class="turn-label">${esc(t.label)}</div>
                <div class="turn-step">
                  <span>#${t.step ?? '–'} · ${hms(t.at)} · ctx ${fmtTok(t.ctx)} (${Math.round(t.hit * 100)}% hit)</span>
                  ${missTag}
                </div>
              </div>
              <div class="turn-right">
                <div class="${costCls}"${costTip ? ` title="${esc(costTip)}"` : ''}>${t.costly ? '!' : ''}${fmtUsd(t.cost)} · +${fmtTok(t.out)}</div>
                <div class="turn-sub">${t.tps ? Math.round(t.tps) + ' tok/s' : '—'}${t.ttftMs ? ' · ' + (t.ttftMs / 1000).toFixed(1) + 's TTFT' : ''}</div>
              </div>
            </div>`;
        }).join('')}
      </div>`;
  }

  // ---------------------------------------------------------------- Usage tab + Standalone Heatmap
  function showHoverCard(html, ev) {
    if (!usageHoverCard) {
      usageHoverCard = document.createElement('div');
      usageHoverCard.className = 'hover-card';
      document.body.appendChild(usageHoverCard);
    }
    usageHoverCard.innerHTML = html;
    usageHoverCard.style.display = 'block';
    const w = usageHoverCard.offsetWidth, h = usageHoverCard.offsetHeight;
    const x = Math.min(window.innerWidth - w - 10, Math.max(10, ev.clientX + 12));
    const y = ev.clientY - h - 12 > 10 ? ev.clientY - h - 12 : Math.min(window.innerHeight - h - 10, ev.clientY + 14);
    usageHoverCard.style.left = x + 'px';
    usageHoverCard.style.top = y + 'px';
  }
  function hideHoverCard() {
    if (usageHoverCard) usageHoverCard.style.display = 'none';
  }

  async function loadHeatmapData(silent = false) {
    if (heatmapLoading) return;
    heatmapLoading = true;
    try {
      const res = await fetch('/api/usage?days=all', { cache: 'no-store' });
      if (res.ok) {
        heatmapData = await res.json();
        if (usageSpan === 'all') usageData = heatmapData;
        if (activeTab === 'usage' && !usageDayDate && usageData) renderUsage();
        if (activeTab === 'overview' && last) renderOverview(last);
      }
    } catch (e) {
      if (!silent) console.error('heatmap load failed:', e);
    } finally {
      heatmapLoading = false;
    }
  }

  async function loadUsage(silent = false) {
    if (usageLoading) return;
    usageLoading = true;
    if (!silent && !usageData) {
      panelUsage.innerHTML = '<div class="empty-msg">加载每日用量数据中…</div>';
    }
    try {
      const res = await fetch(`/api/usage?days=${usageSpan}`, { cache: 'no-store' });
      if (res.ok) {
        usageData = await res.json();
        if (usageSpan === 'all') heatmapData = usageData;
        if (activeTab === 'usage' && !usageDayDate) renderUsage();
        if (activeTab === 'overview' && last) renderOverview(last);
      }
    } catch (e) {
      if (!usageData) panelUsage.innerHTML = `<div class="empty-msg err">加载失败：${esc(e.message)}</div>`;
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
    usageDayRefreshTimer = 0;
    const seq = ++usageDayReqSeq;
    if (!silent) {
      panelUsage.innerHTML = `<div class="empty-msg">加载 ${esc(date)} 调用明细…</div>`;
    }
    try {
      const res = await fetch(`/api/usage/day?date=${encodeURIComponent(date)}&limit=${dayLimit}`, { cache: 'no-store' });
      if (!res.ok) throw new Error('HTTP ' + res.status);
      const next = await res.json();
      if (seq !== usageDayReqSeq) return;
      const changed = JSON.stringify(next) !== JSON.stringify(usageDayData);
      usageDayData = next;
      if (activeTab === 'usage' && usageDayDate === date && (!silent || changed)) {
        const scroller = panelUsage.querySelector('.u-table-wrap');
        const prevTop = scroller ? scroller.scrollTop : 0;
        renderUsageDay();
        const after = panelUsage.querySelector('.u-table-wrap');
        if (after && silent) after.scrollTop = prevTop;
      }
      const todayKey = usageData?.today?.date || heatmapData?.today?.date;
      if (date === todayKey) {
        clearInterval(usageDayRefreshTimer);
        usageDayRefreshTimer = setInterval(() => {
          if (!document.hidden && activeTab === 'usage' && usageDayDate === date) openUsageDay(date, true);
        }, 30000);
      }
    } catch (e) {
      if (seq !== usageDayReqSeq) return;
      panelUsage.innerHTML = `<div class="usage-top"><button class="pill-btn sm" id="btn-usage-back">← 返回用量</button></div><div class="empty-msg err">读取明细失败：${esc(e.message)}</div>`;
      $('btn-usage-back')?.addEventListener('click', closeUsageDay);
    }
  }

  function closeUsageDay() {
    usageDayDate = null;
    usageDayData = null;
    usageDayReqSeq++;
    clearInterval(usageDayRefreshTimer);
    usageDayRefreshTimer = 0;
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
    const legendRef = usageData?.legend || heatmapData?.legend || models;

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
      ? `<div class="pill-seg">
          <button type="button" class="${daySrc === 'all' ? 'on' : ''}" data-dsrc="all">全部</button>
          <button type="button" class="${daySrc === 'desktop' ? 'on' : ''}" data-dsrc="desktop">桌面端</button>
          <button type="button" class="${daySrc === 'cli' ? 'on' : ''}" data-dsrc="cli">CLI (${d.cliCalls})</button>
        </div>`
      : '';

    const modelChips = models.length >= 2
      ? `<div class="btn-group">${models.slice(0, 6).map((m) => {
          const col = modelColor(m.name, legendRef);
          const on = dayModel === m.name;
          return `<button type="button" class="pill-btn sm ${on ? 'on' : ''}" data-dmod="${esc(m.name)}"><i class="model-dot" style="--mc:${col}"></i>${esc(m.name)}</button>`;
        }).join('')}</div>`
      : '';

    const missToggle = ms.count > 0
      ? `<button type="button" class="pill-btn sm ${dayOnlyMiss ? 'on' : ''}" id="btn-day-miss" title="只看缓存未命中的调用">仅未命中 (${ms.count})</button>`
      : '';

    const convChip = dayConv
      ? `<button type="button" class="pill-btn sm on" id="btn-clear-conv" title="取消对话筛选">对话: ${esc(convTitle.slice(0, 18))} ×</button>`
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
          <span>当前已加载最新 ${d.shown} / ${d.totalCalls} 条（顶部合计与按对话汇总均覆盖全部）</span>
          <div class="btn-group">
            <button type="button" class="pill-btn sm" id="btn-more-600">再显示 600 条</button>
            <button type="button" class="pill-btn sm" id="btn-more-all">显示全部 (${d.totalCalls})</button>
          </div>
        </div>`
      : '';

    const missBadge = ms.count > 0
      ? ` <span class="miss-chip" title="当日缓存未命中 ${ms.count} 次，多花约 ${fmtUsd(ms.usd)}">miss +${fmtUsd(ms.usd)}</span>`
      : '';

    panelUsage.innerHTML = `
      <div class="usage-top" style="flex-wrap:wrap">
        <button type="button" class="pill-btn sm" id="btn-usage-back">← 返回用量</button>
        <span style="font-size:13px;font-weight:600">${esc(d.date)} 调用明细</span>
        <span class="tonal-tag">${d.totalCalls || 0} 次 · ${fmtUsd(t.usd)}${missBadge}</span>
      </div>
      <div class="kpi-grid">
        <div class="kpi-card"><div class="kpi-label">当日花费</div><div class="kpi-val" style="color:var(--k-cost)">${fmtUsd(t.usd)}</div></div>
        <div class="kpi-card"><div class="kpi-label">调用次数</div><div class="kpi-val" style="color:var(--k-model)">${d.totalCalls || 0}</div></div>
        <div class="kpi-card"><div class="kpi-label">Token 总数</div><div class="kpi-val" style="color:var(--k-tokens)">${fmtTok(t.tokens)}</div></div>
        <div class="kpi-card"><div class="kpi-label">缓存命中</div><div class="kpi-val" style="color:var(--k-cache)">${pct(t.hit, 0)}</div></div>
      </div>
      <div class="calls-toolbar">
        <div class="pill-seg">
          <button type="button" class="${dayView === 'calls' ? 'on' : ''}" data-dview="calls">逐次调用</button>
          <button type="button" class="${dayView === 'conv' ? 'on' : ''}" data-dview="conv">按对话 (${byConv.length})</button>
        </div>
        ${srcBtns}
        ${missToggle}
        ${convChip}
        ${modelChips}
      </div>
      <div class="u-table-wrap">${tableHtml}</div>
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

  // Build standalone calendar heatmap (independent of 7d/30d/all range selector)
  function buildHeatmapView(srcData) {
    if (!srcData || !Array.isArray(srcData.days)) {
      return { html: '<div class="form-hint" style="padding:12px 0;text-align:center">加载热力图数据中…</div>', dayMap: new Map() };
    }
    const dayMap = new Map();
    for (const d of srcData.days) dayMap.set(d.date, d);

    const todayStr = srcData.today?.date || dateKeyLocal(new Date());
    const endDate = parseLocalDate(todayStr);
    // Show at least 20 weeks (140 days), or full available history up to 400 days.
    const totalSpan = Math.max(140, srcData.days.length);
    const startDate = new Date(endDate);
    startDate.setDate(endDate.getDate() - (totalSpan - 1));

    const series = [];
    for (let cur = new Date(startDate); cur <= endDate; cur.setDate(cur.getDate() + 1)) {
      const key = dateKeyLocal(cur);
      const rec = dayMap.get(key);
      const item = rec
        ? { ...rec, dow: cur.getDay(), month: cur.getMonth() + 1 }
        : { date: key, isToday: key === todayStr, usd: 0, calls: 0, tokens: 0, hit: null, models: [], resets: [], dow: cur.getDay(), month: cur.getMonth() + 1 };
      dayMap.set(key, item);
      series.push(item);
    }

    const metricVal = (d) => (hmMetric === 'usd' ? Number(d.usd) || 0 : hmMetric === 'tokens' ? Number(d.tokens) || 0 : Number(d.calls) || 0);
    const activeVals = series.map(metricVal).filter((v) => v > 0).sort((a, b) => a - b);
    const q = (p) => activeVals[Math.min(activeVals.length - 1, Math.floor(activeVals.length * p))] || 0;
    const t1 = q(0.25), t2 = q(0.55), t3 = q(0.82);
    const lvOf = (v) => (v <= 0 ? 0 : v <= t1 ? 1 : v <= t2 ? 2 : v <= t3 ? 3 : 4);

    // Align first column to Monday (Mon=0..Sun=6)
    const mondayOffset = (series[0].dow + 6) % 7;
    const cells = [];
    for (let i = 0; i < mondayOffset; i++) cells.push({ empty: true });
    for (const d of series) cells.push({ ...d, lv: lvOf(metricVal(d)) });
    while (cells.length % 7 !== 0) cells.push({ future: true });

    const numWeeks = cells.length / 7;
    const monthSpans = [];
    let lastM = -1;
    for (let w = 0; w < numWeeks; w++) {
      const c = (cells[w * 7] && !cells[w * 7].empty && !cells[w * 7].future)
        ? cells[w * 7]
        : cells.slice(w * 7, w * 7 + 7).find((x) => x.month);
      if (c && c.month !== lastM) {
        monthSpans.push(`<span>${c.month}月</span>`);
        lastM = c.month;
      } else {
        monthSpans.push('<span></span>');
      }
    }

    const activeDays = series.filter((d) => (d.calls || 0) > 0).length;
    let streak = 0;
    for (let i = series.length - 1; i >= 0; i--) {
      if ((series[i].calls || 0) > 0) streak++;
      else break;
    }
    let peak = null;
    for (const d of series) {
      if (!peak || metricVal(d) > metricVal(peak)) peak = d;
    }
    const peakV = peak ? metricVal(peak) : 0;
    const peakTxt = peakV <= 0 ? '—'
      : hmMetric === 'usd' ? fmtUsd(peakV)
        : hmMetric === 'tokens' ? fmtTok(peakV)
          : `${peakV} 次`;

    const html = `
      <div class="hm-scroll" id="hm-scroll">
        <div class="hm-layout">
          <div class="hm-wdays">
            <span></span><span>一</span><span></span><span>三</span><span></span><span>五</span><span></span><span>日</span>
          </div>
          <div class="hm-cols">
            <div class="hm-months" style="grid-template-columns:repeat(${numWeeks}, 12px)">${monthSpans.join('')}</div>
            <div class="hm-grid">
              ${cells.map((c) => {
                if (c.empty) return '<div class="hm-cell future" style="visibility:hidden"></div>';
                if (c.future) return '<div class="hm-cell future"></div>';
                return `<div class="hm-cell${c.lv ? ' lv' + c.lv : ''}${c.isToday ? ' today' : ''}" data-hm-date="${esc(c.date)}"></div>`;
              }).join('')}
            </div>
          </div>
        </div>
      </div>
      <div class="hm-foot">
        <span>近 ${numWeeks} 周活跃 <b>${activeDays}</b> 天 · 连续 <b>${streak}</b> 天 · 峰值 <b>${peakTxt}</b></span>
        <span class="hm-scale">
          <span>少</span>
          <i style="background:var(--hm-0)"></i>
          <i style="background:var(--hm-1)"></i>
          <i style="background:var(--hm-2)"></i>
          <i style="background:var(--hm-3)"></i>
          <i style="background:var(--hm-4)"></i>
          <span>多</span>
        </span>
      </div>`;
    return { html, dayMap };
  }

  function renderUsage() {
    hideHoverCard();
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
    const avgPct = avgVal > 0 ? Math.min(96, (avgVal / maxUsd) * 100) : 0;

    const acct = u.account;
    const acctTxt = acct ? `${acct.masked}${acct.tier ? ' · ' + acct.tier.replace(/^Google AI\s*/i, '') : ''}` : '每日用量统计';

    const barsHtml = barItems.map((d, idx) => {
      const showVal = (!isWeekly && barItems.length <= 7 && d.usd > 0) || (d.usd > 0 && (d.isToday || d.date === peakDate));
      const parts = (d.models || []).slice().reverse().map((m, mi, arr) => {
        const h = Math.max(2.5, (m.usd / maxUsd) * 100);
        const col = modelColor(m.name, legend);
        const isTop = mi === arr.length - 1;
        return `<div class="chart-part${isTop ? ' top' : ''}" data-day-idx="${idx}" data-model="${esc(m.name)}" style="height:${h.toFixed(2)}%;--mc:${col}"></div>`;
      }).join('');
      const valTag = showVal ? `<div class="chart-val">${fmtUsdShort(d.usd)}</div>` : '';
      return `<div class="chart-col" data-day-idx="${idx}" data-date="${esc(d.date)}"><div class="chart-stack">${parts}${valTag}</div></div>`;
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
        <td>${d.isToday ? '今天' : esc(d.date.slice(5))}${d.bf ? '<span class="bf-tag" title="从对话库补算">补</span>' : ''}${cliDot}</td>
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
          <span style="flex:none;font-weight:600">查看 →</span>
        </div>`;
    }

    const hmView = buildHeatmapView(heatmapData || (usageSpan === 'all' ? u : null));
    const hmLegend = (heatmapData || u)?.legend || legend;
    const forecastsHtml = renderForecastSubcards(u.forecasts || [], false);
    const spanLabel = usageSpan === 'all' ? `全部 ${u.spanDays || days.length} 天` : `近 ${u.span} 天`;
    const cliNote = u.hasCli ? ' · 含 agy CLI' : '';

    panelUsage.innerHTML = `
      <!-- Top Account Row -->
      <div class="usage-top">
        <span class="acct-badge" title="${esc(acctTxt)}">${esc(acctTxt)}</span>
        <button type="button" class="pill-btn sm" id="btn-usage-refresh" title="刷新用量与热力图">↻ 刷新</button>
      </div>

      <!-- Today Snapshot KPIs -->
      <div class="kpi-grid">
        <div class="kpi-card"><div class="kpi-label">今日花费</div><div class="kpi-val" style="color:var(--k-cost)">${fmtUsd(td.usd)}</div></div>
        <div class="kpi-card"><div class="kpi-label">今日调用</div><div class="kpi-val" style="color:var(--k-model)">${td.calls || 0}</div></div>
        <div class="kpi-card"><div class="kpi-label">今日 Token</div><div class="kpi-val" style="color:var(--k-tokens)">${fmtTok(td.tokens)}</div></div>
        <div class="kpi-card"><div class="kpi-label">缓存命中</div><div class="kpi-val" style="color:var(--k-cache)">${pct(td.hit, 0)}</div></div>
      </div>

      ${missBannerHtml}

      <!-- Section 1: Range-Scoped Stacked Bar Chart + Daily Table -->
      <div class="sec-title">
        <span>花费趋势与每日明细</span>
        <div class="pill-seg">
          <button type="button" class="${usageSpan === 7 ? 'on' : ''}" data-span="7">7天</button>
          <button type="button" class="${usageSpan === 30 ? 'on' : ''}" data-span="30">30天</button>
          <button type="button" class="${usageSpan === 'all' ? 'on' : ''}" data-span="all">全部</button>
        </div>
      </div>
      <div class="surface surface-pad surface-gap">
        <div class="period-stats">
          <span>${spanLabel} <b>${fmtUsd(totals.usd)}</b></span>
          <span>${isWeekly ? '周均' : '日均'} <b>${fmtUsd(avgVal)}</b></span>
          <span>调用 <b>${totals.calls || 0}</b></span>
          <span>Token <b>${fmtTok(totals.tokens)}</b></span>
        </div>

        <div class="chart-area">
          <div class="chart-grid-line" style="bottom:50%"></div>
          <div class="chart-bars ${barItems.length <= 10 ? 'wide' : ''}" id="usage-bars">
            ${barsHtml}
            ${avgPct > 0 ? `<div class="chart-avg" style="bottom:${avgPct.toFixed(1)}%"><span class="chart-avg-lbl">${isWeekly ? '周均' : '日均'} ${fmtUsdShort(avgVal)}</span></div>` : ''}
          </div>
        </div>
        <div class="chart-labels">${labelsHtml}</div>
        <div class="chart-legend">${legendHtml || '<span class="form-hint">暂无模型花费数据</span>'}</div>

        <div class="sub-tbl-wrap">
          <table class="u-table">
            <thead><tr><th>日期</th><th>花费</th><th>调用</th><th>Token</th><th>命中</th><th>周额度 Δ</th></tr></thead>
            <tbody>${tableRows}</tbody>
          </table>
        </div>
        <div class="form-hint" style="margin-top:6px">点击柱子或任意日期行查看当天逐次调用明细${isWeekly ? '（柱图按周合并）' : ''}${cliNote} · 价格：${esc(u.priceSource || 'bundled')}</div>
      </div>

      <!-- Section 2: Standalone Calendar Heatmap Card -->
      <div class="sec-title">
        <span>活跃度热力图</span>
        <div class="pill-seg">
          <button type="button" class="${hmMetric === 'usd' ? 'on' : ''}" data-hm="usd">花费</button>
          <button type="button" class="${hmMetric === 'tokens' ? 'on' : ''}" data-hm="tokens">Token</button>
          <button type="button" class="${hmMetric === 'calls' ? 'on' : ''}" data-hm="calls">调用</button>
        </div>
      </div>
      <div class="surface surface-pad surface-gap">
        ${hmView.html}
      </div>

      <!-- Section 3: Weekly Quota Forecasts -->
      ${forecastsHtml ? `
        <div class="sec-title"><span>每周额度预测</span></div>
        <div class="surface surface-pad"><div class="forecast-stack flush">${forecastsHtml}</div></div>` : ''}
    `;

    panelUsage.querySelectorAll('[data-span]').forEach((b) => {
      b.addEventListener('click', () => {
        usageSpan = b.dataset.span === 'all' ? 'all' : Number(b.dataset.span);
        if (usageSpan === 'all' && heatmapData) {
          usageData = heatmapData;
          renderUsage();
          return;
        }
        usageData = null;
        loadUsage();
      });
    });
    panelUsage.querySelectorAll('[data-hm]').forEach((b) => {
      b.addEventListener('click', () => { hmMetric = b.dataset.hm; renderUsage(); });
    });
    $('btn-usage-refresh')?.addEventListener('click', () => {
      loadUsage(false);
      if (usageSpan !== 'all') loadHeatmapData(true);
    });
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
        const rstHtml = d.resets?.length ? `<div style="color:var(--warning);margin-top:2px">${esc(formatResetTip(d.resets))}</div>` : '';
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

    // Anchor heatmap scroll to latest week + wire hover tooltip & click drill-down
    const hmScroll = $('hm-scroll');
    if (hmScroll) hmScroll.scrollLeft = hmScroll.scrollWidth;
    const wdays = ['周日', '周一', '周二', '周三', '周四', '周五', '周六'];
    panelUsage.querySelectorAll('[data-hm-date]').forEach((cell) => {
      const d = hmView.dayMap.get(cell.dataset.hmDate);
      if (!d) return;
      cell.addEventListener('mousemove', (ev) => {
        const rows = (d.models || []).slice(0, 5).map((m) =>
          `<div class="hc-row"><i style="--mc:${modelColor(m.name, hmLegend)}"></i><span>${esc(m.name)}</span><span class="val">${fmtUsd(m.usd)}</span></div>`
        ).join('');
        const rstHtml = d.resets?.length ? `<div style="color:var(--warning);margin-top:2px">${esc(formatResetTip(d.resets))}</div>` : '';
        showHoverCard(
          `<div class="hc-title">${esc(d.date)} (${wdays[d.dow] || ''}) · ${fmtUsd(d.usd)}</div>` +
          (rows || '<div class="dim">当日无调用</div>') +
          (d.calls ? `<div class="dim" style="margin-top:2px">${d.calls} 次 · ${fmtTok(d.tokens)} tok · 命中 ${pct(d.hit, 0)} · 点击看明细</div>` : '') +
          rstHtml,
          ev
        );
      });
      cell.addEventListener('mouseleave', hideHoverCard);
      cell.addEventListener('click', () => { if ((d.calls || 0) > 0) openUsageDay(d.date); });
    });
  }

  // ---------------------------------------------------------------- Theme tab
  const DEFAULT_LOOK = { palette: 'cometix', skin: 'plain', bars: 'thin', density: 'compact', icons: 'solid' };

  function renderTheme(p, force = false) {
    if (!window.__agyBar) return;
    if (!force && panelTheme.childElementCount) return;
    const c = activeConfig(p);
    const look = { ...DEFAULT_LOOK, ...(c.bar_look || {}) };
    const pals = window.__agyBar.PALETTES || [];
    const darkCls = isDarkTheme() ? ' dark' : '';

    const rowHtml = (label, key, opts) => `
      <div class="item">
        <div class="item-info"><div class="item-title">${label}</div></div>
        <div class="pill-seg">
          ${opts.map(([v, txt]) => `<button type="button" class="${look[key] === v ? 'on' : ''}" data-look-key="${key}" data-look-val="${v}">${txt}</button>`).join('')}
        </div>
      </div>`;

    panelTheme.innerHTML = `
      <div class="sec-title">
        <span>配色主题</span>
        <div class="btn-group">
          <span id="theme-save-state" class="save-state"></span>
          <button type="button" id="btn-look-reset" class="pill-btn sm">恢复默认</button>
        </div>
      </div>
      <div class="pal-grid">
        ${pals.map((pal) => `
          <button type="button" class="pal-tile ${look.palette === pal.id ? 'on' : ''}" data-pal-id="${esc(pal.id)}">
            <div class="pal-head">
              <span class="pal-name">${esc(pal.name)}</span>
              <span class="pal-tag">${look.palette === pal.id ? '使用中' : esc(pal.desc)}</span>
            </div>
            <div class="pal-dots agy-statusline-bar${darkCls}" data-pal="${esc(pal.id)}">
              <span style="background:var(--k-model)"></span>
              <span style="background:var(--k-context)"></span>
              <span style="background:var(--k-cache)"></span>
              <span style="background:var(--k-quota)"></span>
              <span style="background:var(--k-cost)"></span>
              <span style="background:var(--k-git)"></span>
            </div>
          </button>`).join('')}
      </div>

      <div class="sec-title"><span>外形与细节</span></div>
      <div class="surface">
        ${rowHtml('外形样式', 'skin', [['plain', 'Plain'], ['pill', 'Pill'], ['powerline', 'Powerline']])}
        ${rowHtml('额度进度条', 'bars', [['thin', '细条'], ['cells', '10格'], ['ring', '圆环'], ['none', '隐藏']])}
        ${rowHtml('信息密度', 'density', [['compact', '紧凑'], ['full', '完整']])}
        ${rowHtml('图标样式', 'icons', [['solid', '实心'], ['outline', '线条'], ['unicode', '字符'], ['off', '关闭']])}
      </div>
      <div class="form-hint" style="margin:8px 4px 0">所有配色均含浅色与深色两版，随 Antigravity 明暗主题自动切换；点选后立刻反映在顶部状态栏预览并自动同步到所有窗口。</div>
    `;

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

  // ---------------------------------------------------------------- Settings tab (Antigravity Tonal Quiet)
  const SEGMENTS = [
    ['model', '模型'], ['git', 'Git'], ['context', '上下文'], ['cache', '缓存命中'],
    ['cost', '对话花费'], ['today', '今日花费'], ['ttft', '首字延迟'], ['speed', '输出速度'],
    ['quota_5h', '5 小时额度'], ['quota_7d', '7 天额度'], ['running', '运行状态'], ['subagents', '子代理行'],
  ];
  const CTX_PRESETS = [[0, '自动'], [200000, '200K'], [256000, '256K'], [512000, '512K'], [1000000, '1M']];
  const CHECK_SVG = '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" aria-hidden="true"><path d="M5 13l4 4L19 7"/></svg>';

  function readSettingsForm() {
    const seg = {};
    for (const [k] of SEGMENTS) {
      const chip = panelSettings.querySelector(`[data-seg="${k}"]`);
      seg[k] = chip ? chip.getAttribute('aria-pressed') === 'true' : true;
    }
    const rlActive = panelSettings.querySelector('[data-rl].on');
    const ctxActive = panelSettings.querySelector('[data-ctx].on');
    const ctxSel = $('cfg-ctx-win-custom');
    const ctxVal = ctxSel ? Number(ctxSel.value) || 0 : (ctxActive ? Number(ctxActive.dataset.ctx) || 0 : 0);
    return {
      bar_enabled: $('cfg-bar-on').checked,
      bar_segments: seg,
      ui_zh: $('cfg-zh').checked,
      relaunch_mode: rlActive ? rlActive.dataset.rl : 'never',
      online_updates: $('cfg-online-upd').checked,
      online_data_sync: $('cfg-data-sync').checked,
      context_limit_override: ctxVal,
      cache_ttl_sec: Math.max(30, Number($('cfg-ttl').value) || 300),
      costly_call_usd: Math.max(0, Number($('cfg-costly').value) || 0),
      include_subagents: $('cfg-sub').checked,
    };
  }

  function onSettingsChange() {
    const patch = readSettingsForm();
    panelSettings.querySelector('.chip-cloud')?.classList.toggle('off', !patch.bar_enabled);
    const enBtn = $('btn-toggle-en');
    if (enBtn) enBtn.disabled = !patch.ui_zh || !(liveStatus?.debug ?? true);
    scheduleSave(patch);
  }

  function tipBtn(id) {
    return `<button type="button" class="tip-btn ${openTip === id ? 'open' : ''}" data-tip="${id}" title="查看说明" aria-expanded="${openTip === id}">?</button>`;
  }

  function renderSettings(p, force = false) {
    if (!force && panelSettings.childElementCount) {
      updateLiveStatus();
      renderUpdateCenter();
      return;
    }
    const c = activeConfig(p);
    const sg = c.bar_segments || {};
    const rlMode = c.relaunch_mode || 'never';
    const ctxVal = Number(c.context_limit_override) || 0;
    const isPresetCtx = CTX_PRESETS.some(([v]) => v === ctxVal);
    const shortcutKey = /Mac/i.test(navigator.platform || navigator.userAgent) ? 'Ctrl+Option+E' : 'Ctrl+Alt+E';

    const ctxControlHtml = isPresetCtx
      ? `<div class="pill-seg">
          ${CTX_PRESETS.map(([v, label]) => `<button type="button" data-ctx="${v}" class="${ctxVal === v ? 'on' : ''}">${label}</button>`).join('')}
        </div>`
      : `<select id="cfg-ctx-win-custom" class="t-select">
          ${CTX_PRESETS.map(([v, label]) => `<option value="${v}">${label}</option>`).join('')}
          <option value="${ctxVal}" selected>${fmtTok(ctxVal)} (自定义)</option>
        </select>`;

    panelSettings.innerHTML = `
      <!-- Top Unified Health & Update Banner -->
      <div id="health-banner-wrap"></div>

      <!-- Section 1: Statusline Bar -->
      <div class="sec-title">
        <span>输入框状态栏</span>
        <span id="save-state" class="save-state">自动保存</span>
      </div>
      <div class="surface">
        <div class="item">
          <div class="item-info">
            <div class="item-title">在输入框上方显示状态栏 ${tipBtn('bar')}</div>
            <div class="item-sub">配色与外形样式请在「外观」页切换</div>
          </div>
          <label class="m3-sw">
            <input id="cfg-bar-on" type="checkbox" ${c.bar_enabled !== false ? 'checked' : ''} />
            <span></span>
          </label>
        </div>
        ${openTip === 'bar' ? '<div class="tip-drawer">状态栏通过本地调试端口注入至对话输入框上方。点击下方任意字段标签立刻反映在顶部预览并自动保存到 config.toml。</div>' : ''}
        <div class="chip-cloud ${c.bar_enabled !== false ? '' : 'off'}">
          ${SEGMENTS.map(([k, label]) => {
            const on = sg[k] !== false;
            return `<button type="button" class="t-chip ${on ? 'on' : ''}" data-seg="${k}" aria-pressed="${on}">${on ? CHECK_SVG : ''}<span>${label}</span></button>`;
          }).join('')}
        </div>
      </div>

      <!-- Section 2: Chinese UI & Debug Launch -->
      <div class="sec-title"><span>语言与调试启动</span></div>
      <div class="surface">
        <div class="item">
          <div class="item-info">
            <div class="item-title">中文界面（含 Swarm） ${tipBtn('zh')}</div>
            <div class="item-sub">即时生效，不动对话内容与代码</div>
          </div>
          <label class="m3-sw">
            <input id="cfg-zh" type="checkbox" ${c.ui_zh ? 'checked' : ''} />
            <span></span>
          </label>
        </div>
        ${openTip === 'zh' ? '<div class="tip-drawer">将 Antigravity 主界面与 Swarm 面板文案实时译为中文，词典来源于 agy-zh-pack。打开或关闭均立刻生效，无需刷新窗口。</div>' : ''}

        <div class="item">
          <div class="item-info">
            <div class="item-title">临时切回英文</div>
            <div class="item-sub">快捷键 ${shortcutKey}，不改配置、刷新恢复中文</div>
          </div>
          <button id="btn-toggle-en" type="button" class="pill-btn" ${c.ui_zh ? '' : 'disabled'}>${liveStatus?.zhPaused ? '切回中文' : '临时英文'}</button>
        </div>

        <div class="item">
          <div class="item-info">
            <div class="item-title">未带调试端口启动时 ${tipBtn('rl')}</div>
            <div class="item-sub">启动检测策略</div>
          </div>
          <div class="pill-seg">
            ${[['never', '不处理'], ['ask', '询问'], ['auto', '自动']].map(([v, label]) =>
              `<button type="button" data-rl="${v}" class="${rlMode === v ? 'on' : ''}">${label}</button>`
            ).join('')}
          </div>
        </div>
        ${openTip === 'rl' ? '<div class="tip-drawer">先核验主进程命令行确实未带 --remote-debugging-port 再触发，防止端口短暂繁忙时误重启 Antigravity。</div>' : ''}

        <div class="item">
          <div class="item-info">
            <div class="item-title">以调试模式重启 Antigravity</div>
            <div class="item-sub" id="restart-hint">${restartPending ? '正在退出 Antigravity，稍后自动重新打开…' : '关闭所有窗口并附带调试端口重新打开'}</div>
          </div>
          <button id="btn-restart" type="button" class="pill-btn err" ${restartPending ? 'disabled' : ''}>${restartPending ? '重启中…' : '重启'}</button>
        </div>
      </div>

      <!-- Section 3: Updates & Online Data Sync -->
      <div class="sec-title"><span>更新与在线数据</span></div>
      <div class="surface">
        <div class="item">
          <div class="item-info">
            <div class="item-title">自动检查插件更新</div>
            <div class="item-sub" id="plugin-update-summary">加载版本信息中…</div>
          </div>
          <div class="btn-group">
            <span id="plugin-update-btns" class="btn-group"></span>
            <label class="m3-sw">
              <input id="cfg-online-upd" type="checkbox" ${c.online_updates !== false ? 'checked' : ''} />
              <span></span>
            </label>
          </div>
        </div>
        <div id="plugin-update-detail" class="update-detail-line hidden"></div>

        <div class="item">
          <div class="item-info">
            <div class="item-title">在线同步词典与价格表</div>
            <div class="item-sub" id="data-sync-summary">加载同步状态中…</div>
          </div>
          <div class="btn-group">
            <button type="button" id="btn-sync-data" class="pill-btn sm">同步</button>
            <label class="m3-sw">
              <input id="cfg-data-sync" type="checkbox" ${c.online_data_sync !== false ? 'checked' : ''} />
              <span></span>
            </label>
          </div>
        </div>
        <div id="data-sync-detail" class="update-detail-line hidden"></div>
      </div>

      <!-- Section 4: Calculation Config (config.toml) -->
      <div class="sec-title"><span>计算口径</span><span class="sec-meta">config.toml</span></div>
      <div class="surface">
        <div class="item">
          <div class="item-info">
            <div class="item-title">上下文上限</div>
            <div class="item-sub">默认使用后台 maxContextTokens</div>
          </div>
          ${ctxControlHtml}
        </div>
        <div class="item">
          <div class="item-info">
            <div class="item-title">Prompt 缓存 TTL（秒）</div>
            <div class="item-sub">概览页「缓存热度」倒计时</div>
          </div>
          <input id="cfg-ttl" type="number" step="30" min="30" class="t-input" value="${c.cache_ttl_sec || 300}" />
        </div>
        <div class="item">
          <div class="item-info">
            <div class="item-title">高价调用提醒阈值（$）</div>
            <div class="item-sub">达标在状态栏与明细标红，填 0 关闭</div>
          </div>
          <input id="cfg-costly" type="number" step="0.1" min="0" class="t-input" value="${c.costly_call_usd ?? 0.5}" />
        </div>
        <div class="item">
          <div class="item-info">
            <div class="item-title">总计包含子代理</div>
            <div class="item-sub">将子代理 Token 与费用计入主对话总计</div>
          </div>
          <label class="m3-sw">
            <input id="cfg-sub" type="checkbox" ${c.include_subagents !== false ? 'checked' : ''} />
            <span></span>
          </label>
        </div>
      </div>
      <div class="form-hint" style="margin:8px 4px 0">模型单价（含 Claude 缓存写入与分时调价）可在 config.toml 的 [[models]] 中自定义。</div>
    `;

    panelSettings.querySelectorAll('[data-tip]').forEach((b) => {
      b.addEventListener('click', () => {
        openTip = openTip === b.dataset.tip ? '' : b.dataset.tip;
        if (last) renderSettings(last, true);
      });
    });
    panelSettings.querySelectorAll('[data-seg]').forEach((chip) => {
      chip.addEventListener('click', () => {
        const next = chip.getAttribute('aria-pressed') !== 'true';
        chip.setAttribute('aria-pressed', String(next));
        chip.classList.toggle('on', next);
        const label = chip.querySelector('span')?.textContent || '';
        chip.innerHTML = `${next ? CHECK_SVG : ''}<span>${esc(label)}</span>`;
        onSettingsChange();
      });
    });
    panelSettings.querySelectorAll('[data-rl]').forEach((b) => {
      b.addEventListener('click', () => {
        panelSettings.querySelectorAll('[data-rl]').forEach((x) => x.classList.toggle('on', x === b));
        onSettingsChange();
      });
    });
    panelSettings.querySelectorAll('[data-ctx]').forEach((b) => {
      b.addEventListener('click', () => {
        panelSettings.querySelectorAll('[data-ctx]').forEach((x) => x.classList.toggle('on', x === b));
        onSettingsChange();
      });
    });
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
    $('btn-sync-data')?.addEventListener('click', onSyncDataClick);

    renderHealthBanner();
    renderUpdateCenter();
    updateLiveStatus();
    if (!updateStatus) refreshUpdateStatus();
  }

  function renderHealthBanner() {
    const wrap = $('health-banner-wrap');
    if (!wrap) return;
    const j = liveStatus;
    const s = updateStatus;
    const rel = s?.release || {};
    const debugOk = j ? !!j.debug : true;
    const title = !j
      ? '检测连接状态中…'
      : debugOk
        ? `调试端口已连接${j.zhPaused ? '（当前临时英文）' : ''}`
        : '未连上调试端口：状态栏和中文界面需要调试模式启动';
    let sub = '加载版本信息中…';
    if (s) {
      sub = s.available
        ? `v${s.current} → 发现新版本 v${rel.latest}`
        : `当前 v${s.current}${rel.latest ? ' · 已是最新' : ''}`;
    }
    const ico = debugOk
      ? '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M5 13l4 4L19 7"/></svg>'
      : '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M12 9v4m0 4h.01M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/></svg>';

    const quickUpdBtn = s?.available && s?.canSelfUpdate
      ? `<button type="button" class="pill-btn sm pri" id="btn-banner-upd" ${s.installing ? 'disabled' : ''}>${s.installing ? '更新中…' : '更新 v' + esc(rel.latest)}</button>`
      : '';

    let diagHtml = '';
    if (diagOpen) {
      const rawFlag = j?.relaunch?.lastFlag || '';
      const flag = (rawFlag && (!j?.debug || rawFlag === 'flag')) ? ` · 进程校验: ${esc(rawFlag)}` : '';
      const swarm = j?.swarmFrames ? ` · Swarm 面板: ${j.swarmFrames}` : '';
      const panes = j?.panes?.length ? `<div>showing: ${j.panes.map((x) => esc(String(x).slice(0, 8))).join(', ')}</div>` : '';
      diagHtml = `
        <div class="diag-card">
          <div class="diag-head">
            <span>连接诊断信息</span>
            <button type="button" class="pill-btn sm err" id="btn-diag-restart" ${restartPending ? 'disabled' : ''}>${restartPending ? '重启中…' : '以调试模式重启'}</button>
          </div>
          <div>CDP: ${esc(j?.cdp || '—')}${swarm}${flag}</div>
          <div>language_server: ${esc(j?.ls || '—')}</div>
          ${panes}
        </div>`;
    }

    wrap.innerHTML = `
      <div class="health-banner ${debugOk ? '' : 'warn'}">
        <div class="health-banner-l">
          <div class="health-ico">${ico}</div>
          <div style="min-width:0">
            <div class="health-title">${esc(title)}</div>
            <div class="health-sub">${esc(sub)}</div>
          </div>
        </div>
        <div class="btn-group">
          <button type="button" class="pill-btn sm" id="btn-toggle-diag" style="background:color-mix(in srgb,currentColor 15%,transparent);color:inherit">${diagOpen ? '收起诊断' : '诊断'}</button>
          ${quickUpdBtn}
        </div>
      </div>
      ${diagHtml}
    `;

    $('btn-toggle-diag')?.addEventListener('click', () => { diagOpen = !diagOpen; renderHealthBanner(); });
    $('btn-diag-restart')?.addEventListener('click', onRestart);
    $('btn-banner-upd')?.addEventListener('click', onInstallUpdateClick);
  }

  // ---------------------------------------------------------------- Update Center logic
  async function refreshUpdateStatus() {
    try {
      const res = await fetch('/api/update/status', { cache: 'no-store' });
      if (!res.ok) return;
      updateStatus = await res.json();
      updateDot?.classList.toggle('hidden', !updateStatus.available);
      if (activeTab === 'settings') {
        renderHealthBanner();
        renderUpdateCenter();
      }
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

  async function onCheckUpdateClick() {
    const btn = $('btn-check-upd');
    if (btn) { btn.disabled = true; btn.textContent = '检查中…'; }
    try {
      await postJson('/api/update/check', {});
    } catch (e) {}
    await refreshUpdateStatus();
  }

  async function onInstallUpdateClick() {
    for (const id of ['btn-install-upd', 'btn-banner-upd']) {
      const b = $(id);
      if (b) { b.disabled = true; b.textContent = '启动更新…'; }
    }
    try {
      const res = await postJson('/api/update/install', {});
      const j = await res.json().catch(() => ({}));
      if (!res.ok) alert(j.error || '更新失败');
    } catch (e) {
      alert('更新失败：' + e.message);
    }
    await refreshUpdateStatus();
  }

  async function onSyncDataClick() {
    const btn = $('btn-sync-data');
    if (btn) { btn.disabled = true; btn.textContent = '同步中…'; }
    try {
      await postJson('/api/data-sync', {});
    } catch (e) {}
    await refreshUpdateStatus();
    lastJson = '';
    poll();
  }

  function renderUpdateCenter() {
    const pSum = $('plugin-update-summary');
    const pBtns = $('plugin-update-btns');
    const pDet = $('plugin-update-detail');
    const dSum = $('data-sync-summary');
    const dBtn = $('btn-sync-data');
    const dDet = $('data-sync-detail');
    if (!pSum || !dSum) return;
    if (!updateStatus) {
      pSum.textContent = '加载版本信息中…';
      dSum.textContent = '加载同步状态中…';
      return;
    }
    const s = updateStatus;
    const rel = s.release || {};
    const inst = s.install || {};
    const ds = s.dataSync || {};

    let verLine = `当前 v${esc(s.current)}`;
    if (s.available) verLine += ` · <span class="ok">发现 v${esc(rel.latest)}</span>`;
    else if (rel.latest) verLine += ' · 已是最新';
    verLine += `（${fmtAgo(rel.checked_at)}检查）`;
    pSum.innerHTML = verLine;

    let btnsHtml = `<button type="button" id="btn-check-upd" class="pill-btn sm" ${s.checking || s.installing ? 'disabled' : ''}>${s.checking ? '检查中…' : '检查'}</button>`;
    if (s.available && s.canSelfUpdate) {
      btnsHtml += `<button type="button" id="btn-install-upd" class="pill-btn sm pri" ${s.installing ? 'disabled' : ''}>${s.installing ? '更新中…' : '更新'}</button>`;
    }
    pBtns.innerHTML = btnsHtml;

    const pNotes = [];
    if (inst.status && inst.status !== 'idle') {
      const map = {
        downloading: '正在下载安装包…',
        verifying: '正在校验 SHA256SUMS…',
        installing: '正在备份并替换插件文件…',
        installed: `已安装 v${esc(inst.version)}，sidecar 将自动重启生效`,
        failed: `更新失败：${esc(inst.error || '未知错误')}`,
      };
      pNotes.push(`<div class="${inst.status === 'failed' ? 'err' : 'ok'}">${map[inst.status] || esc(inst.status)}</div>`);
    }
    if (s.available && s.devCheckout) {
      pNotes.push('<div class="bad">当前运行在开发仓库目录（含 .git），请用 git pull &amp;&amp; cargo build 更新</div>');
    }
    if (rel.error) {
      pNotes.push(`<div class="err">检查失败：${esc(rel.error)}</div>`);
    }
    if (rel.html_url) {
      pNotes.push(`<div><a href="${esc(rel.html_url)}" target="_blank" rel="noreferrer">查看 GitHub Release Notes ↗</a></div>`);
    }
    pDet.innerHTML = pNotes.join('');
    pDet.classList.toggle('hidden', pNotes.length === 0);

    dSum.textContent = ds.synced_at
      ? `词典 ${ds.dict_entries} 条 · 规则 ${ds.rule_entries} 条 · ${ds.prices_models || 0} 模型 · ${fmtAgo(ds.synced_at)}同步`
      : '尚未在线同步（当前使用内置词典与价格表）';
    if (dBtn) {
      dBtn.disabled = !!s.syncingData;
      dBtn.textContent = s.syncingData ? '同步中…' : '同步';
    }
    const dNotes = [];
    if (ds.error) dNotes.push(`<div class="err">同步失败：${esc(ds.error)}</div>`);
    if (ds.synced_at) dNotes.push(`<div>${esc(ds.source_host)} @ ${esc(ds.commit)} · 价格表 ${esc(ds.prices_updated || '—')}</div>`);
    dDet.innerHTML = dNotes.join('');
    dDet.classList.toggle('hidden', dNotes.length === 0);

    $('btn-check-upd')?.addEventListener('click', onCheckUpdateClick);
    $('btn-install-upd')?.addEventListener('click', onInstallUpdateClick);
  }

  // ---------------------------------------------------------------- confirm modal & debug relaunch
  let confirmResolver = null;
  function openConfirmModal({ title, bodyHtml, okText = '确认', cancelText = '取消', danger = false, focusCancel = true }) {
    const modal = $('confirm-modal');
    if (!modal) return Promise.resolve(false);
    if (confirmResolver) {
      confirmResolver(false);
      confirmResolver = null;
    }
    $('confirm-modal-title').textContent = title;
    $('confirm-modal-body').innerHTML = bodyHtml;
    const cancelBtn = $('confirm-modal-cancel');
    const okBtn = $('confirm-modal-ok');
    cancelBtn.textContent = cancelText;
    okBtn.textContent = okText;
    okBtn.className = 'pill-btn ' + (danger ? 'err-fill' : 'pri');
    modal.classList.remove('hidden');

    return new Promise((resolve) => {
      const finish = (accepted) => {
        if (!confirmResolver) return;
        confirmResolver = null;
        modal.classList.add('hidden');
        cancelBtn.removeEventListener('click', onCancel);
        okBtn.removeEventListener('click', onOk);
        modal.removeEventListener('click', onBackdrop);
        document.removeEventListener('keydown', onKey);
        resolve(accepted);
      };
      const onCancel = () => finish(false);
      const onOk = () => finish(true);
      const onBackdrop = (e) => { if (e.target === modal) finish(false); };
      const onKey = (e) => {
        if (e.key === 'Escape') {
          e.preventDefault();
          finish(false);
        }
      };
      confirmResolver = finish;
      cancelBtn.addEventListener('click', onCancel);
      okBtn.addEventListener('click', onOk);
      modal.addEventListener('click', onBackdrop);
      document.addEventListener('keydown', onKey);
      (focusCancel ? cancelBtn : okBtn).focus();
    });
  }

  let restartChecking = false;
  let restartPending = false;
  let restartTimeout = 0;
  function resetRestartUi(message = '', isErr = false) {
    clearTimeout(restartTimeout);
    restartTimeout = 0;
    restartPending = false;
    const btn = $('btn-restart');
    const diagBtn = $('btn-diag-restart');
    const hint = $('restart-hint');
    if (btn) { btn.disabled = false; btn.textContent = '重启'; }
    if (diagBtn) { diagBtn.disabled = false; diagBtn.textContent = '以调试模式重启'; }
    if (hint) {
      hint.textContent = message || '关闭所有窗口并附带调试端口重新打开';
      hint.classList.toggle('err', !!isErr);
    }
  }

  async function onRestart() {
    if (restartPending || restartChecking) return;
    restartChecking = true;
    const btn = $('btn-restart');
    if (btn) btn.disabled = true;
    let running = 0;
    try {
      const status = await (await fetch('/api/live-status', { cache: 'no-store' })).json();
      running = Number(status.running) || 0;
    } catch (e) {} finally {
      restartChecking = false;
      const curBtn = $('btn-restart');
      if (curBtn && !restartPending) curBtn.disabled = false;
    }

    const bodyHtml =
      '<div>将关闭当前所有 Antigravity 窗口，并附带 <code>--remote-debugging-port=0</code> 重新打开。对话记录会保留。</div>' +
      (running > 0 ? `<div class="modal-warn">当前有 ${running} 个对话正在运行，重启会中断正在进行的任务。</div>` : '');

    const ok = await openConfirmModal({
      title: '以调试模式重启 Antigravity？',
      bodyHtml,
      okText: '确认重启',
      cancelText: '取消',
      danger: true,
      focusCancel: running > 0,
    });
    if (!ok) {
      $('btn-restart')?.focus();
      return;
    }

    restartPending = true;
    for (const id of ['btn-restart', 'btn-diag-restart']) {
      const b = $(id);
      if (b) { b.disabled = true; b.textContent = '重启中…'; }
    }
    const hint = $('restart-hint');
    if (hint) {
      hint.classList.remove('err');
      hint.textContent = '正在退出 Antigravity，稍后自动重新打开…';
    }
    try {
      const res = await postJson('/api/restart-debug', {});
      const j = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(j.error || 'HTTP ' + res.status);
      restartTimeout = setTimeout(() => {
        resetRestartUi('重启超时：Antigravity 未退出，请检查是否有未保存确认窗或查看 relaunch.log', true);
      }, 20000);
    } catch (e) {
      resetRestartUi('重启失败：' + e.message, true);
    }
  }

  async function updateLiveStatus() {
    const enBtn = $('btn-toggle-en');
    const wrap = $('health-banner-wrap');
    if (!enBtn && !wrap) return;
    try {
      const j = await (await fetch('/api/live-status', { cache: 'no-store' })).json();
      liveStatus = j;
      if (enBtn) {
        const zhOn = $('cfg-zh') ? $('cfg-zh').checked : !!j.uiZh;
        enBtn.disabled = !zhOn || !j.debug;
        enBtn.textContent = j.zhPaused ? '切回中文' : '临时英文';
      }
      renderHealthBanner();
    } catch (e) {
      if (wrap) wrap.innerHTML = '<div class="diag-card">live status unavailable</div>';
    }
  }

  // ---------------------------------------------------------------- main poll loop
  function renderPanels(p, force = false) {
    const turnsTotal = p.turnsTotal ?? p.turns?.length ?? 0;
    turnsCountEl.textContent = turnsTotal ? String(turnsTotal) : '';
    if (activeTab === 'overview') renderOverview(p);
    else if (activeTab === 'turns') renderTurns(p);
    else if (activeTab === 'usage') {
      if (!usageData) loadUsage();
      else if (force) renderUsage();
    }
    else if (activeTab === 'theme') renderTheme(p, force);
    else if (activeTab === 'settings') renderSettings(p, force);
  }

  function renderEmpty(msg) {
    stripEl.replaceChildren();
    panelOverview.innerHTML = `<div class="empty-msg">${esc(msg)}</div>`;
  }

  async function pollOnce() {
    try {
      const convId = selectedConvId;
      const qs = new URLSearchParams();
      if (convId) qs.set('conversationId', convId);
      const res = await fetch(`/api/panel?${qs}`, { cache: 'no-store' });
      if (!res.ok) return;
      const text = await res.text();
      if (convId !== selectedConvId) return;
      if (text === lastJson) return;
      lastJson = text;
      const p = JSON.parse(text);
      if (!p.id) { renderEmpty(p.error || '还没有对话'); return; }
      last = p;

      const recents = p.recent || [];
      const autoLabel = (hostConvId ? '当前窗格 · ' : '自动 · ') + (recents[0]?.title || '');
      convSelect.innerHTML = `<option value="${esc(hostConvId)}">${esc(autoLabel)}</option>` +
        recents.filter((c) => c.id !== hostConvId).map((c) =>
          `<option value="${esc(c.id)}" ${c.id === selectedConvId ? 'selected' : ''}>${esc(c.title)}</option>`).join('');

      livePill.classList.toggle('running', !!p.running);
      liveLabel.textContent = p.activity || (p.running ? '运行中' : '空闲');

      renderStrip(p);
      renderPanels(p, false);
    } catch (err) {
      console.error('statusline poll failed:', err);
    }
  }

  let pollRun = null;
  let pollQueued = null;
  let pollTimer = 0;
  function poll() {
    if (pollRun) {
      pollQueued ??= pollRun.then(() => { pollQueued = null; return poll(); });
      return pollQueued;
    }
    clearTimeout(pollTimer);
    pollTimer = 0;
    pollRun = pollOnce().finally(() => {
      pollRun = null;
      pollTimer = setTimeout(() => {
        pollTimer = 0;
        if (!document.hidden) poll();
      }, 1500);
    });
    return pollRun;
  }
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) poll();
  });

  poll();
  refreshUpdateStatus();
  // Preload full history once in background so Overview gets weekly quota forecasts immediately
  // and Usage's standalone heatmap is ready without blocking initial paint.
  loadHeatmapData(true);
  setInterval(refreshUpdateStatus, 60000);
  setInterval(() => {
    if (!document.hidden) {
      if (activeTab === 'usage' && !usageDayDate) {
        loadUsage(true);
        if (usageSpan !== 'all') loadHeatmapData(true);
      } else if (activeTab === 'overview') {
        loadHeatmapData(true);
      }
    }
  }, 30000);
})();
