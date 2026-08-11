"use strict";

const csrf = document.querySelector('meta[name="infer-console-session"]').content;
const state = {
  snapshot: null,
  logs: [],
  history: [],
  previousCounters: null,
  lastGeneration: 0,
  configLoaded: false,
  configDirty: false,
  refreshing: false,
};

const viewCopy = {
  overview: ["运行总览", "查看本机推理设施的实时状态与重要变化。"],
  statistics: ["统计与预算", "观察吞吐、队列、失败和资源使用趋势。"],
  jobs: ["任务与执行", "检查 Job、Intent、物理 Deployment 与运行状态。"],
  models: ["模型与资源", "管理 Provider、模型驻留、Inventory 与压力策略。"],
  access: ["Apps 与访问", "创建 Consumer 身份，管理调用权限、令牌轮换与撤销。"],
  logs: ["实时日志", "筛选并跟踪由本控制台启动的 inferd 进程输出。"],
  config: ["Runtime 配置", "在浏览器中校验配置，并通过显式重启应用变更。"],
};

async function api(path, options = {}) {
  const method = options.method || "GET";
  const headers = new Headers(options.headers || {});
  if (method !== "GET" && method !== "HEAD") {
    headers.set("x-infer-console-session", csrf);
  }
  if (options.body !== undefined) {
    headers.set("content-type", "application/json");
    options.body = JSON.stringify(options.body);
  }
  const response = await fetch(path, { ...options, method, headers });
  const payload = await response.json().catch(() => ({ ok: false, error: { message: `HTTP ${response.status}` } }));
  if (!response.ok) throw new Error(payload?.error?.message || `HTTP ${response.status}`);
  return payload;
}

function endpoint(name) {
  return state.snapshot?.runtime?.[name]?.value ?? null;
}

function endpointError(name) {
  return state.snapshot?.runtime?.[name]?.error ?? null;
}

async function refreshAll({ quiet = false } = {}) {
  if (state.refreshing) return;
  state.refreshing = true;
  const button = document.getElementById("refresh-button");
  button.disabled = true;
  button.textContent = "刷新中…";
  try {
    const [snapshotPayload, logPayload] = await Promise.all([
      api("/api/snapshot"),
      api("/api/logs"),
    ]);
    state.snapshot = snapshotPayload;
    state.logs = logPayload.logs || [];
    recordHistory();
    renderAll();
    if (!state.configLoaded) await loadConfig();
  } catch (error) {
    renderConsoleDisconnected(error.message);
    if (!quiet) toast(error.message, true);
  } finally {
    state.refreshing = false;
    button.disabled = false;
    button.textContent = "立即刷新";
  }
}

function recordHistory() {
  const generation = state.snapshot?.runtime?.generation || 0;
  if (!generation || generation === state.lastGeneration) return;
  state.lastGeneration = generation;
  const metrics = endpoint("metrics") || {};
  const current = {
    succeeded: number(metrics.succeeded),
    failed: number(metrics.failed) + number(metrics.expired) + number(metrics.cancelled),
  };
  if (state.previousCounters) {
    state.history.push({
      succeeded: Math.max(0, current.succeeded - state.previousCounters.succeeded),
      failed: Math.max(0, current.failed - state.previousCounters.failed),
    });
    if (state.history.length > 60) state.history.shift();
  }
  state.previousCounters = current;
}

function renderAll() {
  renderDaemon();
  renderMetrics();
  renderCharts();
  renderProviders();
  renderJobs();
  renderResources();
  renderBudget();
  renderLogs();
  renderConfigValidation();
  const refreshedAt = state.snapshot?.runtime?.refreshed_at_unix_ms;
  document.getElementById("last-updated").textContent = refreshedAt ? `更新于 ${formatClock(refreshedAt)}` : "等待首次刷新";
}

