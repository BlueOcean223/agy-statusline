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
  let workspaceUris = [];
  let activeTab = 'overview';
  let settingsDirty = false;
  let lastEtag = '';
  let lastSnap = null;

  if (window.sidecar?.onWorkspaceChange) {
    window.sidecar.onWorkspaceChange((uris) => {
      workspaceUris = Array.isArray(uris) ? uris : [];
    });
  }

  // Sync light/dark scheme from Antigravity host's --background luminance
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
      const lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
      const theme = lum < 0.5 ? 'dark' : 'light';
      if (document.documentElement.dataset.theme !== theme) {
        document.documentElement.dataset.theme = theme;
      }
    }
  }
  syncScheme();
  window.addEventListener('message', (e) => {
    if (e.data?.type === 'theme-change') requestAnimationFrame(syncScheme);
  });

  // Tab switching
  for (const btn of document.querySelectorAll('.tab')) {
    btn.addEventListener('click', () => {
      activeTab = btn.dataset.tab;
      for (const b of document.querySelectorAll('.tab')) b.classList.toggle('active', b === btn);
      panelOverview.classList.toggle('hidden', activeTab !== 'overview');
      panelTurns.classList.toggle('hidden', activeTab !== 'turns');
      panelSettings.classList.toggle('hidden', activeTab !== 'settings');
      settingsDirty = false;
      if (lastSnap) renderPanels(lastSnap, true);
    });
  }

  convSelect.addEventListener('change', () => {
    selectedConvId = convSelect.value;
    lastEtag = '';
    poll();
  });

  // Crisp inline 13px SVG icons
  const IC = {
    model: '<svg viewBox="0 0 16 16"><path d="M8 2l1.6 3.6L13.2 7l-3.6 1.4L8 12l-1.6-3.6L2.8 7l3.6-1.4L8 2z"/><circle cx="12.5" cy="12.5" r="1"/></svg>',
    git: '<svg viewBox="0 0 16 16"><circle cx="4.5" cy="4" r="1.7"/><circle cx="4.5" cy="12" r="1.7"/><circle cx="11.5" cy="6" r="1.7"/><path d="M4.5 5.7v4.6M11.5 7.7c0 2.2-2 2.5-7 2.6"/></svg>',
    ctx: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.5" stroke-opacity="0.3"/><path d="M8 2.5A5.5 5.5 0 112.5 8"/></svg>',
    cache: '<svg viewBox="0 0 16 16"><ellipse cx="8" cy="4.5" rx="5" ry="2.1"/><path d="M3 4.5v3.5c0 1.2 2.2 2.1 5 2.1s5-.9 5-2.1V4.5"/><path d="M3 8v3.5c0 1.2 2.2 2.1 5 2.1s5-.9 5-2.1V8"/></svg>',
    tokens: '<svg viewBox="0 0 16 16"><path d="M4 4h8M4 8h8M4 12h5"/></svg>',
    cost: '<svg viewBox="0 0 16 16"><path d="M8 2v12M10.8 4.8H6.6a1.8 1.8 0 000 3.6h2.8a1.8 1.8 0 010 3.6H5"/></svg>',
    today: '<svg viewBox="0 0 16 16"><rect x="2.5" y="3.5" width="11" height="10" rx="1.8"/><path d="M5.5 2v2.5M10.5 2v2.5M2.5 6.8h11"/></svg>',
    tps: '<svg viewBox="0 0 16 16"><path d="M3.2 12a6 6 0 119.6 0"/><path d="M8 8.5L11 5.5"/><circle cx="8" cy="8.5" r="1" fill="currentColor"/></svg>',
    wait: '<svg viewBox="0 0 16 16"><path d="M5 2.5h6M5 13.5h6M5.5 2.5c0 3 2.5 4 2.5 5.5s-2.5 2.5-2.5 5.5M10.5 2.5c0 3-2.5 4-2.5 5.5s2.5 2.5 2.5 5.5"/></svg>',
    q5: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.3l2.3 1.4"/></svg>',
    q7: '<svg viewBox="0 0 16 16"><rect x="2.5" y="3.5" width="11" height="10" rx="1.8"/><path d="M2.5 6.8h11"/></svg>',
  };

  function esc(s) {
    return String(s ?? '')
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function fmtTok(n) {
    const v = Number(n) || 0;
    if (v >= 1e6) return (v / 1e6).toFixed(v >= 1e7 ? 1 : 2).replace(/\.?0+$/, '') + 'M';
    if (v >= 1e3) return (v / 1e3).toFixed(v >= 1e5 ? 0 : 1).replace(/\.0$/, '') + 'k';
    return String(Math.round(v));
  }

  function fmtUsd(n) {
    const v = Number(n) || 0;
    if (v === 0) return '$0.00';
    if (v < 0.01) return '$' + v.toFixed(4);
    if (v < 1) return '$' + v.toFixed(3);
    return '$' + v.toFixed(2);
  }

  function cellsHtml(ratio, total = 8) {
    const on = Math.round(Math.max(0, Math.min(1, ratio)) * total);
    let out = '<span class="cc-cells">';
    for (let i = 0; i < total; i++) out += `<i class="${i < on ? 'on' : ''}"></i>`;
    return out + '</span>';
  }

  function paceBarHtml(usedRatio, elapsedRatio) {
    const u = (Math.max(0, Math.min(1, usedRatio)) * 100).toFixed(1);
    const e = (Math.max(0, Math.min(1, elapsedRatio)) * 100).toFixed(1);
    return `<span class="cc-pace"><span class="cc-pace-row"><span class="cc-pace-fill" style="width:${u}%"></span></span><span class="cc-pace-row clock"><span class="cc-pace-fill" style="width:${e}%"></span></span></span>`;
  }

  function segHtml(colorVar, iconSvg, bodyHtml, tooltip, extraCls = '') {
    return `<span class="cc-seg ${extraCls}" style="--kc:var(${colorVar})" title="${esc(tooltip)}"><span class="cc-ic">${iconSvg}</span>${bodyHtml}</span>`;
  }

  function sparkSvg(series, colorCss) {
    if (!Array.isArray(series) || series.length < 2) return '';
    const w = 180;
    const h = 22;
    const min = Math.min(...series);
    const max = Math.max(...series);
    const span = Math.max(1, max - min);
    const pts = series
      .map((v, i) => {
        const x = (i / (series.length - 1)) * w;
        const y = h - 2 - ((v - min) / span) * (h - 4);
        return `${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(' ');
    return `<svg class="spark-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><polyline fill="none" stroke="${colorCss}" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" points="${pts}"/></svg>`;
  }

  // Render the flush 2-line CCometixLine strip in the header
  function renderStrip(snap) {
    const s = snap.summary;
    const line1 = [];
    const line2 = [];

    // 1. Model + Effort
    const modelLabel = s.effort ? `${esc(s.model_short)}<span class="cc-dot">·</span><span class="cc-sub">${esc(s.effort)}</span>` : esc(s.model_short);
    line1.push(segHtml('--k-model', IC.model, modelLabel, `模型: ${s.model_name}${s.effort ? ` (${s.effort})` : ''}\n定价: 输入 $${s.price_in}/M · 缓存 $${s.price_cache}/M · 输出 $${s.price_out}/M`));

    // 2. Git branch (if in git repo)
    if (s.git_branch) {
      const dirty = s.git_dirty ? '*' : '';
      const sync = [s.git_ahead ? `↑${s.git_ahead}` : '', s.git_behind ? `↓${s.git_behind}` : ''].filter(Boolean).join('');
      line1.push(segHtml('--k-git', IC.git, `${esc(s.git_branch)}${dirty}${sync ? ` ${sync}` : ''}`, `仓库目录: ${s.workspace_dir || '—'}\n分支: ${s.git_branch}${s.git_dirty ? ' (有未提交修改)' : ' (干净)'}${s.git_added || s.git_deleted ? `\n改动: +${s.git_added} / -${s.git_deleted} 行` : ''}`));
    }

    // 3. Context % + tokens/window + compactions
    const ctxVar = s.context_pct >= 85 ? '--k-crit' : s.context_pct >= 65 ? '--k-warn' : '--k-context';
    const compPart = s.compactions > 0 ? `<span class="cc-dot">·</span><span class="cc-warn">${s.compactions}× compact</span>` : '';
    line1.push(
      segHtml(
        ctxVar,
        IC.ctx,
        `${Math.round(s.context_pct)}%<span class="cc-dot">·</span>${fmtTok(s.context_tokens)}/${fmtTok(s.context_limit)}${compPart}`,
        `当前上下文: ${s.context_tokens.toLocaleString()} / ${s.context_limit.toLocaleString()} (${s.context_pct.toFixed(1)}%)\n剩余空间: ${s.context_left.toLocaleString()} tokens\n自动压缩次数: ${s.compactions} 次`,
      ),
    );

    // 4. Cache read & hit%
    if (s.turns > 0) {
      line1.push(
        segHtml(
          '--k-cache',
          IC.cache,
          `${fmtTok(s.last_cache_tokens)}<span class="cc-dot">·</span>${Math.round(s.last_cache_hit_pct)}% hit`,
          `最近一轮缓存读取: ${s.last_cache_tokens.toLocaleString()} tokens (${s.last_cache_hit_pct.toFixed(1)}%)\n会话累计命中率: ${s.session_cache_hit_pct.toFixed(1)}%\n缓存状态: ${s.cache_warm ? `热态 (余 ${s.cache_warm_left_fmt})` : '已冷却'}\n已省费用: ${fmtUsd(s.cache_saved_usd)}`,
        ),
      );
    }

    // Line 2: Total Tokens | Cost | Today Cost | TPS | 5h Pace | 7d Pace
    line2.push(
      segHtml(
        '--k-tokens',
        IC.tokens,
        `Σ ${fmtTok(s.total_tokens)}`,
        `当前会话总消耗: ${s.total_tokens.toLocaleString()} tokens\n├─ 新增输入: ${s.fresh_input_tokens.toLocaleString()}\n├─ 缓存读取: ${s.cache_read_tokens.toLocaleString()}\n└─ 模型输出: ${s.output_tokens.toLocaleString()}${s.subagents.length ? `\n(含 ${s.subagents.length} 个子代理)` : ''}`,
      ),
    );

    line2.push(
      segHtml(
        '--k-cost',
        IC.cost,
        `${fmtUsd(s.session_cost_usd)}${s.today_cost_usd > 0 ? `<span class="cc-dot">·</span><span class="cc-sub">${fmtUsd(s.today_cost_usd)} today</span>` : ''}`,
        `当前会话花费: ${fmtUsd(s.session_cost_usd)}${s.cost_rate_per_hr != null ? ` (${fmtUsd(s.cost_rate_per_hr)}/h)` : ''}\n├─ 新增输入: ${fmtUsd(s.cost_in_usd)}\n├─ 缓存读取: ${fmtUsd(s.cost_cache_usd)}\n└─ 模型输出: ${fmtUsd(s.cost_out_usd)}\n今日全部会话累计: ${fmtUsd(s.today_cost_usd)} (${fmtTok(s.today_tokens)} tokens)`,
      ),
    );

    if (s.last_tps > 0) {
      line2.push(
        segHtml(
          '--k-timing',
          IC.tps,
          `${Math.round(s.last_tps)} tok/s`,
          `最新一轮速度: ${s.last_tps.toFixed(1)} tok/s (${s.tps_source === 'hook' ? 'Rust Hook 毫秒精确计时' : '日志时间戳计算'})\n会话平均速度: ${s.avg_tps.toFixed(1)} tok/s\n上轮推理耗时: ${s.last_duration_sec.toFixed(1)}s`,
        ),
      );
    }

    if (snap.config.quotas_enabled) {
      const q5 = s.quota_5h;
      const q7 = s.quota_7d;
      const q5Color = q5.pct >= 85 ? '--k-crit' : q5.pct >= 65 ? '--k-warn' : '--k-quota';
      const q7Color = q7.pct >= 85 ? '--k-crit' : q7.pct >= 65 ? '--k-warn' : '--k-quota';
      const q5Arrow = q5.pace_delta_pct !== 0 ? `<span class="${q5.pace_delta_pct > 0 ? 'cc-warn' : 'cc-ok'}">${q5.pace_delta_pct > 0 ? '↑' : '↓'}${Math.abs(q5.pace_delta_pct)}%</span>` : '';
      const q7Arrow = q7.pace_delta_pct !== 0 ? `<span class="${q7.pace_delta_pct > 0 ? 'cc-warn' : 'cc-ok'}">${q7.pace_delta_pct > 0 ? '↑' : '↓'}${Math.abs(q7.pace_delta_pct)}%</span>` : '';

      line2.push(
        segHtml(
          q5Color,
          IC.q5,
          `${paceBarHtml(q5.ratio, q5.elapsed_ratio)}5h ${q5.pct}%${q5Arrow ? ' ' + q5Arrow : ''}`,
          `5 小时滚动窗口预算: ${q5.used_fmt} / ${q5.limit_fmt} (${q5.pct}%)\n窗口时间进度: ${Math.round(q5.elapsed_ratio * 100)}% · 重置剩余: ${q5.left_fmt}`,
        ),
      );

      line2.push(
        segHtml(
          q7Color,
          IC.q7,
          `${paceBarHtml(q7.ratio, q7.elapsed_ratio)}7d ${q7.pct}%${q7Arrow ? ' ' + q7Arrow : ''}`,
          `7 天周滚动窗口预算: ${q7.used_fmt} / ${q7.limit_fmt} (${q7.pct}%)\n窗口时间进度: ${Math.round(q7.elapsed_ratio * 100)}% · 重置剩余: ${q7.left_fmt}`,
        ),
      );
    }

    stripEl.innerHTML = `<div class="cc-line">${line1.join('')}</div><div class="cc-line">${line2.join('')}</div>`;
  }

  function renderOverview(snap) {
    const s = snap.summary;
    const ctxColor = s.context_pct >= 85 ? 'var(--k-crit)' : s.context_pct >= 65 ? 'var(--k-warn)' : 'var(--k-context)';
    const q5 = s.quota_5h;
    const q7 = s.quota_7d;

    const subagentsSection = s.subagents.length
      ? `<div class="section">
          <div class="sec-head">
            <span class="sec-title">Subagents (${s.subagents.length})</span>
            <span class="sec-value">${fmtUsd(s.cost_sub_usd)}</span>
          </div>
          ${s.subagents
            .map(
              (sub) => `<div class="sub-row">
              <div>
                <span>${esc(sub.model)}</span>
                <span class="sub-meta"> · ${fmtTok(sub.total_tokens)} tok · ${fmtUsd(sub.cost_usd)} · ${esc(sub.state)}</span>
              </div>
              <button type="button" class="link-btn btn-open-conv" data-conv="${esc(sub.id)}">Open</button>
            </div>`,
            )
            .join('')}
        </div>`
      : '';

    panelOverview.innerHTML = `
      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Context Window</span>
          <span class="sec-value">${s.context_pct.toFixed(1)}% · ${fmtTok(s.context_tokens)} / ${fmtTok(s.context_limit)}</span>
        </div>
        <div class="meter"><div class="meter-fill" style="width:${Math.min(100, s.context_pct).toFixed(1)}%;background:${ctxColor}"></div></div>
        <div class="kv-grid">
          <div class="kv-row"><span class="kv-k">Remaining</span><span class="kv-v">${fmtTok(s.context_left)}</span></div>
          <div class="kv-row"><span class="kv-k">Compactions</span><span class="kv-v">${s.compactions}×</span></div>
          <div class="kv-row"><span class="kv-k">Last Turn Δ</span><span class="kv-v">${s.last_context_delta >= 0 ? '+' : ''}${fmtTok(s.last_context_delta)}</span></div>
          <div class="kv-row"><span class="kv-k">Est. Turns Left</span><span class="kv-v">${s.turns_to_compact ? `~${s.turns_to_compact} turns` : '—'}</span></div>
        </div>
        ${s.context_series.length >= 2 ? `<div class="spark-wrap"><span class="kv-k" style="font-size:11px">Trend</span>${sparkSvg(s.context_series, 'var(--k-context)')}</div>` : ''}
      </div>

      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Prompt Cache</span>
          <span class="sec-value">${s.last_cache_hit_pct.toFixed(1)}% hit</span>
        </div>
        <div class="meter"><div class="meter-fill" style="width:${Math.min(100, s.last_cache_hit_pct).toFixed(1)}%;background:var(--k-cache)"></div></div>
        <div class="kv-grid">
          <div class="kv-row"><span class="kv-k">Session Avg Hit</span><span class="kv-v">${s.session_cache_hit_pct.toFixed(1)}%</span></div>
          <div class="kv-row"><span class="kv-k">Warmth (5m TTL)</span><span class="kv-v">${s.cache_warm ? s.cache_warm_left_fmt : 'cold'}</span></div>
          <div class="kv-row"><span class="kv-k">Cache Misses</span><span class="kv-v">${s.cache_misses}×</span></div>
          <div class="kv-row"><span class="kv-k">API Saved</span><span class="kv-v">${fmtUsd(s.cache_saved_usd)}</span></div>
        </div>
      </div>

      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Tokens &amp; Cost</span>
          <span class="sec-value">${fmtTok(s.total_tokens)} · ${fmtUsd(s.session_cost_usd)}</span>
        </div>
        <div class="kv-grid">
          <div class="kv-row"><span class="kv-k">Uncached Input</span><span class="kv-v">${fmtTok(s.fresh_input_tokens)} (${fmtUsd(s.cost_in_usd)})</span></div>
          <div class="kv-row"><span class="kv-k">Cache Read</span><span class="kv-v">${fmtTok(s.cache_read_tokens)} (${fmtUsd(s.cost_cache_usd)})</span></div>
          <div class="kv-row"><span class="kv-k">Model Output</span><span class="kv-v">${fmtTok(s.output_tokens)} (${fmtUsd(s.cost_out_usd)})</span></div>
          <div class="kv-row"><span class="kv-k">Burn Rate</span><span class="kv-v">${s.cost_rate_per_hr != null ? fmtUsd(s.cost_rate_per_hr) + '/h' : '—'}</span></div>
          <div class="kv-row"><span class="kv-k">Today Global</span><span class="kv-v">${fmtUsd(s.today_cost_usd)}</span></div>
          <div class="kv-row"><span class="kv-k">Today Tokens</span><span class="kv-v">${fmtTok(s.today_tokens)}</span></div>
        </div>
      </div>

      <div class="section">
        <div class="sec-head">
          <span class="sec-title">Speed &amp; Timing</span>
          <span class="sec-value">${s.last_tps > 0 ? s.last_tps.toFixed(1) + ' tok/s' : '—'}</span>
        </div>
        <div class="kv-grid">
          <div class="kv-row"><span class="kv-k">Session Avg TPS</span><span class="kv-v">${s.avg_tps > 0 ? s.avg_tps.toFixed(1) + ' tok/s' : '—'}</span></div>
          <div class="kv-row"><span class="kv-k">Last Turn Time</span><span class="kv-v">${s.last_duration_sec > 0 ? s.last_duration_sec.toFixed(1) + 's' : '—'}</span></div>
          <div class="kv-row"><span class="kv-k">Timing Source</span><span class="kv-v">${s.tps_source === 'hook' ? 'Rust Hook (ms)' : 'Transcript'}</span></div>
          <div class="kv-row"><span class="kv-k">Session Wall</span><span class="kv-v">${esc(s.session_duration_fmt)}</span></div>
        </div>
        ${s.tps_series.length >= 2 ? `<div class="spark-wrap"><span class="kv-k" style="font-size:11px">TPS</span>${sparkSvg(s.tps_series, 'var(--k-timing)')}</div>` : ''}
      </div>

      <div class="section">
        <div class="sec-head">
          <span class="sec-title">5h / 7d Rolling Quota Pace</span>
          <span class="sec-value">${snap.config.quota_mode === 'tokens' ? 'Tokens' : 'USD Budget'}</span>
        </div>
        <div class="kv-row">
          <span class="kv-k">5-Hour Window (${q5.used_fmt} / ${q5.limit_fmt})</span>
          <span class="kv-v">${q5.pct}% · reset ${q5.left_fmt} ${q5.pace_delta_pct ? (q5.pace_delta_pct > 0 ? `↑${q5.pace_delta_pct}%` : `↓${Math.abs(q5.pace_delta_pct)}%`) : ''}</span>
        </div>
        <div class="meter-dual">
          <div class="meter"><div class="meter-fill" style="width:${(q5.ratio * 100).toFixed(1)}%;background:var(--k-quota)"></div></div>
          <div class="meter sub"><div class="meter-fill" style="width:${(q5.elapsed_ratio * 100).toFixed(1)}%;background:var(--fg-muted)"></div></div>
        </div>

        <div class="kv-row" style="margin-top:8px">
          <span class="kv-k">7-Day Window (${q7.used_fmt} / ${q7.limit_fmt})</span>
          <span class="kv-v">${q7.pct}% · reset ${q7.left_fmt} ${q7.pace_delta_pct ? (q7.pace_delta_pct > 0 ? `↑${q7.pace_delta_pct}%` : `↓${Math.abs(q7.pace_delta_pct)}%`) : ''}</span>
        </div>
        <div class="meter-dual">
          <div class="meter"><div class="meter-fill" style="width:${(q7.ratio * 100).toFixed(1)}%;background:var(--k-quota)"></div></div>
          <div class="meter sub"><div class="meter-fill" style="width:${(q7.elapsed_ratio * 100).toFixed(1)}%;background:var(--fg-muted)"></div></div>
        </div>
      </div>

      ${subagentsSection}
    `;

    for (const btn of panelOverview.querySelectorAll('.btn-open-conv')) {
      btn.addEventListener('click', () => {
        const cid = btn.dataset.conv;
        if (window.sidecar?.ui?.toggleConversation) {
          window.sidecar.ui.toggleConversation(cid);
        } else {
          selectedConvId = cid;
          poll();
        }
      });
    }
  }

  function renderTurns(snap) {
    const turns = snap.turns || [];
    if (!turns.length) {
      panelTurns.innerHTML = `<div style="padding:24px 0;color:var(--fg-muted);font-size:12px">暂无模型推理记录</div>`;
      return;
    }
    panelTurns.innerHTML = turns
      .slice()
      .reverse()
      .map(
        (t) => `
      <div class="turn-row">
        <div class="turn-left">
          <span class="turn-step">#${t.step_index} · ${esc(t.time_hms)} · ctx ${fmtTok(t.prompt_context)} (${Math.round(t.hit_pct)}% hit)</span>
          <span class="turn-label">${esc(t.label || 'Final reply')}</span>
        </div>
        <div class="turn-right">
          <div>${fmtUsd(t.cost_usd)} · +${fmtTok(t.out_tokens)} out</div>
          <div style="color:var(--fg-muted)">${t.tps > 0 ? `${t.tps.toFixed(0)} tok/s · ${t.duration_sec.toFixed(1)}s` : '—'}</div>
        </div>
      </div>
    `,
      )
      .join('');
  }

  function renderSettings(snap, force = false) {
    if (settingsDirty && !force) return;
    const c = snap.config;
    panelSettings.innerHTML = `
      <div class="section">
        <div class="sec-head"><span class="sec-title">输入框状态栏 (In-page Statusline)</span></div>
        <div class="form-hint" id="live-status" style="margin-bottom:8px;line-height:1.6">…</div>

        <div class="form-row">
          <div class="form-label">
            <span>Show Statusline Above Input</span>
            <span class="form-hint">需要 Antigravity 带 --remote-debugging-port 启动</span>
          </div>
          <input id="cfg-bar-on" type="checkbox" ${c.bar_enabled ? 'checked' : ''} />
        </div>
        <div class="form-row">
          <div class="form-label"><span>Cost (conversation · today)</span></div>
          <input id="cfg-bar-cost" type="checkbox" ${c.bar_show_cost ? 'checked' : ''} />
        </div>
        <div class="form-row">
          <div class="form-label"><span>5h / Weekly Quota</span><span class="form-hint">来自 Antigravity 后台的真实额度</span></div>
          <input id="cfg-bar-quota" type="checkbox" ${c.bar_show_quota ? 'checked' : ''} />
        </div>
        <div class="form-row">
          <div class="form-label"><span>Running Subagent Rows</span></div>
          <input id="cfg-bar-subs" type="checkbox" ${c.bar_show_subagents ? 'checked' : ''} />
        </div>
      </div>

      <div class="section">
        <div class="sec-head"><span class="sec-title">Context &amp; Rolling Budgets (config.toml)</span></div>

        <div class="form-row">
          <div class="form-label">
            <span>Context Window</span>
            <span class="form-hint">默认按模型实际窗口计算（Gemini 1M / Claude 200K），也可手动指定压缩线</span>
          </div>
          <select id="cfg-ctx-win" class="field-select">
            <option value="0" ${c.context_limit_override === 0 ? 'selected' : ''}>模型默认 (1M / 200K)</option>
            <option value="200000" ${c.context_limit_override === 200000 ? 'selected' : ''}>200K</option>
            <option value="256000" ${c.context_limit_override === 256000 ? 'selected' : ''}>256K</option>
            <option value="512000" ${c.context_limit_override === 512000 ? 'selected' : ''}>512K</option>
            <option value="1000000" ${c.context_limit_override === 1000000 ? 'selected' : ''}>1M</option>
          </select>
        </div>

        <div class="form-row">
          <div class="form-label">
            <span>Show 5h / 7d Quota Pace</span>
            <span class="form-hint">统计本机全部会话在滚动窗口内的消耗配速</span>
          </div>
          <input id="cfg-q-on" type="checkbox" ${c.quotas_enabled ? 'checked' : ''} />
        </div>

        <div class="form-row">
          <div class="form-label"><span>Quota Unit</span></div>
          <select id="cfg-q-mode" class="field-select">
            <option value="usd" ${c.quota_mode !== 'tokens' ? 'selected' : ''}>USD ($)</option>
            <option value="tokens" ${c.quota_mode === 'tokens' ? 'selected' : ''}>Tokens</option>
          </select>
        </div>

        <div class="form-row">
          <div class="form-label"><span>5-Hour USD Budget ($)</span></div>
          <input id="cfg-5h-usd" type="number" step="0.5" class="field-input" value="${c.five_hour_usd_limit}" />
        </div>

        <div class="form-row">
          <div class="form-label"><span>7-Day Weekly USD Budget ($)</span></div>
          <input id="cfg-7d-usd" type="number" step="1" class="field-input" value="${c.seven_day_usd_limit}" />
        </div>

        <div class="form-row">
          <div class="form-label"><span>5-Hour Token Budget</span></div>
          <input id="cfg-5h-tok" type="number" step="500000" class="field-input" value="${c.five_hour_token_limit}" />
        </div>

        <div class="form-row">
          <div class="form-label"><span>7-Day Token Budget</span></div>
          <input id="cfg-7d-tok" type="number" step="1000000" class="field-input" value="${c.seven_day_token_limit}" />
        </div>

        <div class="form-row">
          <div class="form-label"><span>Include Subagents in Session Total</span></div>
          <input id="cfg-sub" type="checkbox" ${c.include_subagents ? 'checked' : ''} />
        </div>

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
        quotas_enabled: $('cfg-q-on').checked,
        quota_mode: $('cfg-q-mode').value,
        five_hour_usd_limit: Number($('cfg-5h-usd').value) || 15,
        seven_day_usd_limit: Number($('cfg-7d-usd').value) || 80,
        five_hour_token_limit: Number($('cfg-5h-tok').value) || 15000000,
        seven_day_token_limit: Number($('cfg-7d-tok').value) || 80000000,
        include_subagents: $('cfg-sub').checked,
        bar_enabled: $('cfg-bar-on').checked,
        bar_show_cost: $('cfg-bar-cost').checked,
        bar_show_quota: $('cfg-bar-quota').checked,
        bar_show_subagents: $('cfg-bar-subs').checked,
      };
      const f = window.sidecar?.fetch ? window.sidecar.fetch.bind(window.sidecar) : fetch;
      const res = await f('/api/config', {
        method: 'POST',
        body: JSON.stringify(payload),
      });
      if (res.ok) {
        settingsDirty = false;
        lastEtag = '';
        await poll();
      }
    });
  }

  async function updateLiveStatus() {
    const box = document.getElementById('live-status');
    if (!box) return;
    try {
      const r = await fetch('/api/live-status', { cache: 'no-store' });
      const j = await r.json();
      box.innerHTML = `CDP: ${esc(j.cdp || '—')}<br>language_server: ${esc(j.ls || '—')}` +
        (j.panes && j.panes.length ? `<br>showing: ${j.panes.map((x) => esc(String(x).slice(0, 8))).join(', ')}` : '');
    } catch (e) {
      box.textContent = 'live status unavailable';
    }
  }

  function renderPanels(snap, forceSettings = false) {
    turnsCountEl.textContent = snap.summary.turns ? String(snap.summary.turns) : '';
    if (activeTab === 'overview') renderOverview(snap);
    else if (activeTab === 'turns') renderTurns(snap);
    else if (activeTab === 'settings') {
      renderSettings(snap, forceSettings);
      updateLiveStatus();
    }
  }

  async function poll() {
    try {
      const qs = new URLSearchParams();
      if (selectedConvId) qs.set('conversationId', selectedConvId);
      if (workspaceUris.length) qs.set('workspaceUris', workspaceUris.join(','));
      if (lastEtag) qs.set('etag', lastEtag);

      const res = await fetch(`/api/telemetry?${qs.toString()}`, { cache: 'no-store' });
      if (!res.ok) return;
      const data = await res.json();
      if (data.unchanged && lastSnap) return;

      lastEtag = data.etag || '';
      lastSnap = data;

      // Update conversation selector
      const recents = data.recent_conversations || [];
      const autoTitle = hostConvId
        ? `Current Pane (${data.conversation_title})`
        : `Auto (${data.conversation_title})`;
      if (convSelect.options.length !== recents.length + 1) {
        convSelect.innerHTML =
          `<option value="${esc(hostConvId)}">${esc(autoTitle)}</option>` +
          recents
            .filter((c) => c.id !== hostConvId)
            .map((c) => `<option value="${esc(c.id)}" ${c.id === selectedConvId ? 'selected' : ''}>${esc(c.title)}</option>`)
            .join('');
      } else if (convSelect.options[0]) {
        convSelect.options[0].textContent = autoTitle;
      }

      // Live indicator
      const running = data.summary.state !== 'idle';
      livePill.classList.toggle('running', running);
      liveLabel.textContent = data.summary.activity_label || (running ? 'Running' : 'Idle');

      renderStrip(data);
      renderPanels(data, false);
    } catch (err) {
      console.error('statusline poll failed:', err);
    }
  }

  poll();
  setInterval(poll, 1200);
})();
