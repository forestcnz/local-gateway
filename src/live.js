/* ============================================================
   live.js — 控制台实时数据接线
   从 /admin/api/* 拉取数据渲染页面；静态预览（file:// 打开）时
   fetch 失败则保留演示数据，互不影响。
   ============================================================ */
(function () {
  "use strict";
  const $ = (s) => document.querySelector(s);
  const $$ = (s) => Array.from(document.querySelectorAll(s));
  const esc = (s) =>
    String(s ?? "").replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );

  async function api(path, opts = {}) {
    if (opts.body && typeof opts.body === "string") {
      opts.headers = Object.assign({ "Content-Type": "application/json" }, opts.headers || {});
    }
    const r = await fetch(path, opts);
    const txt = await r.text();
    let d;
    try { d = JSON.parse(txt); } catch { d = { error: txt || r.statusText }; }
    if (!r.ok) throw new Error(d.error || "HTTP " + r.status);
    return d;
  }

  const PATH_TO_PROTO = {
    "/v1/chat/completions": "chat",
    "/v1/responses": "responses",
    "/v1/messages": "anthropic",
  };
  const protoBadge = (p) => `<span class="proto ${p}">${esc(p)}</span>`;
  const fmtMs = (ms) => (ms < 1000 ? ms + "ms" : (ms / 1000).toFixed(2) + "s");
  const hhmmss = (iso) => {
    const d = new Date(iso);
    const ymd = d.getFullYear() + "-" +
      String(d.getMonth() + 1).padStart(2, "0") + "-" +
      String(d.getDate()).padStart(2, "0");
    return ymd + " " + d.toTimeString().slice(0, 8);
  };

  /* ---------- 统计 & 元信息 ---------- */
  let lastHourlyKey = "";

  function renderSpark(hourly) {
    const svg = document.getElementById("sparkChart");
    if (!svg || !Array.isArray(hourly) || hourly.length !== 24) return;
    const key = hourly.join(",");
    if (key === lastHourlyKey) return; // 数据未变则不重绘，避免动画闪烁
    lastHourlyKey = key;

    const W = 640, top = 30, bot = 175;
    const max = Math.max.apply(null, hourly.concat([1]));
    // 滚动窗口：桶 0 = 24 小时前那一小时，桶 23 = 当前进行中的小时（即「现在」）
    const x = (i) => 4 + (i * (W - 8)) / 23;
    const y = (v) => bot - (v / max) * (bot - top);
    const pts = hourly.map((v, i) => x(i).toFixed(1) + "," + y(v).toFixed(1)).join(" L");
    const line = "M" + pts;
    const area = line + " L" + x(23).toFixed(1) + "," + bot + " L" + x(0).toFixed(1) + "," + bot + " Z";
    const cur = hourly[23] || 0;
    const grid = [40, 85, 130, 175]
      .map((yy) => '<line x1="0" y1="' + yy + '" x2="' + W + '" y2="' + yy + '" stroke="rgba(0,0,0,.06)" stroke-width="1"/>')
      .join("");
    // 每个小时一个数据点：悬浮显示「HH:MM · N 次请求」
    const dots = hourly.map((v, i) => {
      const d = new Date(Date.now() - (23 - i) * 3600 * 1000);
      const hhmm = d.toTimeString().slice(0, 5);
      return '<circle cx="' + x(i).toFixed(1) + '" cy="' + y(v).toFixed(1) + '" r="3" fill="#1677ff">' +
        '<title>' + hhmm + " · " + v + " 次请求</title></circle>";
    }).join("");
    svg.innerHTML =
      grid +
      '<path d="' + area + '" fill="rgba(22,119,255,.08)"/>' +
      '<path class="spark-line" d="' + line + '" fill="none" stroke="#1677ff" stroke-width="2"/>' +
      dots +
      '<circle cx="' + x(23).toFixed(1) + '" cy="' + y(cur).toFixed(1) + '" r="4.5" fill="#1677ff"/>' +
      '<text x="' + (x(23) - 10).toFixed(1) + '" y="' + Math.max(14, y(cur) - 12).toFixed(1) +
      '" text-anchor="end" font-family="IBM Plex Mono,monospace" font-size="10" fill="#262626" font-weight="600">' + cur + "/h</text>";
    // 横轴刻度 = 滚动窗口各槽位的本地时间
    const axis = svg.parentElement.querySelector(".spark-axis");
    if (axis) {
      const fmt = (i) => {
        const d = new Date(Date.now() - (23 - i) * 3600 * 1000);
        return d.toTimeString().slice(0, 5);
      };
      axis.innerHTML = [0, 6, 12, 18, 23]
        .map((i) => "<span>" + (i === 23 ? "现在" : fmt(i)) + "</span>")
        .join("");
    }
  }

  function renderStats(d) {
    const chip = $(".top-status .ep-chip .v");
    if (chip) chip.textContent = d.listen;
    const nums = $$("#view-overview .stat .num");
    if (nums.length >= 4) {
      nums[0].innerHTML = Number(d.requests).toLocaleString();
      nums[1].innerHTML = d.success_rate + "<small>%</small>";
      nums[2].innerHTML = (d.avg_latency_ms || 0) + "<small>ms</small>";
      nums[3].innerHTML = d.active_providers + "<small>/ " + d.total_providers + "</small>";
    }
    // 相对昨日的趋势（真实数据，不再写死 ▲18%）
    const badge = $("#view-overview .stat .lbl i");
    if (badge) {
      const yd = Number(d.yesterday_requests) || 0;
      const td = Number(d.requests) || 0;
      if (yd === 0) {
        badge.textContent = td > 0 ? "今日起量" : "暂无请求";
        badge.className = "";
      } else {
        const pct = Math.round(((td - yd) / yd) * 100);
        badge.textContent = (pct >= 0 ? "▲ " : "▼ ") + Math.abs(pct) + "% vs 昨日";
        badge.className = pct >= 0 ? "" : "dn";
      }
    }
    renderSpark(d.hourly);
    const om = $("#view-overview .sec-meta");
    if (om) om.textContent = "数据截至 " + new Date().toTimeString().slice(0, 8) + " · 5s 自动刷新";
    const pm = $("#view-providers .sec-meta");
    if (pm) pm.textContent = d.total_providers + " 个供应商 · " + d.active_providers + " 个启用中";
    const lm = $("#view-logs .sec-meta");
    if (lm) lm.textContent = "SQLite 存储 · 共 " + Number(d.total_logs || 0).toLocaleString() + " 条 · 保留 " + d.retention_days + " 天";
  }

  /* ---------- 供应商 ---------- */
  const ADD_CARD_HTML =
    '<button class="add-card" data-open-modal>' +
    '<span class="plus">+</span><span class="t">新增供应商</span>' +
    '<span style="font-size:10.5px;letter-spacing:.08em">OpenAI / Anthropic / 中转站 / 本地模型 均可接入</span>' +
    "</button>";

  function providerCard(p) {
    const protos = p.protocols.map(protoBadge).join("");
    const chipText = (m) => {
      const i = m.indexOf("=");
      return i > 0 ? m.slice(0, i) + " → " + m.slice(i + 1) : m;
    };
    let chips = p.models
      .slice(0, 4)
      .map((m) => {
        const t = chipText(m);
        return '<span class="chip"' + (m.indexOf("=") > 0 ? ' title="别名映射：客户端请求左侧名称，转发右侧模型"' : "") + ">" + esc(t) + "</span>";
      })
      .join("");
    if (p.models.length > 4) chips += '<span class="chip">+' + (p.models.length - 4) + "</span>";
    if (!p.models.length) chips = '<span class="chip" style="opacity:.5">未配置模型</span>';
    const state = p.enabled
      ? '<span class="dot"></span>启用中'
      : '<span class="dot off"></span>已停用';
    return (
      '<div class="card prov">' +
      '<div class="prov-head"><span class="prov-name">' + esc(p.name) + "</span>" + protos +
      '<button class="switch ' + (p.enabled ? "on" : "") + '" data-act="ptoggle" data-name="' + esc(p.name) + '" aria-pressed="' + p.enabled + '"><span class="knob"></span></button></div>' +
      '<div class="prov-body">' +
      '<div class="kv"><span class="k">Base URL</span><span class="v">' + esc(p.base_url) + "</span></div>" +
      '<div class="kv"><span class="k">API Key</span><span class="v">' + (p.api_key_set ? "已配置 · 界面不回显" : '<span class="dim">无需密钥</span>') + "</span></div>" +
      '<div class="kv"><span class="k">模型</span><span class="models">' + chips + "</span></div>" +
      "</div>" +
      '<div class="prov-foot"><span class="lat">' + state + "</span>" +
      '<button class="btn sm ghost" data-act="test" data-name="' + esc(p.name) + '">测试连接</button>' +
      '<button class="btn sm" data-act="edit" data-name="' + esc(p.name) + '">编辑</button>' +
      '<button class="btn sm" data-act="del" data-name="' + esc(p.name) + '">删除</button>' +
      "</div></div>"
    );
  }

  async function refreshProviders() {
    let list = [];
    try { list = (await api("/admin/api/providers")).providers; } catch { return; }
    const grid = $("#view-providers .prov-grid");
    if (grid) grid.innerHTML = list.map((p) => providerCard(p)).join("") + ADD_CARD_HTML;
    // 侧边导航「供应商」计数
    const navCount = document.getElementById("navProvCount");
    if (navCount) navCount.textContent = list.length;
    const meta = $("#view-providers .sec-meta");
    if (meta) meta.textContent = list.length + " 个供应商 · " + list.filter((p) => p.enabled).length + " 个启用中 · model 直接写模型名";
  }

  /* ---------- 模型别名（全局） ---------- */
  function aliasRow(a, m) {
    return (
      '<tr><td class="mono" style="font-weight:600">' + esc(a) + "</td>" +
      '<td class="mono">' + esc(m) + "</td>" +
      '<td style="text-align:right"><span class="detail-link" data-act="alias-del" data-alias="' + esc(a) + '">删除</span></td></tr>'
    );
  }

  async function renderAliases() {
    let list = [];
    try { list = (await api("/admin/api/aliases")).aliases; } catch { return; }
    const body = $("#aliasBody");
    if (!body) return;
    body.innerHTML = list.length
      ? list.map((r) => aliasRow(r.alias, r.model)).join("")
      : '<tr><td colspan="3" class="dim" style="text-align:center;padding:14px">暂无别名。客户端请求的模型命中别名时，先转换为真实模型再选址</td></tr>';
    const count = $("#navAliasCount");
    if (count) count.textContent = list.length;
    const meta = $("#aliasMeta");
    if (meta) meta.textContent = list.length + " 个别名 · 命中即转换为真实模型";
  }

  async function addAlias() {
    const a = ($("#aliasIn")?.value || "").trim();
    const m = ($("#aliasModelIn")?.value || "").trim();
    if (!a || !m) { showToast("别名与真实模型名都必填"); return; }
    try {
      await api("/admin/api/aliases/" + encodeURIComponent(a), { method: "PUT", body: JSON.stringify({ model: m }) });
      showToast("别名已保存：" + a + " → " + m);
      $("#aliasIn").value = "";
      $("#aliasModelIn").value = "";
      renderAliases();
    } catch (e) {
      showToast("保存失败：" + e.message);
    }
  }

  /* ---------- 请求日志 ---------- */
  function logQuerystring() {
    const p = new URLSearchParams();
    const fP = $("#fProtocol")?.value || "";
    const fPr = $("#fProvider")?.value || "";
    const fS = $("#fStatus")?.value || "";
    const fT = $("#fSince")?.value || "";
    const fq = ($("#fSearch")?.value || "").trim();
    if (fP) p.set("protocol", fP);
    if (fPr) p.set("provider", fPr);
    if (fS) p.set("status", fS);
    if (fT) p.set("since", fT);
    if (fq) p.set("q", fq);
    p.set("limit", "200");
    const s = p.toString();
    return s ? "?" + s : "";
  }

  const fmtK = (n) => (n >= 10000 ? Math.round(n / 1000) + "k" : n >= 1000 ? (n / 1000).toFixed(1) + "k" : String(n));
  function tokensCell(l) {
    if (!l.tokens_in && !l.tokens_out && !l.tokens_cached) return "—";
    return [l.tokens_in, l.tokens_out, l.tokens_cached].map(fmtK).join(" / ");
  }

  function uaCell(l) {
    const ua = l.user_agent || "";
    if (!ua) return '<td><span class="dim">—</span></td>';
    return (
      '<td title="' + esc(ua) + '">' +
      '<div class="mono dim" style="font-size:11.5px;max-width:170px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">' + esc(ua) + "</div></td>"
    );
  }

  function logRow(l, i) {
    const s = String(l.status);
    const cls = s[0] === "2" ? "s2" : s[0] === "4" ? "s4" : "s5";
    const pct = Math.min(100, Math.max(3, l.latency_ms / 300));
    const col = l.status < 400 ? "#52c41a" : l.status < 500 ? "#faad14" : "#ff4d4f";
    const fb = l.attempts && l.attempts.length > 1 ? '<span class="fallback-tag">已切换备用</span>' : "";
    return (
      '<tr><td class="mono dim" style="font-size:12px">' + hhmmss(l.ts) + "</td>" +
      '<td class="mono" style="font-size:12.5px">POST ' + esc(l.path) + "</td>" +
      '<td class="mono" style="font-weight:600">' + esc(l.model) + "</td>" +
      "<td>" + esc(l.provider || "—") + " " + fb + "</td>" +
      '<td><span class="status ' + cls + '">' + s + "</span></td>" +
      '<td><span class="lat-cell"><span class="lat-bar"><i style="width:' + pct + "%;background:" + col + '"></i></span>' + fmtMs(l.latency_ms) + "</span></td>" +
      '<td class="mono" style="text-align:right">' + tokensCell(l) + "</td>" +
      uaCell(l) +
      "</tr>"
    );
  }

  // 总览页「最近请求」行渲染（6 列精简版，不含详情入口）
  function recentRow(l) {
    const s = String(l.status);
    const cls = s[0] === "2" ? "s2" : s[0] === "4" ? "s4" : "s5";
    return (
      '<tr><td class="time">' + hhmmss(l.ts) + "</td>" +
      '<td class="mono dim">POST ' + esc(l.path) + "</td>" +
      '<td class="model">' + esc(l.model) + "</td>" +
      '<td class="dim">' + esc(l.provider || "—") + "</td>" +
      '<td><span class="status ' + cls + '">' + s + "</span></td>" +
      '<td class="mono" style="text-align:right">' + fmtMs(l.latency_ms) + "</td></tr>"
    );
  }

  async function refreshLogs() {
    let logs = [];
    try { logs = (await api("/admin/api/logs" + logQuerystring())).logs; } catch { return; }
    const tbody = $("#view-logs table.data tbody");
    if (tbody)
      tbody.innerHTML = logs.length
        ? logs.map(logRow).join("")
        : '<tr><td colspan="8" class="dim" style="text-align:center;padding:28px">暂无请求，向网关发出第一次调用后这里会出现记录</td></tr>';
    // 总览「最近请求」独立拉取全时段最近 5 条，
    // 不跟随日志页筛选（默认「时间：今天」会导致当天无请求时显示为空）
    let recentLogs = [];
    try { recentLogs = (await api("/admin/api/logs?limit=5")).logs; } catch { recentLogs = []; }
    const recent = $("#view-overview table.mini-table tbody");
    if (recent) {
      // 无真实日志时显示空状态，绝不能回退保留 HTML 里的演示数据
      recent.innerHTML = recentLogs.length
        ? recentLogs.map(recentRow).join("")
        : '<tr><td colspan="6" class="dim" style="text-align:center;padding:18px">暂无请求，向网关发出第一次调用后这里会出现记录</td></tr>';
    }
  }

  /* ---------- 弹窗：新增 / 编辑供应商 ---------- */
  const modal = $("#providerModal");
  const finAll = () => $$("#providerModal input.fin");
  let editing = null;

  function clearTags() {
    $$("#tagbox .tag").forEach((t) => t.remove());
  }
  function addTagUI(txt) {
    txt = String(txt || "").trim();
    if (!txt) return;
    const s = document.createElement("span");
    s.className = "tag";
    s.textContent = txt;
    const x = document.createElement("span");
    x.className = "x"; x.textContent = "✕";
    x.addEventListener("click", () => s.remove());
    s.appendChild(x);
    const input = $("#tagInput");
    $("#tagbox").insertBefore(s, input);
  }
  function setProtoSel(protos) {
    $$("#providerModal .pp").forEach((b) => {
      const p = PATH_TO_PROTO[b.querySelector(".pp-path").textContent.trim()];
      b.classList.toggle("sel", protos.indexOf(p) >= 0);
    });
  }
  function openModalForNew() {
    editing = null;
    const f = finAll();
    f[0].value = ""; f[0].disabled = false; f[0].placeholder = "例如：OpenAI 官方 / 公司中转 / 本地 Ollama";
    f[1].value = ""; f[2].value = ""; f[2].placeholder = "sk-…";
    clearTags(); setProtoSel(["chat"]);
  }
  function openModalForEdit(p) {
    editing = p.name;
    const f = finAll();
    f[0].value = p.name; f[0].disabled = false; f[0].placeholder = "供应商名称（可修改）";
    f[1].value = p.base_url;
    f[2].value = ""; f[2].placeholder = "留空则保持原 Key 不变";
    clearTags(); (p.models || []).forEach(addTagUI); setProtoSel(p.protocols);
  }

  async function saveProvider() {
    const f = finAll();
    const name = f[0].value.trim();
    const base = f[1].value.trim();
    if (!name || !base) { showToast("名称与 Base URL 为必填项"); return; }
    const protos = $$("#providerModal .pp.sel .pp-path")
      .map((b) => PATH_TO_PROTO[b.textContent.trim()])
      .filter(Boolean);
    if (!protos.length) { showToast("至少勾选一种协议"); return; }
    const body = {
      name: name,
      base_url: base,
      protocols: protos,
      models: $$("#tagbox .tag").map((t) => t.firstChild.textContent),
    };
    const key = f[2].value.trim();
    if (key) body.api_key = key;
    try {
      if (editing) {
        await api("/admin/api/providers/" + encodeURIComponent(editing), { method: "PUT", body: JSON.stringify(body) });
        showToast("供应商已更新");
      } else {
        await api("/admin/api/providers", { method: "POST", body: JSON.stringify(body) });
        showToast("供应商已创建");
      }
      modal.classList.remove("open");
      refreshProviders();
    } catch (e) {
      showToast("保存失败：" + e.message);
    }
  }

  /* ---------- 设置 ---------- */
  function fillSettings(s) {
    const tin = $("#view-settings input.tin");
    if (tin) tin.value = s.listen;
    const drop = $("#tinDrop");
    if (drop) drop.value = s.drop_params || "";
    const narrow = $$("#view-settings input.tin.narrow");
    if (narrow.length >= 3) {
      narrow[0].value = s.upstream_timeout_secs;
      narrow[1].value = s.max_body_mb;
      narrow[2].value = s.retention_days;
    }
    const sw = $$("#view-settings .switch");
    if (sw.length >= 2) {
      [s.sse_passthrough, s.cors].forEach((v, i) => {
        sw[i].classList.toggle("on", !!v);
        sw[i].setAttribute("aria-pressed", String(!!v));
      });
    }
  }

  async function saveSettings() {
    const tin = $("#view-settings input.tin");
    const narrow = $$("#view-settings input.tin.narrow");
    const sw = $$("#view-settings .switch");
    const body = {
      listen: tin ? tin.value.trim() : undefined,
      drop_params: $("#tinDrop") ? $("#tinDrop").value.trim() : "",
      upstream_timeout_secs: Number(narrow[0] && narrow[0].value) || 120,
      max_body_mb: Number(narrow[1] && narrow[1].value) || 20,
      retention_days: Number(narrow[2] && narrow[2].value) || 7,
      sse_passthrough: !!(sw[0] && sw[0].classList.contains("on")),
      cors: !!(sw[1] && sw[1].classList.contains("on")),
    };
    try {
      const r = await api("/admin/api/settings", { method: "PUT", body: JSON.stringify(body) });
      showToast(r.note || "设置已保存");
    } catch (e) {
      showToast("保存失败：" + e.message);
    }
  }

  /* ---------- Toast（沿用页面样式） ---------- */
  const toast = $("#toast"), toastMsg = $("#toastMsg");
  let toastTimer;
  function showToast(msg) {
    if (!toast) return;
    toastMsg.textContent = msg;
    toast.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => toast.classList.remove("show"), 2600);
  }

  /* ---------- 用克隆替换：去掉演示 toast，绑真实动作 ---------- */
  function rebind(selector, fn) {
    const old = document.querySelector(selector);
    if (!old) return false;
    const nu = old.cloneNode(true);
    nu.removeAttribute("data-toast");
    nu.removeAttribute("data-close-modal");
    nu.removeAttribute("data-test");
    old.replaceWith(nu);
    nu.addEventListener("click", fn);
    return true;
  }

  /* ---------- 事件委托（动态渲染的元素） ---------- */
  document.addEventListener("click", async (e) => {
    const el = e.target.closest("[data-act]");
    if (!el) return;
    const act = el.dataset.act;
    const name = el.dataset.name;

    if (act === "ptoggle") {
      const on = !el.classList.contains("on");
      try {
        await api("/admin/api/providers/" + encodeURIComponent(name), { method: "PATCH", body: JSON.stringify({ enabled: on }) });
        refreshProviders();
      } catch (err) { showToast("操作失败：" + err.message); }
    } else if (act === "test") {
      if (el.disabled) return;
      el.disabled = true; el.textContent = "检测中…";
      try {
        const r = await api("/admin/api/providers/" + encodeURIComponent(name) + "/test", { method: "POST" });
        showToast(r.ok ? name + "：连接正常 · " + r.status + " · " + fmtMs(r.latency_ms) : name + "：HTTP " + r.status + " — " + (r.error || "失败"));
      } catch (err) { showToast(name + "：" + err.message); }
      el.disabled = false; el.textContent = "测试连接";
    } else if (act === "edit") {
      try {
        const list = (await api("/admin/api/providers")).providers;
        const p = list.find((x) => x.name === name);
        if (p) { openModalForEdit(p); modal.classList.add("open"); }
      } catch (err) { showToast(err.message); }
    } else if (act === "del") {
      if (!confirm("确定删除供应商「" + name + "」？该操作会写入数据库，日志会保留。")) return;
      try {
        await api("/admin/api/providers/" + encodeURIComponent(name), { method: "DELETE" });
        showToast("已删除");
        refreshProviders();
      } catch (err) { showToast("删除失败：" + err.message); }
    } else if (act === "alias-del") {
      if (!confirm("确定删除别名「" + el.dataset.alias + "」？")) return;
      try {
        await api("/admin/api/aliases/" + encodeURIComponent(el.dataset.alias), { method: "DELETE" });
        showToast("别名已删除");
        renderAliases();
      } catch (err) { showToast("删除失败：" + err.message); }
    }
  });

  /* 弹窗协议卡片本来就支持点选（原脚本），这里无需重复绑定 */

  /* ---------- 日志筛选控件 ---------- */
  async function populateProviderFilter() {
    const sel = $("#fProvider");
    if (!sel) return;
    let list = [];
    try { list = (await api("/admin/api/providers")).providers; } catch { return; }
    const cur = sel.value;
    sel.innerHTML =
      '<option value="">供应商：全部</option>' +
      list.map((p) => '<option value="' + esc(p.name) + '">' + esc(p.name) + "</option>").join("");
    // 保留当前选择
    if ([...sel.options].some((o) => o.value === cur)) sel.value = cur;
  }

  function initFilters() {
    const selects = ["#fProtocol", "#fProvider", "#fStatus", "#fSince"].map((s) => $(s)).filter(Boolean);
    selects.forEach((sel) => sel.addEventListener("change", () => refreshLogs()));
    const search = $("#fSearch");
    if (search) {
      let deb;
      search.addEventListener("input", () => {
        clearTimeout(deb);
        deb = setTimeout(refreshLogs, 300);
      });
      search.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          clearTimeout(deb);
          refreshLogs();
        }
      });
    }
  }

  async function reloadConfig() {
    try {
      await api("/admin/api/reload", { method: "POST" });
      showToast("配置已重新加载");
      refreshProviders();
    } catch (e) { showToast("重载失败：" + e.message); }
  }

  /* ---------- 初始化 ---------- */
  async function refreshStats() {
    try { renderStats(await api("/admin/api/stats")); } catch { /* 静态预览时静默 */ }
  }

  async function init() {
    // 支持 #providers / #routes 等 hash 深链直达对应页面
    const hash = location.hash.slice(1);
    if (hash && document.getElementById("view-" + hash) && typeof goto === "function") {
      goto(hash);
    }
    // 接管弹窗保存按钮
    const oldSave = $("#providerModal .modal-foot .btn.primary");
    if (oldSave) {
      const saveBtn = oldSave.cloneNode(true);
      oldSave.replaceWith(saveBtn);
      saveBtn.addEventListener("click", saveProvider);
    }
    // 打开弹窗（新增）时先重置表单；重渲染后的 add-card 原监听已失效，
    // 这里统一在捕获阶段负责「重置 + 打开」，原脚本的打开逻辑与之重复无害。
    document.addEventListener("click", (e) => {
      if (e.target.closest("[data-open-modal]")) {
        openModalForNew();
        modal.classList.add("open");
      }
    }, true);

    rebind("#view-settings div .btn.primary", saveSettings);
    rebind("#view-settings .set-row .btn.sm", reloadConfig);

    // 别名添加按钮
    const aliasAddBtn = $("#aliasAdd");
    if (aliasAddBtn) aliasAddBtn.addEventListener("click", addAlias);

    initFilters();
    await populateProviderFilter();
    await Promise.all([refreshStats(), refreshProviders(), refreshLogs(), renderAliases()]);
    try { fillSettings(await api("/admin/api/settings")); } catch { /* 静态预览 */ }

    setInterval(() => {
      if (document.hidden) return;
      refreshStats(); refreshLogs();
    }, 5000);
  }

  init();
})();