function renderDaemon() {
  const daemon = state.snapshot?.daemon || {};
  const online = Boolean(daemon.reachable);
  const ownership = daemon.ownership || "none";
  setStatusDot("sidebar-status-dot", online ? "online" : "offline");
  setStatusDot("pulse-core", online ? "online" : "offline");
  document.getElementById("sidebar-runtime-state").textContent = online ? "运行正常" : "服务离线";
  document.getElementById("sidebar-runtime-url").textContent = daemon.runtime_url || "—";
  document.getElementById("sidebar-ownership").textContent = ownership === "console"
    ? `控制台管理 · PID ${daemon.pid || "—"} · ${formatDuration(daemon.uptime_seconds)}`
    : ownership === "external" ? "已连接外部 daemon" : "没有运行中的 daemon";

  const chip = document.getElementById("header-status");
  chip.textContent = online ? "Online" : "Offline";
  chip.className = `status-chip ${online ? "online" : "offline"}`;
  document.getElementById("offline-banner").classList.toggle("hidden", online);
  document.getElementById("hero-state").textContent = online ? "控制平面正在稳定运行" : "推理服务尚未启动";
  document.getElementById("hero-summary").textContent = online
    ? ownership === "console"
      ? "inferd 由当前 Web Console 管理。任务、Provider 和资源状态会持续刷新。"
      : "已安全连接到一个外部 inferd；控制台不会停止或重启它。"
    : daemon.config_valid
      ? "配置已通过校验，可以从这里启动一个由控制台管理的 inferd。"
      : "当前配置未通过校验，请先在配置页修正后再启动。";

  document.querySelectorAll('[data-action="daemon-start"]').forEach(button => { button.disabled = online || !daemon.config_valid; });
  document.querySelectorAll('[data-action="daemon-stop"], [data-action="daemon-restart"]').forEach(button => { button.disabled = ownership !== "console"; });
}

function renderMetrics() {
  const metrics = endpoint("metrics") || {};
  const queues = Object.values(metrics.provider_queues || {});
  const active = queues.reduce((sum, queue) => sum + number(queue.active), 0);
  const pending = queues.reduce((sum, queue) => sum + number(queue.pending_interactive) + number(queue.pending_normal) + number(queue.pending_background), 0);
  const submitted = number(metrics.submitted);
  const succeeded = number(metrics.succeeded);
  const failed = number(metrics.failed) + number(metrics.queue_rejected);
  const completed = succeeded + number(metrics.failed) + number(metrics.cancelled) + number(metrics.expired);
  setText("metric-submitted", formatNumber(submitted));
  setText("metric-succeeded", formatNumber(succeeded));
  setText("metric-success-rate", completed ? `成功率 ${Math.round((succeeded / completed) * 100)}%` : "成功率 —");
  setText("metric-active", formatNumber(active));
  setText("metric-queued", `队列中 ${formatNumber(pending)}`);
  setText("metric-failed", formatNumber(failed));
  setText("metric-expired", `超时 ${formatNumber(metrics.expired)}`);
  setText("stat-dispatched", formatNumber(metrics.dispatched));
  setText("stat-cancelled", formatNumber(metrics.cancelled));
  setText("stat-rejected", formatNumber(metrics.queue_rejected));
  const averageWait = number(metrics.dispatched) ? number(metrics.queue_wait_ms_total) / number(metrics.dispatched) : null;
  setText("stat-wait", averageWait === null ? "—" : formatMilliseconds(averageWait));
}

function renderCharts() {
  renderHistoryChart("overview-chart", 36);
  renderHistoryChart("statistics-chart", 60);
  const metrics = endpoint("metrics") || {};
  const queues = Object.entries(metrics.provider_queues || {});
  const target = document.getElementById("queue-statistics");
  if (!queues.length) {
    target.className = "queue-list empty-state";
    target.textContent = endpointError("metrics") || "暂无队列数据";
    return;
  }
  target.className = "queue-list";
  target.innerHTML = queues.map(([provider, queue]) => {
    const active = number(queue.active);
    const pending = number(queue.pending_interactive) + number(queue.pending_normal) + number(queue.pending_background);
    const slots = [];
    for (let index = 0; index < 10; index += 1) {
      const className = index < Math.min(active, 10) ? "active" : index < Math.min(active + pending, 10) ? "pending" : "";
      slots.push(`<i class="${className}"></i>`);
    }
    return `<div class="queue-row"><div class="queue-row-header"><strong>${escapeHtml(provider)}</strong><span>${active} active · ${pending} pending</span></div><div class="queue-track">${slots.join("")}</div></div>`;
  }).join("");
}

