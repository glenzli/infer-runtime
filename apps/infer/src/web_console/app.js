"use strict";

const csrf = document.querySelector('meta[name="infer-console-session"]').content;
const state = {
  snapshot: null,
  logs: [],
  configLoaded: false,
  configDirty: false,
  refreshing: false,
  subscriptionCatalogs: {},
  subscriptionRefreshAt: 0,
  lastDaemonPid: null,
};

const viewCopy = {
  overview: ["运行总览", "inferd、资源与近期任务。"],
  statistics: ["运行统计", "吞吐、并发与已结算用量。"],
  jobs: ["任务记录", "路由、状态与耗时。"],
  models: ["能力与资源", "按 Intent 查看模型与资源。"],
  access: ["Apps 与访问", "管理接入应用与权限。"],
  logs: ["进程日志", "当前 Console 管理的 inferd 输出。"],
  config: ["Runtime 配置", "保存后重启 inferd 生效。"],
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
    const daemonPid = snapshotPayload.daemon?.pid || null;
    if (daemonPid !== state.lastDaemonPid) {
      state.subscriptionCatalogs = {};
      state.subscriptionRefreshAt = 0;
      state.lastDaemonPid = daemonPid;
    }
    state.logs = logPayload.logs || [];
    renderAll();
    void refreshSubscriptionCatalogs();
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

async function refreshSubscriptionCatalogs() {
  const providers = endpoint("providers")?.providers || endpoint("providers") || [];
  if (!Array.isArray(providers) || !state.snapshot?.daemon?.reachable) return;
  const now = Date.now();
  if (now - state.subscriptionRefreshAt < 60_000) return;
  state.subscriptionRefreshAt = now;
  const codexProviders = providers.filter(provider => provider.kind === "codex_app_server");
  const results = await Promise.allSettled(codexProviders.map(provider =>
    api(`/api/providers/${encodeURIComponent(provider.id)}/models`)));
  results.forEach((result, index) => {
    if (result.status === "fulfilled") {
      state.subscriptionCatalogs[codexProviders[index].id] = result.value.result;
    }
  });
  renderProviders();
  renderResources();
}

function renderAll() {
  renderDaemon();
  renderMetrics();
  renderCharts();
  renderOverviewJobs();
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
  const unavailableDependencies = (daemon.provider_readiness || []).filter(readiness => readiness.status === "unavailable");
  setStatusDot("pulse-core", online ? "online" : "offline");
  const chip = document.getElementById("header-status");
  chip.textContent = online ? "inferd 可用" : "inferd 离线";
  chip.className = `status-chip ${online ? "online" : "offline"}`;
  document.getElementById("offline-banner").classList.toggle("hidden", online);
  document.getElementById("hero-state").textContent = online ? "推理服务运行正常" : "inferd 尚未启动";
  document.getElementById("hero-summary").textContent = online
    ? ownership === "console"
      ? `inferd 由此 Console 管理 · PID ${daemon.pid || "—"} · 已运行 ${formatDuration(daemon.uptime_seconds)}。状态每 2 秒刷新。`
      : "已连接到外部 inferd。此 Console 只读取状态，不会停止或重启该进程。"
    : daemon.config_valid
      ? unavailableDependencies.length
        ? `配置有效，但 ${unavailableDependencies.length} 个 Provider 缺少本机运行依赖；inferd 仍可启动，相关路由会保持不可用。`
        : "配置已通过校验，可以从这里启动一个由控制台管理的 inferd。"
      : "当前配置未通过校验，请先在配置页修正后再启动。";

  document.querySelectorAll('[data-action="daemon-start"]').forEach(button => {
    button.disabled = online || !daemon.config_valid;
    button.classList.toggle("hidden", online);
  });
  document.querySelectorAll('[data-action="daemon-stop"], [data-action="daemon-restart"]').forEach(button => { button.disabled = ownership !== "console"; });
  document.querySelectorAll(".daemon-online-action").forEach(button => button.classList.toggle("hidden", !online));
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
  renderHistoryChart("overview-chart", 24, "overview-chart-time");
  renderHistoryChart("statistics-chart", 24, "statistics-chart-time");
  const metrics = endpoint("metrics") || {};
  const queues = Object.entries(metrics.provider_queues || {});
  const target = document.getElementById("queue-statistics");
  if (!queues.length) {
    target.className = "queue-list empty-state";
    target.textContent = endpointError("metrics") || "暂无队列数据";
    return;
  }
  target.className = "queue-list";
  const capacity = metrics.node_capacity || {};
  const capacityDimensions = [
    capacity.cpu_slots && `CPU ${number(capacity.cpu_slots.reserved)}/${number(capacity.cpu_slots.limit)}`,
    capacity.unified_memory_mib && `统一内存 ${number(capacity.unified_memory_mib.reserved)}/${number(capacity.unified_memory_mib.limit)} MiB`,
    capacity.accelerator_slots && `加速器 ${number(capacity.accelerator_slots.reserved)}/${number(capacity.accelerator_slots.limit)}`,
  ].filter(Boolean);
  const capacityRow = capacityDimensions.length
    ? `<div class="queue-row queue-capacity"><div class="queue-row-header"><strong>共享资源池</strong><span>${escapeHtml(capacityDimensions.join(" · "))} · ${number(capacity.pending)} 等待</span></div></div>`
    : `<div class="queue-row queue-capacity"><div class="queue-row-header"><strong>共享资源池</strong><span>未启用；各 Provider 仅受自身并发槽位限制</span></div></div>`;
  target.innerHTML = capacityRow + queues.map(([provider, queue]) => {
    const active = number(queue.active);
    const pending = number(queue.pending_interactive) + number(queue.pending_normal) + number(queue.pending_background);
    const capacity = Math.max(1, number(queue.max_concurrency) || 1);
    const visibleSlots = Math.min(10, capacity);
    const wait = queue.estimated_wait_ms;
    const waitLabel = !pending && (wait === null || wait === undefined || number(wait) <= 0)
      ? "空闲"
      : wait === null || wait === undefined
      ? (pending ? "等待估计积累中" : "空闲")
      : `预计等待 ${formatMilliseconds(wait)}`;
    const slots = [];
    for (let index = 0; index < visibleSlots; index += 1) {
      const className = index < Math.min(active, visibleSlots) ? "active" : index < Math.min(active + pending, visibleSlots) ? "pending" : "";
      slots.push(`<i class="${className}"></i>`);
    }
    return `<div class="queue-row"><div class="queue-row-header"><strong>${escapeHtml(provider)}</strong><span>${active}/${capacity} 执行 · ${pending} 排队 · ${escapeHtml(waitLabel)}</span></div><div class="queue-track">${slots.join("")}</div></div>`;
  }).join("");
}

function renderHistoryChart(id, limit, axisId) {
  const target = document.getElementById(id);
  const telemetry = endpoint("telemetry");
  const history = (telemetry?.buckets || []).slice(-limit).map(bucket => ({
    startedAt: number(bucket.started_at_ms),
    width: number(telemetry?.bucket_width_ms),
    succeeded: number(bucket.succeeded),
    failed: number(bucket.failed) + number(bucket.cancelled) + number(bucket.expired),
  }));
  const axis = document.getElementById(axisId);
  const first = history[0];
  const last = history.at(-1);
  if (axis) axis.textContent = first && last
    ? `${formatChartRange(first.startedAt)} — ${formatChartRange(last.startedAt + last.width)}`
    : "";
  const max = Math.max(0, ...history.map(item => item.succeeded + item.failed));
  if (!history.length || max === 0) {
    target.className = target.className.replace(/\s*empty-chart/g, "") + " empty-chart";
    target.innerHTML = `<span>${escapeHtml(endpointError("telemetry") || "过去 24 小时没有完成、失败、取消或超时的任务")}</span>`;
    return;
  }
  target.classList.remove("empty-chart");
  target.innerHTML = history.map(item => {
    const successHeight = item.succeeded ? Math.max(2, (item.succeeded / max) * 100) : 0;
    const failureHeight = item.failed ? Math.max(2, (item.failed / max) * 100) : 0;
    const label = `${formatDateTime(item.startedAt)} — ${formatDateTime(item.startedAt + item.width)}：成功 ${item.succeeded}，失败 ${item.failed}`;
    const failureStyle = failureHeight ? `height:${failureHeight}%` : "display:none";
    const successStyle = successHeight ? `height:${successHeight}%` : "display:none";
    return `<span class="chart-column" title="${escapeAttribute(label)}" aria-label="${escapeAttribute(label)}"><i class="chart-bar failure" style="${failureStyle}"></i><i class="chart-bar success" style="${successStyle}"></i></span>`;
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
  const providerPriority = provider => provider.readiness?.status === "unavailable" || provider.circuit_open ? 0
    : (state.subscriptionCatalogs[provider.id]?.configured_deployments || []).some(model => model.presence?.startsWith("missing_")) ? 1
    : provider.configured ? 3 : 2;
  const visibleProviders = [...providers].sort((left, right) => {
    const leftQueue = queues[left.id] || {};
    const rightQueue = queues[right.id] || {};
    const priority = providerPriority(left) - providerPriority(right);
    if (priority) return priority;
    return (number(rightQueue.active) + number(rightQueue.pending_interactive) + number(rightQueue.pending_normal) + number(rightQueue.pending_background))
      - (number(leftQueue.active) + number(leftQueue.pending_interactive) + number(leftQueue.pending_normal) + number(leftQueue.pending_background));
  }).slice(0, 6);
  const hiddenCount = providers.length - visibleProviders.length;
  target.innerHTML = visibleProviders.map(provider => {
    const queue = queues[provider.id] || {};
    const pending = number(queue.pending_interactive) + number(queue.pending_normal) + number(queue.pending_background);
    const unavailable = provider.readiness?.status === "unavailable";
    const missingCount = (state.subscriptionCatalogs[provider.id]?.configured_deployments || [])
      .filter(model => model.presence?.startsWith("missing_")).length;
    const health = unavailable ? "依赖缺失" : provider.circuit_open ? "熔断" : missingCount ? `${missingCount} 个模型缺席` : provider.configured ? "可用" : "未配置";
    const healthClass = unavailable || provider.circuit_open ? "error" : missingCount ? "warning" : provider.configured ? "healthy" : "neutral";
    return `<div class="stack-row"><div><strong>${escapeHtml(provider.id)}</strong><small>${escapeHtml(provider.kind)} · ${escapeHtml(placementLabel(provider.placement))} · ${number(queue.active)} 执行 / ${pending} 排队</small></div><span class="status-chip ${healthClass}">${health}</span></div>`;
  }).join("") + (hiddenCount ? `<div class="stack-more">另有 ${hiddenCount} 个 Provider，前往“能力与资源”查看全部。</div>` : "");
}

function renderOverviewJobs() {
  const jobs = endpoint("jobs")?.jobs || [];
  const target = document.getElementById("overview-recent-jobs");
  if (!jobs.length) {
    target.innerHTML = `<div class="empty-state">${escapeHtml(endpointError("jobs") || "暂无持久化任务记录")}</div>`;
    return;
  }
  target.innerHTML = jobs.slice(0, 5).map(job => {
    const time = formatDateTime(job.created_at_ms);
    const detail = [job.app_id || "—", job.intent || "—", job.provider || "—"].join(" · ");
    return `<div class="stack-row"><div><button class="table-action job-id mono" data-job-explain="${escapeAttribute(job.id)}" title="${escapeAttribute(job.id)}">${escapeHtml(shortId(job.id))}</button><small>${escapeHtml(detail)}</small><small class="muted">${escapeHtml(time)} · ${escapeHtml(formatJobElapsed(job))}</small></div><span class="status-chip ${statusClass(job.state)}">${escapeHtml(job.state || "unknown")}</span></div>`;
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
      <td><time datetime="${escapeAttribute(isoTimestamp(job.created_at_ms))}" title="提交：${escapeAttribute(formatDateTime(job.created_at_ms))}">${escapeHtml(formatDateTime(job.created_at_ms))}</time><small class="muted">更新 ${escapeHtml(formatRelative(job.updated_at_ms))} · ${escapeHtml(formatJobElapsed(job))}</small></td>
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
  const allProviders = Array.isArray(providers) ? [...providers] : [];
  for (const nativeProvider of resourceProviders) {
    if (!allProviders.some(provider => provider.id === nativeProvider.provider)) {
      allProviders.push({ id: nativeProvider.provider, kind: nativeProvider.kind, placement: "local", configured: true });
    }
  }
  syncModelIntentFilter(allProviders);
  const filters = modelFilters();
  const providerCatalog = allProviders.map(provider => {
    const resource = resourceMap.get(provider.id);
    const deployments = providerDeployments(provider, resource);
    return {
      provider,
      resource,
      deployments,
      visible: deployments.filter(deployment => deploymentMatchesModelFilters(deployment, provider, filters)),
    };
  });
  const filterActive = hasActiveModelFilters(filters);
  const visibleCatalog = providerCatalog.filter(entry => entry.visible.length || (!filterActive && !entry.deployments.length));
  const cards = visibleCatalog.map(entry => providerCard(
    entry.provider,
    entry.resource,
    entry.visible,
    entry.deployments.length,
    filters,
  ));
  const totalDeployments = providerCatalog.reduce((sum, entry) => sum + entry.deployments.length, 0);
  const visibleDeployments = providerCatalog.reduce((sum, entry) => sum + entry.visible.length, 0);
  const visibleProviders = providerCatalog.filter(entry => entry.visible.length).length;
  setText("model-filter-count", `${visibleDeployments} / ${totalDeployments} 个 Deployment · ${visibleProviders} 个 Provider`);
  setText("resource-summary", `${visibleDeployments} / ${totalDeployments} 个 Deployment · ${resident.length} 个驻留`);
  const filterSummary = modelFilterSummary(filters);
  const pressureSummary = pressure.last_error || `主机压力 ${pressureLabel(pressureLevel)}，可用内存 ${free ?? "未知"}%`;
  setText("resource-detail", filterSummary ? `${filterSummary} · ${pressureSummary}` : pressureSummary);
  document.getElementById("resource-providers").innerHTML = cards.length
    ? cards.join("")
    : `<article class="panel empty-state">${escapeHtml(endpointError("resources") || (totalDeployments ? "没有匹配当前筛选条件的模型。" : "暂无资源数据"))}</article>`;

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

function providerDeployments(provider, resource) {
  const lifecycleModels = resource?.model_lifecycle || [];
  const configuredDeployments = Array.isArray(provider.deployments) ? provider.deployments : [];
  return configuredDeployments.length
    ? configuredDeployments
    : lifecycleModels.map(model => ({ id: model.deployment, model: model.model_id }));
}

function providerCard(provider, resource, deployments, totalDeployments, filters) {
  const lifecycleModels = resource?.model_lifecycle || [];
  const lifecycleByDeployment = new Map(lifecycleModels.map(model => [model.deployment, model]));
  const subscription = state.subscriptionCatalogs[provider.id];
  const subscriptionByDeployment = new Map((subscription?.configured_deployments || []).map(model => [model.deployment, model]));
  const missingModels = (subscription?.configured_deployments || []).filter(model => model.presence?.startsWith("missing_"));
  const available = resource?.available_deployments?.length || 0;
  const readinessUnavailable = provider.readiness?.status === "unavailable";
  const providerState = readinessUnavailable ? "unavailable" : provider.circuit_open ? "熔断" : missingModels.length ? "warning" : resource?.state || (provider.configured ? "configured" : "unconfigured");
  const rows = deployments.length ? deployments.map(deployment => {
    const lifecycle = lifecycleByDeployment.get(deployment.id);
    const subscriptionModel = subscriptionByDeployment.get(deployment.id);
    const modelIdentity = deployment.model || lifecycle?.model_id || "—";
    const coverage = deploymentCoverage(deployment, filters);
    const supply = deployment.source_kind
      ? `${deployment.source_kind} · license:${deployment.license_status || "unreviewed"}`
      : "";
    const details = [compactModelIdentity(modelIdentity), deployment.model_profile, supply, coverage]
      .filter(Boolean)
      .join(" · ");
    const state = subscriptionModel?.presence?.startsWith("missing_")
      ? subscriptionModel.presence
      : lifecycle?.state || (readinessUnavailable || provider.circuit_open || !provider.configured ? "unavailable" : "admitted");
    const canLoad = lifecycle && !["ready", "loading"].includes(lifecycle.state);
    const canUnload = lifecycle && ["ready", "draining"].includes(lifecycle.state) && number(lifecycle.active_reservations) === 0;
    const lifecycleDetail = lifecycle
      ? ` · ${formatBytes(lifecycle.resident_memory_bytes)} · ${number(lifecycle.active_reservations)} 个预约`
      : "";
    const actions = lifecycle ? `<button class="mini-button" data-resource-action="load" data-provider="${escapeAttribute(provider.id)}" data-deployment="${escapeAttribute(deployment.id)}" ${canLoad ? "" : "disabled"}>加载</button><button class="mini-button" data-resource-action="unload" data-provider="${escapeAttribute(provider.id)}" data-deployment="${escapeAttribute(deployment.id)}" ${canUnload ? "" : "disabled"}>卸载</button>` : "";
    return `<div class="model-row"><div class="model-main"><strong title="${escapeAttribute(deployment.id)}">${escapeHtml(deployment.id)}</strong><small title="${escapeAttribute(`${modelIdentity}${coverage ? ` · ${coverage}` : ""}`)}">${escapeHtml(details)}${escapeHtml(lifecycleDetail)}</small></div><div class="model-actions"><span class="status-chip ${statusClass(state)}">${escapeHtml(modelStateLabel(state))}</span>${actions}</div></div>`;
  }).join("") : `<div class="empty-state">该 Provider 当前没有已准入的 Deployment。</div>`;
  const probe = deployments.length && ["responses", "codex_app_server"].includes(provider.kind) ? `<button class="mini-button" data-provider-probe="${escapeAttribute(provider.id)}">兼容性 Probe</button>` : "";
  const catalog = provider.kind === "codex_app_server" ? `<button class="mini-button" data-provider-models="${escapeAttribute(provider.id)}">查看模型组</button>` : "";
  const access = provider.access_class && provider.access_class !== "standard" ? ` · ${provider.access_class}` : "";
  const filtered = deployments.length !== totalDeployments ? `${deployments.length}/${totalDeployments} 个匹配` : `${totalDeployments} 个已准入`;
  const dependencySummary = readinessUnavailable ? ` · ${provider.readiness.summary || "本机运行依赖不可用"}` : "";
  const availability = `${filtered}${lifecycleModels.length ? ` · ${available} 个可用` : ""}${missingModels.length ? ` · ${missingModels.length} 个上游缺席` : ""}${subscription?.observation_error ? " · 清单刷新失败" : ""}${dependencySummary}`;
  const modes = provider.execution_modes?.join(" / ") || "unary";
  const tools = catalog || probe ? `<div class="provider-tools">${catalog}${probe}</div>` : "";
  return `<article class="panel provider-card"><div class="provider-card-header"><div><strong>${escapeHtml(provider.id)}</strong><small>${escapeHtml(provider.kind || "unknown")} · ${escapeHtml(placementLabel(provider.placement))}${escapeHtml(access)} · ${escapeHtml(modes)} · ${escapeHtml(availability)}</small></div><span class="status-chip ${statusClass(providerState)}">${escapeHtml(providerStateLabel(providerState))}</span>${tools}</div>${rows}</article>`;
}

function deploymentCoverage(deployment, filters = { intent: "all", capability: "all" }) {
  const ratings = Object.entries(deployment.ratings || {}).filter(([intent, rating]) =>
    (filters.intent === "all" || intent === filters.intent)
      && (filters.capability === "all" || rating.level === filters.capability));
  if (!ratings.length) return "";
  const labels = ratings.map(([intent, rating]) => `${intent}:${rating.level || "—"}`);
  return labels.length <= 3 ? labels.join(" / ") : `${labels.slice(0, 3).join(" / ")} +${labels.length - 3}`;
}

function syncModelIntentFilter(providers) {
  const select = document.getElementById("model-intent-filter");
  const intents = [...new Set(providers.flatMap(provider =>
    (provider.deployments || []).flatMap(deployment => Object.keys(deployment.ratings || {}))))].sort();
  const signature = intents.join("\n");
  if (select.dataset.options === signature) return;
  const selected = select.value;
  select.innerHTML = `<option value="all">全部 Intent (${intents.length})</option>${intents.map(intent =>
    `<option value="${escapeAttribute(intent)}">${escapeHtml(intent)}</option>`).join("")}`;
  select.value = intents.includes(selected) ? selected : "all";
  select.dataset.options = signature;
}

function modelFilters() {
  return {
    query: document.getElementById("model-search").value.trim().toLowerCase(),
    intent: document.getElementById("model-intent-filter").value,
    capability: document.getElementById("model-capability-filter").value,
    placement: document.getElementById("model-placement-filter").value,
  };
}

function hasActiveModelFilters(filters) {
  return Boolean(filters.query) || [filters.intent, filters.capability, filters.placement].some(value => value !== "all");
}

function deploymentMatchesModelFilters(deployment, provider, filters) {
  if (filters.placement !== "all" && provider.placement !== filters.placement) return false;
  const ratings = Object.entries(deployment.ratings || {});
  const scopedRatings = filters.intent === "all"
    ? ratings
    : ratings.filter(([intent]) => intent === filters.intent);
  if (filters.intent !== "all" && !scopedRatings.length) return false;
  if (filters.capability !== "all" && !scopedRatings.some(([, rating]) => rating.level === filters.capability)) return false;
  if (!filters.query) return true;
  const haystack = [
    provider.id,
    provider.kind,
    provider.placement,
    deployment.id,
    deployment.model,
    deployment.model_profile,
    deployment.build,
    ...ratings.flatMap(([intent, rating]) => [intent, rating.level, rating.status]),
  ].filter(Boolean).join(" ").toLowerCase();
  return haystack.includes(filters.query);
}

function modelFilterSummary(filters) {
  const parts = [];
  if (filters.intent !== "all") parts.push(`Intent ${filters.intent}`);
  if (filters.capability !== "all") parts.push(`档位 ${filters.capability}`);
  if (filters.placement !== "all") parts.push(`位置 ${filters.placement}`);
  if (filters.query) parts.push(`搜索“${filters.query}”`);
  return parts.join(" · ");
}

function modelStateLabel(state) {
  return ({
    admitted: "已准入",
    absent: "未加载",
    ready: "已驻留",
    loading: "加载中",
    draining: "待释放",
    unavailable: "不可用",
    unknown: "待检测",
    missing_suspected: "上游缺席，待复核",
    missing_confirmed: "上游持续缺席",
  })[state] || state || "未知";
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
  const unavailable = (daemon.provider_readiness || []).filter(readiness => readiness.status === "unavailable");
  setStatusDot("config-dot", daemon.config_valid ? "online" : "offline");
  setText("config-state", daemon.config_valid ? (unavailable.length ? "配置有效，Provider 待修复" : "配置有效") : "配置无效");
  const dependencyMessage = unavailable.map(readiness => {
    const failed = (readiness.checks || []).find(check => check.status === "unavailable");
    const searched = failed?.searched_paths?.length ? `；已检查 ${failed.searched_paths.join(", ")}` : "";
    return `${readiness.provider}: ${readiness.summary}${searched}`;
  }).join("；");
  setText("config-message", dependencyMessage || daemon.config_message || "等待校验结果");
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
    document.getElementById("dialog-title").textContent = `任务详情 · ${shortId(jobId)}`;
    document.getElementById("dialog-content").textContent = JSON.stringify(payload.result, null, 2);
    document.getElementById("detail-dialog").showModal();
  } catch (error) { toast(error.message, true); }
}

async function showProviderModels(provider) {
  try {
    const payload = await api(`/api/providers/${encodeURIComponent(provider)}/models`);
    const models = payload.result?.models || [];
    const configured = payload.result?.configured_deployments || [];
    state.subscriptionCatalogs[provider] = payload.result;
    const admitted = models.filter(model => model.admitted).length;
    const missing = configured.filter(model => model.presence?.startsWith("missing_"));
    document.getElementById("dialog-title").textContent = `${provider} · ${admitted}/${models.length} 已准入 · ${missing.length} 个配置模型缺席`;
    const observedText = payload.result?.last_success_unix_ms
      ? `上次完整清单：${formatClock(payload.result.last_success_unix_ms)}`
      : "尚无完整清单";
    const observationText = payload.result?.observation_error ? `${observedText} · 最近一次刷新失败` : observedText;
    const configuredText = configured.map(model => {
      const label = modelStateLabel(model.presence);
      return `${label}  ${model.deployment} · ${model.model}`;
    }).join("\n");
    const discoveredText = models.map(model => {
      const efforts = (model.supported_reasoning_efforts || []).join(", ") || "—";
      const state = model.admitted ? "已准入" : "仅发现";
      const upgrade = model.upgrade ? `\n  建议升级：${model.upgrade}` : "";
      return `${state}  ${model.model}\n  ${model.display_name || model.id}\n  推理档位：${efforts}${upgrade}`;
    }).join("\n\n") || (payload.result?.observation_error ? "清单暂不可用。" : "当前没有可见模型。");
    document.getElementById("dialog-content").textContent =
      `${observationText}\n\n已配置 Deployment\n${configuredText || "无"}\n\n当前上游清单\n${discoveredText}`;
    renderProviders();
    renderResources();
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
  if (cancel) return performAction(cancel, `/api/jobs/${encodeURIComponent(cancel.dataset.jobCancel)}/cancel`, "任务已取消并已记录", "确定取消这个任务？");
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
document.getElementById("model-search").addEventListener("input", renderResources);
document.getElementById("model-intent-filter").addEventListener("change", renderResources);
document.getElementById("model-capability-filter").addEventListener("change", renderResources);
document.getElementById("model-placement-filter").addEventListener("change", renderResources);
document.getElementById("model-filter-reset").addEventListener("click", () => {
  document.getElementById("model-search").value = "";
  document.getElementById("model-intent-filter").value = "all";
  document.getElementById("model-capability-filter").value = "all";
  document.getElementById("model-placement-filter").value = "all";
  renderResources();
});
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
function formatDateTime(value) { if (!value) return "—"; return new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(new Date(number(value))); }
function formatChartRange(value) { if (!value) return "—"; return new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" }).format(new Date(number(value))); }
function isoTimestamp(value) { return value ? new Date(number(value)).toISOString() : ""; }
function formatJobElapsed(job) {
  if (!job?.created_at_ms) return "耗时 —";
  const terminal = ["succeeded", "failed", "cancelled", "expired"].includes(job.state);
  const end = terminal ? number(job.updated_at_ms) : Date.now();
  const elapsed = Math.max(0, end - number(job.created_at_ms));
  return `${terminal ? "耗时" : "已运行"} ${formatElapsedMilliseconds(elapsed)}`;
}
function formatElapsedMilliseconds(value) {
  const seconds = Math.floor(Math.max(0, number(value)) / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}
function formatRelative(value) { if (!value) return "—"; const seconds = Math.max(0, Math.floor((Date.now() - number(value)) / 1000)); if (seconds < 60) return `${seconds}s 前`; if (seconds < 3600) return `${Math.floor(seconds / 60)}m 前`; if (seconds < 86400) return `${Math.floor(seconds / 3600)}h 前`; return new Date(number(value)).toLocaleDateString("zh-CN"); }
function shortId(value) { if (!value) return "—"; return value.length > 22 ? `${value.slice(0, 9)}…${value.slice(-8)}` : value; }
function statusClass(value) { const normalized = String(value || "neutral").toLowerCase(); return ["ready", "succeeded", "healthy", "normal", "configured"].includes(normalized) ? "ready" : ["failed", "error", "offline", "expired", "critical", "circuit_open", "missing_confirmed"].includes(normalized) ? "error" : ["queued", "loading", "warn", "warning", "elevated", "draining", "missing_suspected"].includes(normalized) ? "warning" : normalized === "running" ? "running" : "neutral"; }
function placementLabel(value) { return ({ local: "本地", cloud: "云端", trusted_node: "受信节点" })[value] || value || "—"; }
function providerStateLabel(value) { return ({ configured: "已配置", unconfigured: "未配置", unavailable: "不可用", unknown: "待检测", warning: "模型缺席" })[value] || value || "未知"; }
function pressureLabel(value) { return ({ normal: "正常", elevated: "偏高", critical: "严重", unknown: "未知" })[value] || value || "未知"; }
function evictionStatusText(value) { return ({ disabled: "清退策略已关闭。", no_pressure_trigger: "当前没有资源压力触发。", no_target_configured: "当前压力等级没有配置释放目标。", insufficient_pressure_data: "主机压力数据不足，保持保守。", target_already_met: "可用内存已达到策略目标。" })[value?.status] || "当前没有可执行建议。"; }
function escapeHtml(value) { return String(value ?? "").replace(/[&<>'"]/g, character => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[character]); }
function escapeAttribute(value) { return escapeHtml(value); }
function delay(milliseconds) { return new Promise(resolve => window.setTimeout(resolve, milliseconds)); }

window.InferConsole = Object.freeze({ api, toast, refreshAll, escapeHtml, escapeAttribute });
switchView(location.hash.slice(1) || "overview");
refreshAll();
window.setInterval(() => refreshAll({ quiet: true }), 2000);
