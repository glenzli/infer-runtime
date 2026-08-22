"use strict";

(() => {
  const { api, toast, escapeHtml, escapeAttribute } = window.InferConsole;
  const state = { apps: [], intents: [], restartRequired: false, mode: "create", editing: null, loading: false };

  const presets = {
    "local-private": {
      defaultPolicy: "local-first",
      policies: ["local-first"],
      priority: ["interactive", "normal", "background"],
      placement: ["local_only"],
      prefer: ["local"],
      offline: true,
      capability_floor: ["foundational", "capable", "advanced", "expert"],
      latency: ["interactive", "balanced", "throughput"],
      fallback: ["none"],
      maxCost: 0,
    },
    "local-preferred": {
      defaultPolicy: "local-first",
      policies: ["balanced", "local-first", "cost-first"],
      priority: ["interactive", "normal", "background"],
      placement: ["local_only", "private", "anywhere"],
      prefer: ["local", "cloud"],
      offline: true,
      capability_floor: ["foundational", "capable", "advanced", "expert"],
      latency: ["interactive", "balanced", "throughput"],
      fallback: ["none", "equivalent"],
      maxCost: 1,
    },
    hybrid: {
      defaultPolicy: "balanced",
      policies: ["balanced", "local-first", "capability-first", "latency-first", "cost-first"],
      priority: ["interactive", "normal", "background"],
      placement: ["local_only", "private", "anywhere", "cloud_only"],
      prefer: ["local", "trusted_node", "cloud"],
      offline: true,
      capability_floor: ["foundational", "capable", "advanced", "expert", "exceptional"],
      latency: ["interactive", "balanced", "throughput"],
      fallback: ["none", "equivalent", "allow_lower_capability"],
      maxCost: 5,
    },
  };

  async function loadApps({ quiet = false } = {}) {
    if (state.loading) return;
    state.loading = true;
    try {
      const payload = await api("/api/access/apps");
      state.apps = payload.result.apps || [];
      state.intents = payload.result.intents || [];
      state.restartRequired = Boolean(payload.result.restart_required);
      renderIntentOptions();
      renderApps();
    } catch (error) {
      document.getElementById("access-apps").innerHTML = `<article class="panel"><div class="empty-state">${escapeHtml(error.message)}</div></article>`;
      if (!quiet) toast(`读取访问配置失败：${error.message}`, true);
    } finally {
      state.loading = false;
    }
  }

  function renderApps() {
    const managed = state.apps.filter(app => app.credential_source === "managed").length;
    const external = state.apps.filter(app => app.credential_source === "environment").length;
    const pending = state.apps.filter(app => app.pending_restart).length;
    setText("access-total", state.apps.length);
    setText("access-managed", managed);
    setText("access-external", external);
    setText("access-pending", state.restartRequired ? Math.max(1, pending) : 0);
    document.getElementById("access-restart-banner").classList.toggle("hidden", !state.restartRequired);

    const target = document.getElementById("access-apps");
    if (!state.apps.length) {
      target.innerHTML = `<article class="panel"><div class="empty-state">还没有登记任何 App。</div></article>`;
      return;
    }
    target.innerHTML = state.apps.map(renderAppCard).join("");
  }

  function renderAppCard(app) {
    const source = app.credential_source === "managed" ? "托管令牌" : "外部凭据";
    const credentialLabel = ({ ready: "凭证就绪", missing: "尚未生成", invalid: "凭证异常", external: "环境注入" })[app.credential_state] || app.credential_state;
    const credentialClass = app.credential_state === "ready" || app.credential_state === "external" ? "ready" : app.credential_state === "missing" ? "warning" : "error";
    const permission = summarizePermission(app.request_overrides || {});
    const policy = app.allowed_policies?.length ? app.allowed_policies.join(", ") : "不允许请求覆盖策略";
    const intents = app.allowed_intents === null || app.allowed_intents === undefined
      ? app.resource_admin
        ? "全部 Intent（显式 Operator 授权）"
        : "禁止提交推理任务（未配置 ACL）"
      : app.allowed_intents.length ? app.allowed_intents.join(", ") : "禁止提交推理任务";
    const providerAccess = app.allowed_provider_access_classes?.length
      ? app.allowed_provider_access_classes.join(", ")
      : "standard";
    const builtinTools = app.allowed_builtin_tools?.length
      ? app.allowed_builtin_tools.join(", ")
      : "禁止托管工具";
    const cloudInputs = app.allowed_cloud_input_modalities?.length
      ? app.allowed_cloud_input_modalities.join(", ")
      : "禁止向云端发送 payload";
    const identity = app.resource_admin ? "本机管理" : "应用";
    const pending = app.pending_restart ? `<span class="status-chip warning">等待重启</span>` : "";
    const protectedBadge = app.protected ? `<span class="status-chip neutral">受保护</span>` : "";
    const tokenIdentity = app.fingerprint || app.environment_variable || "—";
    const message = app.credential_message ? `<p class="access-warning">${escapeHtml(app.credential_message)}</p>` : "";
    const actions = app.protected ? "" : `<div class="access-card-actions">
      <button class="mini-button" data-access-edit="${escapeAttribute(app.app_id)}">编辑权限</button>
      ${app.credential_source === "managed" ? `<button class="mini-button" data-access-rotate="${escapeAttribute(app.app_id)}">${app.credential_state === "missing" ? "生成令牌" : "轮换令牌"}</button>` : ""}
      <button class="mini-button danger" data-access-revoke="${escapeAttribute(app.app_id)}">${app.credential_source === "managed" ? "撤销访问" : "移除登记"}</button>
    </div>`;

    return `<article class="panel access-card${app.pending_restart ? " pending" : ""}">
      <div class="access-card-header">
        <div><div class="eyebrow">${escapeHtml(identity.toUpperCase())}</div><h3 class="mono">${escapeHtml(app.app_id)}</h3></div>
        <div class="access-badges">${pending}${protectedBadge}<span class="status-chip ${credentialClass}">${escapeHtml(credentialLabel)}</span></div>
      </div>
      <dl class="access-details">
        <div><dt>凭证来源</dt><dd>${escapeHtml(source)}</dd></div>
        <div><dt>指纹 / 环境变量</dt><dd class="mono">${escapeHtml(tokenIdentity)}</dd></div>
        <div><dt>默认策略</dt><dd class="mono">${escapeHtml(app.default_policy || "—")}</dd></div>
        <div><dt>最大排队任务数</dt><dd>${escapeHtml(app.max_pending_jobs)}</dd></div>
      </dl>
      <div class="access-policy"><span>允许 Intent</span><code>${escapeHtml(intents)}</code></div>
      <div class="access-policy"><span>Provider 访问</span><code>${escapeHtml(providerAccess)}</code></div>
      <div class="access-policy"><span>托管工具</span><code>${escapeHtml(builtinTools)}</code></div>
      <div class="access-policy"><span>云端输入</span><code>${escapeHtml(cloudInputs)}</code></div>
      <div class="access-policy"><span>允许策略</span><code>${escapeHtml(policy)}</code></div>
      <div class="access-policy"><span>请求约束</span><code>${escapeHtml(permission)}</code></div>
      ${message}${actions}
    </article>`;
  }

  function summarizePermission(overrides) {
    const parts = [];
    if (overrides.placement?.length) parts.push(`placement: ${overrides.placement.join("/")}`);
    if (overrides.capability_floor?.length) parts.push(`capability: ${overrides.capability_floor.join("/")}`);
    if (overrides.fallback?.length) parts.push(`fallback: ${overrides.fallback.join("/")}`);
    if (overrides.offline_required) parts.push("offline_required");
    const maxCost = overrides.max_cost_usd?.max;
    if (maxCost !== undefined && maxCost !== null) parts.push(`max $${Number(maxCost).toFixed(2)}`);
    return parts.join(" · ") || "不允许 infer.* 请求覆盖";
  }

  function openCreate() {
    state.mode = "create";
    state.editing = null;
    document.getElementById("access-dialog-title").textContent = "创建 Consumer";
    document.getElementById("access-submit").textContent = "创建并生成令牌";
    const appId = document.getElementById("access-app-id");
    appId.value = "";
    appId.disabled = false;
    document.getElementById("access-max-pending").value = "16";
    document.getElementById("access-preset").value = "local-private";
    document.getElementById("access-subscription-providers").checked = false;
    document.getElementById("access-web-search").checked = false;
    document.getElementById("access-cloud-images").checked = false;
    renderIntentOptions();
    setPermission("intents", []);
    applyPreset("local-private");
    document.getElementById("access-dialog").showModal();
    appId.focus();
  }

  function openEdit(appId) {
    const app = state.apps.find(item => item.app_id === appId);
    if (!app || app.protected) return;
    state.mode = "edit";
    state.editing = app;
    document.getElementById("access-dialog-title").textContent = `编辑 ${app.app_id}`;
    document.getElementById("access-submit").textContent = "保存权限";
    const idInput = document.getElementById("access-app-id");
    idInput.value = app.app_id;
    idInput.disabled = true;
    document.getElementById("access-max-pending").value = String(app.max_pending_jobs);
    document.getElementById("access-default-policy").value = app.default_policy || "balanced";
    setPermission("intents", app.allowed_intents ?? state.intents);
    setPermission("policies", app.allowed_policies || []);
    for (const name of ["priority", "placement", "prefer", "capability_floor", "latency", "fallback"]) {
      setPermission(name, app.request_overrides?.[name] || []);
    }
    document.getElementById("access-offline").checked = Boolean(app.request_overrides?.offline_required);
    document.getElementById("access-max-cost").value = String(app.request_overrides?.max_cost_usd?.max ?? 0);
    document.getElementById("access-subscription-providers").checked =
      app.allowed_provider_access_classes?.includes("subscription") || false;
    document.getElementById("access-web-search").checked =
      app.allowed_builtin_tools?.includes("web_search") || false;
    document.getElementById("access-cloud-images").checked =
      app.allowed_cloud_input_modalities?.includes("image") || false;
    document.getElementById("access-preset").value = "custom";
    document.getElementById("access-dialog").showModal();
  }

  function applyPreset(name) {
    const preset = presets[name];
    if (!preset) return;
    document.getElementById("access-default-policy").value = preset.defaultPolicy;
    setPermission("policies", preset.policies);
    for (const permission of ["priority", "placement", "prefer", "capability_floor", "latency", "fallback"]) {
      setPermission(permission, preset[permission]);
    }
    document.getElementById("access-offline").checked = preset.offline;
    document.getElementById("access-max-cost").value = String(preset.maxCost);
    document.getElementById("access-subscription-providers").checked = false;
    document.getElementById("access-web-search").checked = false;
    document.getElementById("access-cloud-images").checked = false;
  }

  function setPermission(name, values) {
    const allowed = new Set(values || []);
    document.querySelectorAll(`[data-permission="${name}"] input[type="checkbox"]`).forEach(input => {
      input.checked = allowed.has(input.value);
    });
  }

  function renderIntentOptions() {
    const target = document.getElementById("access-intents");
    if (!target) return;
    const signature = JSON.stringify(state.intents);
    if (target.dataset.options === signature) return;
    const selected = permissionValues("intents");
    target.innerHTML = state.intents.length
      ? state.intents.map(intent => `<label><input type="checkbox" value="${escapeAttribute(intent)}"> ${escapeHtml(intent)}</label>`).join("")
      : `<span class="muted small">当前配置没有 Intent。</span>`;
    target.dataset.options = signature;
    setPermission("intents", selected);
  }

  function permissionValues(name) {
    return [...document.querySelectorAll(`[data-permission="${name}"] input[type="checkbox"]:checked`)].map(input => input.value);
  }

  function collectInput() {
    const maxCost = Number(document.getElementById("access-max-cost").value || 0);
    return {
      app_id: document.getElementById("access-app-id").value.trim(),
      allowed_intents: permissionValues("intents"),
      allowed_provider_access_classes: document.getElementById("access-subscription-providers").checked
        ? ["standard", "subscription"]
        : ["standard"],
      allowed_builtin_tools: document.getElementById("access-web-search").checked
        ? ["web_search"]
        : [],
      allowed_cloud_input_modalities: document.getElementById("access-cloud-images").checked
        ? ["text", "image"]
        : ["text"],
      max_pending_jobs: Number(document.getElementById("access-max-pending").value),
      default_policy: document.getElementById("access-default-policy").value || null,
      allowed_policies: permissionValues("policies"),
      request_overrides: {
        priority: permissionValues("priority"),
        placement: permissionValues("placement"),
        prefer: permissionValues("prefer"),
        offline_required: document.getElementById("access-offline").checked,
        capability_floor: permissionValues("capability_floor"),
        latency: permissionValues("latency"),
        max_cost_usd: { min: 0, max: maxCost },
        fallback: permissionValues("fallback"),
      },
    };
  }

  async function submitAccess(event) {
    event.preventDefault();
    const input = collectInput();
    const submit = document.getElementById("access-submit");
    submit.disabled = true;
    submit.textContent = state.mode === "create" ? "正在创建…" : "正在保存…";
    try {
      const path = state.mode === "create" ? "/api/access/apps" : `/api/access/apps/${encodeURIComponent(input.app_id)}`;
      const payload = await api(path, { method: state.mode === "create" ? "POST" : "PUT", body: input });
      document.getElementById("access-dialog").close();
      if (state.mode === "create") {
        showCredential(payload.result);
      } else {
        toast(`${input.app_id} 的权限已保存；重启 inferd 后生效。`);
      }
      await loadApps({ quiet: true });
    } catch (error) {
      toast(error.message, true);
    } finally {
      submit.disabled = false;
      submit.textContent = state.mode === "create" ? "创建并生成令牌" : "保存权限";
    }
  }

  async function rotate(appId) {
    const app = state.apps.find(item => item.app_id === appId);
    const verb = app?.credential_state === "missing" ? "生成" : "轮换";
    if (!window.confirm(`${verb} ${appId} 的 Consumer token？当前 daemon 会继续接受旧认证表，直到完成重启。`)) return;
    try {
      const payload = await api(`/api/access/apps/${encodeURIComponent(appId)}/rotate`, { method: "POST", body: {} });
      showCredential(payload.result);
      await loadApps({ quiet: true });
    } catch (error) { toast(error.message, true); }
  }

  async function revoke(appId) {
    const app = state.apps.find(item => item.app_id === appId);
    if (!app) return;
    const noun = app.credential_source === "managed" ? "撤销访问并删除 managed credential" : "移除 App 登记";
    if (!window.confirm(`${noun}：${appId}？当前 daemon 在重启前仍可能接受旧 token。`)) return;
    try {
      const payload = await api(`/api/access/apps/${encodeURIComponent(appId)}`, { method: "DELETE" });
      const warning = payload.result.cleanup_warning;
      toast(warning ? `App 已撤销，但凭证清理需要手工检查：${warning}` : `${appId} 已撤销；重启 inferd 后生效。`, Boolean(warning));
      await loadApps({ quiet: true });
    } catch (error) { toast(error.message, true); }
  }

  function showCredential(result) {
    document.getElementById("credential-app-id").textContent = result.app_id;
    document.getElementById("credential-token").value = result.token;
    document.getElementById("credential-fingerprint").textContent = result.fingerprint;
    document.getElementById("credential-dialog").showModal();
  }

  function closeCredential() {
    const field = document.getElementById("credential-token");
    field.value = "";
    document.getElementById("credential-fingerprint").textContent = "—";
    document.getElementById("credential-app-id").textContent = "—";
    document.getElementById("credential-dialog").close();
  }

  async function copyCredential() {
    const field = document.getElementById("credential-token");
    try {
      await navigator.clipboard.writeText(field.value);
      toast("令牌已复制。请保存到 Consumer 的安全凭证存储。 ");
    } catch (_error) {
      field.focus();
      field.select();
      toast("浏览器未允许自动复制，已选中令牌，请手工复制。", true);
    }
  }

  const providerAccessControl = document.createElement("label");
  providerAccessControl.className = "field field-checkbox";
  providerAccessControl.innerHTML = '<input id="access-subscription-providers" type="checkbox"><span>允许使用订阅式云模型（Codex 等）</span>';
  document.getElementById("access-offline").closest(".form-grid").prepend(providerAccessControl);
  const webSearchControl = document.createElement("label");
  webSearchControl.className = "field field-checkbox";
  webSearchControl.innerHTML = '<input id="access-web-search" type="checkbox"><span>允许托管 Web Search</span>';
  providerAccessControl.after(webSearchControl);
  const cloudImageControl = document.createElement("label");
  cloudImageControl.className = "field field-checkbox";
  cloudImageControl.innerHTML = '<input id="access-cloud-images" type="checkbox"><span>允许图片发送到云端推理</span>';
  webSearchControl.after(cloudImageControl);

  document.getElementById("access-create").addEventListener("click", openCreate);
  document.getElementById("access-dialog-close").addEventListener("click", () => document.getElementById("access-dialog").close());
  document.getElementById("access-form").addEventListener("submit", submitAccess);
  document.getElementById("access-preset").addEventListener("change", event => applyPreset(event.target.value));
  document.getElementById("access-default-policy").addEventListener("change", event => {
    const match = document.querySelector(`[data-permission="policies"] input[value="${CSS.escape(event.target.value)}"]`);
    if (match) match.checked = true;
    document.getElementById("access-preset").value = "custom";
  });
  document.querySelectorAll("#access-form input[type=checkbox], #access-form input[type=number]").forEach(input => {
    input.addEventListener("change", () => { document.getElementById("access-preset").value = "custom"; });
  });
  document.getElementById("credential-copy").addEventListener("click", copyCredential);
  document.getElementById("credential-close").addEventListener("click", closeCredential);
  document.getElementById("credential-dialog").addEventListener("cancel", event => { event.preventDefault(); closeCredential(); });

  document.addEventListener("click", event => {
    const nav = event.target.closest('[data-view="access"], [data-view-link="access"]');
    if (nav) window.setTimeout(() => loadApps({ quiet: true }), 0);
    const edit = event.target.closest("[data-access-edit]");
    if (edit) return openEdit(edit.dataset.accessEdit);
    const rotateButton = event.target.closest("[data-access-rotate]");
    if (rotateButton) return rotate(rotateButton.dataset.accessRotate);
    const revokeButton = event.target.closest("[data-access-revoke]");
    if (revokeButton) return revoke(revokeButton.dataset.accessRevoke);
    if (event.target.closest('[data-action="daemon-restart"], #refresh-button')) {
      window.setTimeout(() => loadApps({ quiet: true }), 1200);
    }
  });

  function setText(id, value) { document.getElementById(id).textContent = String(value); }

  if (location.hash === "#access") loadApps();
  window.setInterval(() => {
    if (location.hash === "#access") loadApps({ quiet: true });
  }, 5000);
})();