function renderHistoryChart(id, limit) {
  const target = document.getElementById(id);
  const history = state.history.slice(-limit);
  const max = Math.max(0, ...history.flatMap(item => [item.succeeded, item.failed]));
  if (!history.length || max === 0) {
    target.className = target.className.replace(/\s*empty-chart/g, "") + " empty-chart";
    target.innerHTML = "<span>暂无任务活动</span>";
    return;
  }
  target.classList.remove("empty-chart");
  target.innerHTML = history.map(item => {
    const successLevel = Math.max(0, Math.min(10, Math.ceil((item.succeeded / max) * 10)));
    const failureLevel = Math.max(0, Math.min(10, Math.ceil((item.failed / max) * 10)));
    return `<span class="chart-column" title="成功 ${item.succeeded}，失败 ${item.failed}"><i class="chart-bar failure level-${failureLevel}"></i><i class="chart-bar success level-${successLevel}"></i></span>`;
  }).join("");
}

function renderProviders() {
  const providers = endpoint("providers")?.providers || endpoint("providers") || [];
  const queues = endpoint("metrics")?.provider_queues || {};
  const target = document.getElementById("overview-providers");
  target.className = "stack-list";
  if (!Array.isArray(providers) || !providers.length) {
    target.innerHTML = `<div class="empty-state">${escapeHtml(endpointError("providers") || "没有 Provider")}</div>`;
    return;
  }
  target.innerHTML = providers.map(provider => {
    const queue = queues[provider.id] || {};
    const pending = number(queue.pending_interactive) + number(queue.pending_normal) + number(queue.pending_background);
    const health = provider.circuit_open ? "熔断" : provider.configured ? "可用" : "未配置";
    const healthClass = provider.circuit_open ? "error" : provider.configured ? "healthy" : "neutral";
    const modes = provider.execution_modes?.join(" / ") || "unary";
    return `<div class="stack-row"><div><strong>${escapeHtml(provider.id)}</strong><small>${escapeHtml(provider.kind)} · ${escapeHtml(provider.placement)} · ${escapeHtml(modes)} · ${number(queue.active)} active / ${pending} pending</small></div><span class="status-chip ${healthClass}">${health}</span></div>`;
  }).join("");
}

function renderJobs() {
  const jobs = endpoint("jobs")?.jobs || [];
  const query = document.getElementById("job-search").value.trim().toLowerCase();
  const filter = document.getElementById("job-filter").value;
  const visible = jobs.filter(job => {
    const haystack = [job.id, job.intent, job.provider, job.deployment, job.app_id].join(" ").toLowerCase();
    return (!query || haystack.includes(query)) && (filter === "all" || job.state === filter);
  });
  const target = document.getElementById("jobs-table");
  if (!visible.length) {
    target.innerHTML = `<tr class="empty-row"><td colspan="7">${escapeHtml(endpointError("jobs") || (jobs.length ? "没有匹配任务" : "暂无任务记录"))}</td></tr>`;
    return;
  }
  target.innerHTML = visible.map(job => {
    const terminal = ["succeeded", "failed", "cancelled", "expired"].includes(job.state);
    return `<tr>
      <td><button class="table-action job-id mono" data-job-explain="${escapeAttribute(job.id)}" title="${escapeAttribute(job.id)}">${escapeHtml(shortId(job.id))}</button><small class="muted">${escapeHtml(job.app_id || "—")}</small></td>
      <td>${escapeHtml(job.intent || "—")}</td>
      <td><span>${escapeHtml(job.provider || "—")}</span><br><small class="muted mono">${escapeHtml(job.deployment || "—")}</small></td>
      <td>${escapeHtml(job.priority || "—")}</td>
      <td><span class="status-chip ${statusClass(job.state)}">${escapeHtml(job.state || "unknown")}</span></td>
      <td>${formatRelative(job.updated_at_ms)}</td>
      <td>${terminal ? "" : `<button class="table-action danger" data-job-cancel="${escapeAttribute(job.id)}">取消</button>`}</td>
    </tr>`;
  }).join("");
}

function renderResources() {
  const resources = endpoint("resources") || {};
  const providers = endpoint("providers")?.providers || endpoint("providers") || [];
  const resourceProviders = resources.providers || [];
  const resourceMap = new Map(resourceProviders.map(provider => [provider.provider, provider]));
  const pressure = resources.system_pressure || {};
  const pressureLevel = pressure.level || "unknown";
  const free = pressure.free_memory_percent;
  setText("free-memory", free === undefined ? "—" : `${free}%`);
  setClass("memory-meter", `meter-fill level-${free === undefined ? 0 : Math.round(free / 10)}`);
  const badge = document.getElementById("pressure-badge");
  badge.textContent = pressureLabel(pressureLevel);
  badge.className = `status-chip ${pressureLevel}`;

  const models = resourceProviders.flatMap(provider => provider.model_lifecycle || []);
  const resident = models.filter(model => ["ready", "loading", "draining"].includes(model.state));
  const reservations = models.reduce((sum, model) => sum + number(model.active_reservations), 0);
  const recommendation = resources.eviction_recommendation || {};
  const targets = recommendation.plan?.targets || [];
  setText("resident-count", String(resident.length));
  setText("reservation-count", String(reservations));
  setText("eviction-count", String(targets.length));
  setText("resource-summary", `${models.length} 个受管 Deployment · ${resident.length} 个驻留`);
  setText("resource-detail", pressure.last_error || `主机压力 ${pressureLabel(pressureLevel)}，可用内存 ${free ?? "未知"}%`);

  const allProviders = Array.isArray(providers) ? providers : [];
  const cards = allProviders.map(provider => providerCard(provider, resourceMap.get(provider.id)));
  for (const nativeProvider of resourceProviders) {
    if (!allProviders.some(provider => provider.id === nativeProvider.provider)) {
      cards.push(providerCard({ id: nativeProvider.provider, kind: nativeProvider.kind, placement: "local", configured: true }, nativeProvider));
    }
  }
  document.getElementById("resource-providers").innerHTML = cards.length ? cards.join("") : `<article class="panel empty-state">${escapeHtml(endpointError("resources") || "暂无资源数据")}</article>`;

  const evictionMode = document.getElementById("eviction-mode");
  evictionMode.textContent = recommendation.status === "planned" ? "有建议" : "只读";
  evictionMode.className = `status-chip ${recommendation.status === "planned" ? "warning" : "neutral"}`;
  const evictionPanel = document.getElementById("eviction-panel");
  if (recommendation.status === "planned") {
    evictionPanel.className = "eviction-item";
    evictionPanel.innerHTML = `<strong>${targets.length} 个候选模型</strong><p class="muted">预计释放 ${formatBytes(recommendation.plan.projected_freed_bytes)}，缺口 ${formatBytes(recommendation.plan.shortfall_bytes)}。此页面不会自动执行清退。</p>${targets.map(target => `<span class="status-chip warning">${escapeHtml(target.deployment)}</span>`).join(" ")}`;
  } else {
    evictionPanel.className = "empty-state";
    evictionPanel.textContent = evictionStatusText(recommendation);
  }
}

function providerCard(provider, resource) {
  const lifecycleModels = resource?.model_lifecycle || [];
  const lifecycleByDeployment = new Map(lifecycleModels.map(model => [model.deployment, model]));
  const configuredDeployments = Array.isArray(provider.deployments) ? provider.deployments : [];
  const deployments = configuredDeployments.length
    ? configuredDeployments
    : lifecycleModels.map(model => ({ id: model.deployment, model: model.model_id }));
  const available = resource?.available_deployments?.length || 0;
  const providerState = provider.circuit_open ? "熔断" : resource?.state || (provider.configured ? "configured" : "unconfigured");
  const rows = deployments.length ? deployments.map(deployment => {
    const lifecycle = lifecycleByDeployment.get(deployment.id);
    const modelIdentity = deployment.model || lifecycle?.model_id || "—";
    const coverage = deploymentCoverage(deployment);
    const details = [compactModelIdentity(modelIdentity), deployment.model_profile, coverage]
      .filter(Boolean)
      .join(" · ");
    const state = lifecycle?.state || (provider.circuit_open || !provider.configured ? "unavailable" : "admitted");
    const canLoad = lifecycle && !["ready", "loading"].includes(lifecycle.state);
    const canUnload = lifecycle && ["ready", "draining"].includes(lifecycle.state) && number(lifecycle.active_reservations) === 0;
    const lifecycleDetail = lifecycle
      ? ` · ${formatBytes(lifecycle.resident_memory_bytes)} · ${number(lifecycle.active_reservations)} reservations`
      : "";
    const actions = lifecycle ? `<button class="mini-button" data-resource-action="load" data-provider="${escapeAttribute(provider.id)}" data-deployment="${escapeAttribute(deployment.id)}" ${canLoad ? "" : "disabled"}>加载</button><button class="mini-button" data-resource-action="unload" data-provider="${escapeAttribute(provider.id)}" data-deployment="${escapeAttribute(deployment.id)}" ${canUnload ? "" : "disabled"}>卸载</button>` : "";
    return `<div class="model-row"><div class="model-main"><strong title="${escapeAttribute(deployment.id)}">${escapeHtml(deployment.id)}</strong><small title="${escapeAttribute(`${modelIdentity}${coverage ? ` · ${coverage}` : ""}`)}">${escapeHtml(details)}${escapeHtml(lifecycleDetail)}</small></div><div class="model-actions"><span class="status-chip ${statusClass(state)}">${escapeHtml(modelStateLabel(state))}</span>${actions}</div></div>`;
  }).join("") : `<div class="empty-state">该 Provider 当前没有已准入的 Deployment。</div>`;
  const probe = deployments.length && ["responses", "codex_app_server"].includes(provider.kind) ? `<button class="mini-button" data-provider-probe="${escapeAttribute(provider.id)}">兼容性 Probe</button>` : "";
  const catalog = provider.kind === "codex_app_server" ? `<button class="mini-button" data-provider-models="${escapeAttribute(provider.id)}">动态 Inventory</button>` : "";
  const access = provider.access_class && provider.access_class !== "standard" ? ` · ${provider.access_class}` : "";
  const availability = `${deployments.length} admitted${lifecycleModels.length ? ` · ${available} available` : ""}`;
  const modes = provider.execution_modes?.join(" / ") || "unary";
  const tools = catalog || probe ? `<div class="provider-tools">${catalog}${probe}</div>` : "";
  return `<article class="panel provider-card"><div class="provider-card-header"><div><strong>${escapeHtml(provider.id)}</strong><small>${escapeHtml(provider.kind || "unknown")} · ${escapeHtml(provider.placement || "—")}${escapeHtml(access)} · ${escapeHtml(modes)} · ${escapeHtml(availability)}</small></div><span class="status-chip ${statusClass(providerState)}">${escapeHtml(providerState)}</span>${tools}</div>${rows}</article>`;
}

function deploymentCoverage(deployment) {
  const ratings = Object.entries(deployment.ratings || {});
  if (!ratings.length) return "";
  const labels = ratings.map(([intent, rating]) => `${intent}:${rating.level || "—"}`);
  return labels.length <= 2 ? labels.join(" / ") : `${labels.slice(0, 2).join(" / ")} +${labels.length - 2}`;
}

function modelStateLabel(state) {
  if (state === "admitted") return "ADMITTED";
  if (state === "unavailable") return "UNAVAILABLE";
  return state;
}

function compactModelIdentity(value) {
  const identity = String(value || "—");
  return /^sha256:[0-9a-f]{64}$/i.test(identity)
    ? `${identity.slice(0, 15)}…${identity.slice(-6)}`
    : identity;
}

function renderBudget() {
  const budget = endpoint("budget") || {};
  const ledger = budget.usage_ledger || [];
  const reservations = budget.active_reservations || [];
  const totals = ledger.reduce((result, entry) => {
    result.cost += number(entry.amount_usd);
    result.input += number(entry.input_tokens);
    result.output += number(entry.output_tokens);
    return result;
  }, { cost: 0, input: 0, output: 0 });
  document.getElementById("budget-summary").innerHTML = `<span>结算记录 <strong>${formatNumber(ledger.length)}</strong></span><span>活跃预约 <strong>${formatNumber(reservations.length)}</strong></span><span>总费用 <strong>${formatUsd(totals.cost)}</strong></span><span>Tokens <strong>${formatNumber(totals.input + totals.output)}</strong></span>`;
  const groups = new Map();
  for (const entry of ledger) {
    const key = `${entry.app_id || "unknown"} / ${entry.provider || "unknown"}`;
    const group = groups.get(key) || { requests: 0, input: 0, output: 0, cost: 0 };
    group.requests += 1; group.input += number(entry.input_tokens); group.output += number(entry.output_tokens); group.cost += number(entry.amount_usd);
    groups.set(key, group);
  }
  const target = document.getElementById("usage-table");
  target.innerHTML = groups.size ? [...groups.entries()].map(([name, group]) => `<tr><td>${escapeHtml(name)}</td><td>${formatNumber(group.requests)}</td><td>${formatNumber(group.input)}</td><td>${formatNumber(group.output)}</td><td>${formatUsd(group.cost)}</td></tr>`).join("") : `<tr class="empty-row"><td colspan="5">暂无已结算使用记录</td></tr>`;
}

function renderLogs() {
  const filter = document.getElementById("log-filter").value;
  const query = document.getElementById("log-search").value.trim().toLowerCase();
  const visible = state.logs.filter(line => (filter === "all" || line.level === filter) && (!query || `${line.source} ${line.level} ${line.text}`.toLowerCase().includes(query)));
  const warnings = state.logs.filter(line => line.level === "warn").length;
  const errors = state.logs.filter(line => line.level === "error").length;
  setText("log-count", `${visible.length} / ${state.logs.length} 条日志`);
  setText("log-warning-count", `${warnings} 条警告`);
  setText("log-error-count", `${errors} 条错误`);
  const target = document.getElementById("log-stream");
  target.innerHTML = visible.length ? visible.map(line => `<div class="log-line ${statusClass(line.level)}"><span class="log-time">${formatClock(line.recorded_at_unix_ms, true)}</span><span class="log-source">${escapeHtml(line.source)}</span><span class="log-level">${escapeHtml(line.level.toUpperCase())}</span><span>${escapeHtml(line.text)}</span></div>`).join("") : `<div class="empty-state">${state.logs.length ? "没有匹配日志" : "启动 inferd 后，日志会实时显示在这里。"}</div>`;
  if (document.getElementById("log-follow").checked) target.scrollTop = target.scrollHeight;
}

async function loadConfig(force = false) {
  if (state.configDirty && !force) return;
  try {
    const payload = await api("/api/config");
    const result = payload.result;
    document.getElementById("config-editor").value = result.source;
    document.getElementById("config-path").textContent = result.path;
    document.getElementById("config-filename").textContent = result.path.split(/[\\/]/).pop();
    state.configLoaded = true;
    state.configDirty = false;
  } catch (error) {
    toast(`读取配置失败：${error.message}`, true);
  }
}

function renderConfigValidation() {
  const daemon = state.snapshot?.daemon || {};
  setStatusDot("config-dot", daemon.config_valid ? "online" : "offline");
  setText("config-state", daemon.config_valid ? "配置有效" : "配置无效");
  setText("config-message", daemon.config_message || "等待校验结果");
  if (daemon.config_path) setText("config-path", daemon.config_path);
}

function renderConsoleDisconnected(message) {
  const chip = document.getElementById("header-status");
  chip.textContent = "Console Error";
  chip.className = "status-chip error";
  document.getElementById("last-updated").textContent = message;
}

async function performAction(button, path, message, confirmMessage) {
  if (confirmMessage && !window.confirm(confirmMessage)) return;
  const original = button?.textContent;
  if (button) { button.disabled = true; button.textContent = "处理中…"; }
  try {
    await api(path, { method: "POST", body: {} });
    toast(message);
    await delay(350);
    await refreshAll({ quiet: true });
  } catch (error) {
    toast(error.message, true);
  } finally {
    if (button) { button.disabled = false; button.textContent = original; }
  }
}

async function saveConfig() {
  const button = document.getElementById("config-save");
  button.disabled = true;
  button.textContent = "校验中…";
  try {
    await api("/api/config", { method: "PUT", body: { source: document.getElementById("config-editor").value } });
    state.configDirty = false;
    toast("配置已校验并保存；重启 inferd 后生效。 ");
    await refreshAll({ quiet: true });
  } catch (error) {
    toast(`配置未保存：${error.message}`, true);
  } finally {
    button.disabled = false;
    button.textContent = "校验并保存";
  }
}

async function showJob(jobId) {
  try {
    const payload = await api(`/api/jobs/${encodeURIComponent(jobId)}/explain`);
    document.querySelector("#detail-dialog .eyebrow").textContent = "JOB EXPLAIN";
    document.getElementById("dialog-title").textContent = shortId(jobId);
    document.getElementById("dialog-content").textContent = JSON.stringify(payload.result, null, 2);
    document.getElementById("detail-dialog").showModal();
  } catch (error) { toast(error.message, true); }
}

async function showProviderModels(provider) {
  try {
    const payload = await api(`/api/providers/${encodeURIComponent(provider)}/models`);
    const models = payload.result?.models || [];
    const admitted = models.filter(model => model.admitted).length;
    document.querySelector("#detail-dialog .eyebrow").textContent = "PROVIDER MODELS";
    document.getElementById("dialog-title").textContent = `${provider} · ${admitted}/${models.length} 已准入`;
    document.getElementById("dialog-content").textContent = models.map(model => {
      const efforts = (model.supported_reasoning_efforts || []).join(", ") || "—";
      const state = model.admitted ? "已准入" : "仅发现";
      const upgrade = model.upgrade ? `\n  建议升级：${model.upgrade}` : "";
      return `${state}  ${model.model}\n  ${model.display_name || model.id}\n  推理档位：${efforts}${upgrade}`;
    }).join("\n\n") || "当前没有可见模型。";
    document.getElementById("detail-dialog").showModal();
  } catch (error) { toast(error.message, true); }
}

function switchView(view) {
  if (!viewCopy[view]) view = "overview";
  document.querySelectorAll(".nav-item").forEach(item => item.classList.toggle("active", item.dataset.view === view));
  document.querySelectorAll(".view").forEach(panel => panel.classList.toggle("active", panel.dataset.viewPanel === view));
  document.getElementById("view-title").textContent = viewCopy[view][0];
  document.getElementById("view-description").textContent = viewCopy[view][1];
  if (window.location.hash !== `#${view}`) history.replaceState(null, "", `#${view}`);
  if (view === "logs") renderLogs();
}

document.addEventListener("click", event => {
  const nav = event.target.closest("[data-view]");
  if (nav) return switchView(nav.dataset.view);
  const link = event.target.closest("[data-view-link]");
  if (link) return switchView(link.dataset.viewLink);
  const actionButton = event.target.closest("[data-action]");
  if (actionButton) {
    const actions = {
      "daemon-start": ["/api/daemon/start", "inferd 已启动", null],
      "daemon-stop": ["/api/daemon/stop", "inferd 已停止", "确定停止当前控制台管理的 inferd？正在执行的请求可能会中断。"],
      "daemon-restart": ["/api/daemon/restart", "inferd 已重启", "确定重启 inferd 并应用当前配置？"],
      "resources-refresh": ["/api/resources/refresh", "本地模型 Inventory 已刷新", null],
    };
    const args = actions[actionButton.dataset.action];
    if (args) return performAction(actionButton, ...args);
  }
  const explain = event.target.closest("[data-job-explain]");
  if (explain) return showJob(explain.dataset.jobExplain);
  const cancel = event.target.closest("[data-job-cancel]");
  if (cancel) return performAction(cancel, `/api/jobs/${encodeURIComponent(cancel.dataset.jobCancel)}/cancel`, "任务已取消", "确定取消这个任务？");
  const resource = event.target.closest("[data-resource-action]");
  if (resource) {
    const action = resource.dataset.resourceAction;
    const provider = encodeURIComponent(resource.dataset.provider);
    const deployment = encodeURIComponent(resource.dataset.deployment);
    const verb = action === "load" ? "加载" : "卸载";
    return performAction(resource, `/api/resources/${provider}/${deployment}/${action}`, `模型已${verb}`, `确定${verb} ${resource.dataset.deployment}？`);
  }
  const probe = event.target.closest("[data-provider-probe]");
  if (probe) return performAction(probe, `/api/providers/${encodeURIComponent(probe.dataset.providerProbe)}/probe`, "Provider 兼容性检查完成", "兼容性 Probe 会发出一次真实模型请求，云端 Provider 可能产生费用。是否继续？");
  const providerModels = event.target.closest("[data-provider-models]");
  if (providerModels) return showProviderModels(providerModels.dataset.providerModels);
});

document.getElementById("refresh-button").addEventListener("click", () => refreshAll());
document.getElementById("job-search").addEventListener("input", renderJobs);
document.getElementById("job-filter").addEventListener("change", renderJobs);
document.getElementById("log-search").addEventListener("input", renderLogs);
document.getElementById("log-filter").addEventListener("change", renderLogs);
document.getElementById("log-follow").addEventListener("change", renderLogs);
document.getElementById("config-editor").addEventListener("input", () => { state.configDirty = true; });
document.getElementById("config-save").addEventListener("click", saveConfig);
document.getElementById("config-reload").addEventListener("click", () => loadConfig(true));
document.getElementById("dialog-close").addEventListener("click", () => document.getElementById("detail-dialog").close());
window.addEventListener("hashchange", () => switchView(location.hash.slice(1)));

function toast(message, error = false) {
  const item = document.createElement("div");
  item.className = `toast${error ? " error" : ""}`;
  item.textContent = message;
  document.getElementById("toast-region").appendChild(item);
  window.setTimeout(() => item.remove(), 4200);
}

function setText(id, value) { document.getElementById(id).textContent = value; }
function setClass(id, value) { document.getElementById(id).className = value; }
function setStatusDot(id, status) { document.getElementById(id).className = `${id === "pulse-core" ? "pulse-core" : "status-dot"} ${status}`; }
function number(value) { return Number.isFinite(Number(value)) ? Number(value) : 0; }
function formatNumber(value) { return new Intl.NumberFormat("zh-CN", { notation: number(value) >= 10000 ? "compact" : "standard", maximumFractionDigits: 1 }).format(number(value)); }
function formatUsd(value) { return `$${number(value).toFixed(number(value) < .01 ? 4 : 2)}`; }
function formatMilliseconds(value) { return value >= 1000 ? `${(value / 1000).toFixed(1)}s` : `${Math.round(value)}ms`; }
function formatDuration(seconds) { if (seconds === undefined || seconds === null) return "—"; const hours = Math.floor(seconds / 3600); const minutes = Math.floor((seconds % 3600) / 60); return hours ? `${hours}h ${minutes}m` : `${minutes}m`; }
function formatBytes(value) { const bytes = number(value); if (!bytes) return "—"; const units = ["B", "KiB", "MiB", "GiB", "TiB"]; const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1); return `${(bytes / (1024 ** index)).toFixed(index > 2 ? 1 : 0)} ${units[index]}`; }
function formatClock(value, milliseconds = false) { const date = new Date(number(value)); return new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit", ...(milliseconds ? { fractionalSecondDigits: 3 } : {}) }).format(date); }
function formatRelative(value) { if (!value) return "—"; const seconds = Math.max(0, Math.floor((Date.now() - number(value)) / 1000)); if (seconds < 60) return `${seconds}s 前`; if (seconds < 3600) return `${Math.floor(seconds / 60)}m 前`; if (seconds < 86400) return `${Math.floor(seconds / 3600)}h 前`; return new Date(number(value)).toLocaleDateString("zh-CN"); }
function shortId(value) { if (!value) return "—"; return value.length > 22 ? `${value.slice(0, 9)}…${value.slice(-8)}` : value; }
function statusClass(value) { const normalized = String(value || "neutral").toLowerCase(); return ["ready", "succeeded", "healthy", "normal", "configured"].includes(normalized) ? "ready" : ["failed", "error", "offline", "expired", "critical", "circuit_open"].includes(normalized) ? "error" : ["queued", "loading", "warn", "warning", "elevated", "draining"].includes(normalized) ? "warning" : normalized === "running" ? "running" : "neutral"; }
function pressureLabel(value) { return ({ normal: "正常", elevated: "偏高", critical: "严重", unknown: "未知" })[value] || value || "未知"; }
function evictionStatusText(value) { return ({ disabled: "清退策略已关闭。", no_pressure_trigger: "当前没有资源压力触发。", no_target_configured: "当前压力等级没有配置释放目标。", insufficient_pressure_data: "主机压力数据不足，保持保守。", target_already_met: "可用内存已达到策略目标。" })[value?.status] || "当前没有可执行建议。"; }
function escapeHtml(value) { return String(value ?? "").replace(/[&<>'"]/g, character => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[character]); }
function escapeAttribute(value) { return escapeHtml(value); }
function delay(milliseconds) { return new Promise(resolve => window.setTimeout(resolve, milliseconds)); }

window.InferConsole = Object.freeze({ api, toast, refreshAll, escapeHtml, escapeAttribute });
switchView(location.hash.slice(1) || "overview");
refreshAll();
window.setInterval(() => refreshAll({ quiet: true }), 2000);
