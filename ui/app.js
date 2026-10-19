(() => {
"use strict";
const globalEvents = new AbortController();
const $ = (id) => document.getElementById(id);
const exactNumber = (value) => new Intl.NumberFormat("zh-CN").format(value || 0);
const number = (value) => {
  const amount = Number(value) || 0;
  return Math.abs(amount) >= 1e9
    ? new Intl.NumberFormat("en-US", {maximumFractionDigits:1, useGrouping:false}).format(amount / 1e9) + "B"
    : new Intl.NumberFormat("en-US", {notation:"compact", maximumFractionDigits:1}).format(amount);
};
const money = (nanos) => {
  const amount = (nanos || 0) / 1e9;
  return Math.abs(amount) >= 1000 ? "$" + number(amount)
    : new Intl.NumberFormat("en-US", {style:"currency", currency:"USD", minimumFractionDigits:2, maximumFractionDigits:9}).format(amount);
};
window.UsageFormat = {number, money};
const date = (value) =>
  value ? new Date(value * 1000).toLocaleString("zh-CN") : "—";
let key = "",
  revision = 0,
  epoch = 0,
  pollTimer,
  pendingRefresh;
let latestSettings,
  settingsRevision = 0,
  settingsDirty = false;
let latestModels,
  modelDraft,
  upstreamCatalog,
  modelRevision = 0,
  modelDirty = false;
let defaultProfile = "",
  profiles = [],
  accountNames = new Map(),
  tokenNames = new Map(),
  editAccount = "",
  editRevision = 0;
let logRecords = [],
  logBefore,
  pagedLogs = false,
  logRequest = 0;
const activeRequests = new Set();
const selectedAccounts = new Set();
let latestAccounts = [], nameEdit, batchEdit, tokenEdit;

function notify(text, error = false) {
  $("notice").textContent = text;
  $("notice").classList.toggle("error", error);
  $("notice").hidden = false;
  clearTimeout(notify.timer);
  notify.timer = setTimeout(() => ($("notice").hidden = true), 6500);
}
function connection(connected, text) {
  $("traffic").classList.toggle("connected", connected);
  $("connection").textContent = text || (connected ? "后端已连接" : "连接中断");
}
function clearSession() {
  window.VersionManager?.disconnected();
  epoch++;
  key = "";
  pendingRefresh = null;
  latestSettings = undefined;
  settingsDirty = false;
  latestModels = modelDraft = undefined;
  upstreamCatalog = undefined;
  modelDirty = false;
  editAccount = "";
  selectedAccounts.clear();
  latestAccounts = [];
  nameEdit = batchEdit = tokenEdit = undefined;
  for (const id of ["account-name-form", "batch-proxy-form", "token-edit-form"]) $(id).hidden = true;
  $("batch-proxy-url").value = "";
  $("save-batch-proxy").disabled = $("save-token-edit").disabled = false;
  $("account-name-form").querySelector('button[type="submit"]').disabled = false;
  updateSelection();
  tokenNames.clear();
  window.UsageCharts?.clear();
  window.SelectUI?.close();
  logRecords = [];
  pagedLogs = false;
  for (const id of [
    "account-table",
    "session-table",
    "settings-fields",
    "usage-table",
    "event-table",
    "log-table",
    "metrics",
    "model-table",
    "token-table",
    "usage-trend",
    "usage-legend",
    "usage-donut",
    "cost-breakdown",
    "token-usage-table",
    "upstream-model-table",
  ])
    $(id).replaceChildren();
  $("import-form").hidden = $("transport-form").hidden = true;
  $("token-reveal").hidden = true;
  $("token-secret").value = "";
  $("token-label").value = "";
  $("token-concurrency").value = "1";
  $("token-never").checked = false;
  $("token-expiration").disabled = false;
  $("token-expiration").required = true;
  $("create-token").disabled = false;
  defaultTokenExpiry();
  clearTimeout(pollTimer);
  activeRequests.forEach((controller) => controller.abort());
  activeRequests.clear();
  $("credentials").value =
    $("import-proxy").value =
    $("proxy-url").value =
    $("key").value =
      "";
  $("console").hidden =
    $("logout").hidden =
    $("refresh-controls").hidden =
      true;
  $("login").hidden = false;
  connection(false, "未连接");
  $("last-update").textContent = "尚未同步";
}
async function api(path, method = "GET", body, groupSignal) {
  const controller = new AbortController();
  const abort = () => controller.abort();
  activeRequests.add(controller);
  groupSignal?.addEventListener("abort", abort, { once: true });
  if (groupSignal?.aborted) controller.abort();
  const timeout = setTimeout(abort, 10000);
  try {
    const response = await fetch("/control/v1" + path, {
      method,
      signal: controller.signal,
      cache: "no-store",
      headers: {
        Authorization: "Bearer " + key,
        ...(body ? { "Content-Type": "application/json" } : {}),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    let data;
    try {
      data = await response.json();
    } catch {
      throw new Error("控制接口响应无法读取");
    }
    if (!response.ok) {
      const error = new Error(data.error?.message || "操作失败");
      error.status = response.status;
      throw error;
    }
    if (!data || data.schema_version !== 1)
      throw new Error("控制接口版本不兼容，请刷新页面");
    return data;
  } catch (error) {
    if (error.name === "AbortError") throw new Error("请求超时或连接已取消");
    throw error;
  } finally {
    clearTimeout(timeout);
    activeRequests.delete(controller);
    groupSignal?.removeEventListener("abort", abort);
  }
}
function schedule() {
  clearTimeout(pollTimer);
  if (!key) return;
  pollTimer = setTimeout(
    () => refresh().catch(() => {}),
    Number($("refresh-interval").value) * 1000
  );
}
function table(target, headings, rows) {
  const host = $(target);
  host.replaceChildren();
  if (!rows.length) {
    const empty = document.createElement("p");
    empty.className = "empty";
    empty.textContent = "暂无记录";
    host.append(empty);
    return;
  }
  const element = document.createElement("table"),
    head = document.createElement("thead"),
    line = document.createElement("tr");
  for (const heading of headings) {
    const cell = document.createElement("th");
    cell.textContent = heading;
    line.append(cell);
  }
  head.append(line);
  element.append(head);
  const body = document.createElement("tbody");
  for (const row of rows) {
    const line = document.createElement("tr");
    for (const [index, value] of row.entries()) {
      const cell = document.createElement("td");
      cell.dataset.label = headings[index];
      if (value instanceof Node) cell.append(value);
      else cell.textContent = String(value ?? "—");
      line.append(cell);
    }
    body.append(line);
  }
  element.append(body);
  host.append(element);
}
function tag(text, style = "") {
  const node = document.createElement("span");
  node.textContent = text;
  node.className = "tag " + style;
  return node;
}
function button(text, action, disabled = false, refreshAfter = true) {
  const node = document.createElement("button");
  node.type = "button";
  node.textContent = text;
  node.disabled = disabled;
  node.addEventListener("click", async () => {
    const expectedEpoch = epoch;
    node.disabled = true;
    try {
      await action();
      if (refreshAfter && expectedEpoch === epoch) await refreshAfterWrite();
      node.disabled = disabled;
    } catch (error) {
      notify(error.message, true);
      node.disabled = false;
    }
  });
  return node;
}
function profileLabel(id) {
  return profiles.find((p) => p.id === id)?.label || id;
}
function profileOptions(target, value) {
  const element = $(target);
  element.replaceChildren();
  for (const profile of profiles) {
    const option = document.createElement("option");
    option.value = profile.id;
    option.textContent = profile.label;
    element.append(option);
  }
  element.value = value;
}
function proxyLabel(proxy) {
  if (!proxy.configured) return "直接连接";
  const host = proxy.host?.includes(":") ? `[${proxy.host}]` : proxy.host;
  return `${proxy.scheme}://${host}${proxy.port ? ":" + proxy.port : ""}${
    proxy.authenticated ? " · 已认证" : ""
  }`;
}
const settingFields = {
  session_ttl_seconds: ["会话空闲有效期", 60, 604800, "秒 · 从最后成功一轮起"],
  request_timeout_seconds: ["请求超时", 1, 900, "秒 · 包含等待与迁移"],
  max_request_bytes: ["请求大小上限", 1024, 33554432, "字节"],
  simulated_cache: ["模拟 1h 缓存", null, null, "本地估算展示，默认开启"],
  compaction_enabled: ["自动压缩上下文", null, null, "保留最近完整交互"],
  context_budget: ["上下文估算预算", 4096, 1000000, "本地估算单位"],
  compaction_trigger_percent: ["压缩触发比例", 10, 90, "%"],
  keep_recent_rounds: ["保留最近完整交互", 1, 32, "轮 · 工具及结果保持完整"],
  account_min_interval_seconds: ["账号最小请求间隔", 1, 300, "秒"],
  account_requests_per_minute: ["账号每分钟请求上限", 1, 60, "次"],
  rate_limit_min_seconds: ["额度冷却最短时间", 60, 86400, "秒"],
  rate_limit_fallback_seconds: ["无明确时间时额度冷却", 60, 604800, "秒"],
  overload_seconds: ["过载冷却", 1, 900, "秒"],
};
function renderSettings(force = false) {
  if (
    !latestSettings ||
    (!force &&
      (settingsDirty || $("settings-form").contains(document.activeElement)))
  )
    return;
  settingsRevision = latestSettings.revision;
  const container = $("settings-fields");
  container.replaceChildren();
  for (const [name, [label, min, max, hint]] of Object.entries(settingFields)) {
    const field = document.createElement("div");
    field.className = "field";
    const caption = document.createElement("label");
    caption.htmlFor = "setting-" + name;
    caption.textContent = label;
    const input = document.createElement("input");
    input.id = "setting-" + name;
    input.name = name;
    input.type =
      typeof latestSettings.settings[name] === "boolean"
        ? "checkbox"
        : "number";
    if (input.type === "checkbox")
      input.checked = latestSettings.settings[name];
    else {
      input.value = latestSettings.settings[name];
      input.min = min;
      input.max = max;
      input.required = true;
    }
    const note = document.createElement("small");
    note.textContent = hint;
    field.append(caption, input, note);
    container.append(field);
  }
  settingsDirty = false;
  $("settings-dirty").hidden = true;
}
function renderAccounts(data) {
  latestAccounts = data.accounts;
  const ids = new Set(latestAccounts.map((a) => a.id));
  for (const id of selectedAccounts) if (!ids.has(id)) selectedAccounts.delete(id);
  updateSelection();
  table(
    "account-table",
    ["选择", "账号", "状态", "本轮用量 · 5h", "上轮用量 · 5h", "浏览器", "代理", "会话", "可开始时间", "操作"],
    data.accounts.map((account) => {
      const actions = document.createElement("div");
      actions.className = "account-actions";
      const checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.checked = selectedAccounts.has(account.id);
      checkbox.setAttribute("aria-label", "选择账号 " + account.label);
      checkbox.addEventListener("change", () => {
        if (checkbox.checked) selectedAccounts.add(account.id); else selectedAccounts.delete(account.id);
        updateSelection();
      });
      actions.append(button("编辑名称", () => {
        nameEdit = {id: account.id, revision};
        $("account-name").value = account.label;
        $("account-name-form").hidden = false;
        $("account-name").focus();
      }, false, false));
      actions.append(
        button(account.enabled ? "停用" : "启用", () =>
          api("/accounts/" + account.id, "PATCH", {
            expected_revision: revision,
            enabled: !account.enabled,
          })
        )
      );
      actions.append(
        button(
          "连接配置",
          () => {
            editAccount = account.id;
            editRevision = revision;
            $("transport-title").textContent = account.label + " · 连接配置";
            $("account-label").value = account.label;
            profileOptions("browser-profile", account.browser_profile);
            $("proxy-mode").value = "keep";
            $("proxy-current").textContent =
              "当前：" + proxyLabel(account.proxy);
            $("proxy-url").value = "";
            $("proxy-url").required = false;
            $("proxy-field").hidden = true;
            $("transport-form").hidden = false;
          },
          account.busy
        )
      );
      if (account.paused)
        actions.append(
          button("审核恢复", async () => {
            if (!confirm("确认已检查账号访问问题？恢复不会清除额度冷却。"))
              return;
            await api("/accounts/" + account.id + "/resume", "POST", {
              expected_revision: revision,
            });
          })
        );
      const cooling = account.ready_at > Date.now() / 1000;
      return [
        checkbox,
        account.label,
        tag(
          account.paused
            ? "待审核"
            : !account.enabled
            ? "已停用"
            : account.busy
            ? "执行中"
            : cooling
            ? "冷却中"
            : "空闲",
          account.paused
            ? "error"
            : !account.enabled
            ? "off"
            : cooling
            ? "amber"
            : ""
        ),
        cycleCell(account.usage_cycles?.current, false),
        cycleCell(account.usage_cycles?.previous, true),
        profileLabel(account.browser_profile),
        proxyLabel(account.proxy),
        account.sessions,
        !account.enabled || account.paused
          ? "—"
          : cooling
          ? date(account.ready_at)
          : "现在",
        actions,
      ];
    })
  );
  const selected = $("log-account").value;
  $("log-account").replaceChildren();
  for (const [id, name] of [["", "全部账号"], ...accountNames]) {
    const option = document.createElement("option");
    option.value = id;
    option.textContent = name;
    $("log-account").append(option);
  }
  $("log-account").value = selected;
}
function logQuery(before) {
  const query = new URLSearchParams({
    level: $("log-level").value,
    limit: "100",
  });
  if ($("log-account").value) query.set("account_id", $("log-account").value);
  if (before) query.set("before", before);
  return "/logs?" + query;
}
function logTokenMetric(kind, label, value, empty) {
  const known = Number.isSafeInteger(value) && value >= 0;
  const metric = document.createElement("span");
  metric.className = "log-token-metric log-token-" + kind;
  metric.title = `${label}：${known ? exactNumber(value) + " Token" : empty}`;
  metric.setAttribute("role", "img");
  metric.setAttribute("aria-label", metric.title);
  const icon = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  icon.setAttribute("viewBox", "0 0 20 20");
  icon.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute(
    "d",
    {
      input: "M10 2v16m-6-6 6 6 6-6",
      output: "M10 18V2m-6 6 6-6 6 6",
      read: "M3 4h14v4H3z M4 8v9h12V8 M8 11h4",
      write: "M10 4H3v13h13v-7 M8 12l1-4 6-6 3 3-6 6-4 1",
    }[kind]
  );
  icon.append(path);
  const count = document.createElement("strong");
  count.textContent = known
    ? number(value)
    : empty;
  if (!known) count.className = "unknown";
  metric.append(icon, count);
  return metric;
}
function logTokenCell(record) {
  const node = document.createElement("div");
  node.className = "log-token-cell";
  const usage = record.usage;
  const generation =
    record.operation === "completion" ||
    (record.operation === "client_request" &&
      ["messages", "chat", "responses"].includes(record.route));
  if (!generation) {
    node.textContent = "—";
    node.title = "非生成操作，不计 Token。";
    return node;
  }
  const input = usage?.input_tokens;
  const write = usage?.cache_creation_input_tokens;
  const read = usage?.cache_read_input_tokens;
  const cached =
    usage?.basis === "logical_request" &&
    usage.complete === true &&
    [input, write, read].every(
      (value) => Number.isSafeInteger(value) && value >= 0
    ) &&
    write <= input &&
    read <= input - write;
  const empty = !record.finished_at && usage ? "待完成" : "未知";
  node.title =
    usage?.basis === "sent_prompt"
      ? "本地估算：本次发送的提示词／已接收的输出片段，不含网页已有历史；并非平台计费 Token。"
      : "本地估算：完整逻辑输入（含模拟缓存）／用量账本的输出计数；Messages 与 API usage 同源，并非平台计费 Token。";
  if (Number.isSafeInteger(input) && input >= 0)
    node.title += ` 输入总量：${exactNumber(input)} Token。`;
  node.title += " 未知不按零计量；旧日志不回填。";
  const main = document.createElement("div");
  main.className = "log-token-line";
  const incoming = logTokenMetric(
    "input",
    cached ? "普通输入" : "输入",
    cached ? input - write - read : input,
    empty
  );
  if (cached) {
    incoming.title += `；输入总量：${exactNumber(input)} Token（含缓存读写）`;
    incoming.setAttribute("aria-label", incoming.title);
  }
  main.append(
    incoming,
    logTokenMetric("output", "输出", usage?.output_tokens, empty)
  );
  node.append(main);
  if (cached) {
    const cache = document.createElement("div");
    cache.className = "log-token-line log-token-cache";
    const creation = logTokenMetric("write", "模拟 1h 缓存写入", write);
    const badge = document.createElement("small");
    badge.className = "log-token-ttl";
    badge.textContent = "1h";
    creation.append(badge);
    cache.append(logTokenMetric("read", "模拟缓存读取", read), creation);
    node.append(cache);
  }
  if (
    Number.isSafeInteger(usage?.output_tokens) &&
    usage.output_tokens >= 0 &&
    !usage.complete
  ) {
    const detail = document.createElement("small");
    detail.className = "log-token-note";
    detail.textContent = record.finished_at ? "已接收 · 未完成" : "接收中";
    node.append(detail);
  }
  return node;
}
function renderLogs(data, append = false) {
  logRecords = append ? [...logRecords, ...data.records] : data.records;
  logBefore = data.next_before;
  pagedLogs = append;
  const phases = {
    processing: "处理中",
    sending: "发送中",
    receiving: "接收中",
    completed: "已完成",
    error: "错误",
    cancelled: "已取消",
    interrupted: "已中断",
    unknown: "记录不完整",
  };
  const operations = {
    client_request: "客户端请求",
    completion: "消息生成",
    organizations: "组织信息",
    model_catalog: "模型目录",
    conversation_create: "创建会话",
    conversation_delete: "清理会话",
  };
  table(
    "log-table",
    [
      "时间",
      "令牌名称",
      "请求模型",
      "Token 用量",
      "状态",
      "账号",
      "方向",
      "消息格式",
      "操作",
      "状态码",
      "耗时",
      "数据 / 分块",
      "请求 ID",
    ],
    logRecords.map((record) => {
      const id = document.createElement("small");
      id.textContent = record.request_id.slice(0, 16) + "…";
      id.title = record.request_id;
      return [
        date(record.at),
        record.token_name ||
          tokenNames.get(record.token_id) ||
          (record.token_id
            ? "历史令牌 " + record.token_id.slice(0, 8)
            : "未归属"),
        record.model || "—",
        logTokenCell(record),
        tag(
          phases[record.phase] || record.phase,
          record.level === "error" ? "error" : ""
        ),
        accountNames.get(record.account_id) ||
          (record.account_id ? record.account_id.slice(0, 8) : "—"),
        record.direction === "upstream" ? "上游" : "下游",
        record.mode === "sse" ? "SSE 流式" : "JSON 同步",
        operations[record.operation] || record.operation,
        record.status || "—",
        number(record.elapsed_ms) + " ms",
        number(record.bytes) + " B / " + number(record.chunks),
        id,
      ];
    })
  );
  $("older-logs").hidden = !data.has_more;
  $("log-note").textContent = !data.healthy
    ? "日志存储异常，请检查服务日志。"
    : data.dropped
    ? `有 ${number(data.dropped)} 条队列溢出记录 · 保留最多 ${number(
        data.retention_limit
      )} 条`
    : pagedLogs
    ? "正在查看历史记录，刷新可回到最新"
    : `保留最多 ${number(data.retention_limit)} 条 / ${data.retention_days} 天`;
}
function dirtyModels(enable = true) {
  modelDirty = true;
  if (enable) {
    modelDraft.enabled = true;
    $("model-mode").checked = true;
  }
  $("model-dirty").hidden = false;
}
function renderModelRows() {
  table(
    "model-table",
    ["启用", "下游模型 ID / 响应 model", "显示名称", "真实上游模型", "操作"],
    modelDraft.entries.map((entry, index) => {
      const active = document.createElement("input");
      active.type = "checkbox";
      active.checked = entry.enabled;
      active.setAttribute("aria-label", `启用模型 ${index + 1}`);
      active.addEventListener("change", () => {
        entry.enabled = active.checked;
        dirtyModels();
      });
      const fields = ["id", "display_name", "upstream_model"].map((name) => {
        const choose =
          name === "upstream_model" && upstreamCatalog?.models.length;
        const input = document.createElement(choose ? "select" : "input");
        if (choose) {
          input.id = `upstream-choice-${index}`;
          const ids = [...upstreamCatalog.models];
          if (entry[name] && !ids.includes(entry[name]))
            ids.unshift(entry[name]);
          const placeholder = document.createElement("option");
          placeholder.value = "";
          placeholder.textContent = "选择真实上游模型";
          input.append(placeholder);
          for (const id of ids) {
            const option = document.createElement("option");
            option.value = id;
            option.textContent = id;
            input.append(option);
          }
        }
        input.value = entry[name];
        input.required = true;
        input.maxLength = name === "display_name" ? 100 : 150;
        input.setAttribute(
          "aria-label",
          {
            id: "下游模型 ID",
            display_name: "显示名称",
            upstream_model: "真实上游模型",
          }[name] + ` ${index + 1}`
        );
        input.addEventListener(choose ? "change" : "input", () => {
          entry[name] = input.value;
          dirtyModels();
        });
        return input;
      });
      return [
        active,
        ...fields,
        button(
          "移除",
          () => {
            modelDraft.entries.splice(index, 1);
            dirtyModels();
            renderModelRows();
          },
          false,
          false
        ),
      ];
    })
  );
}
function cycleCell(cycle, previous) {
  const node = document.createElement("div");
  node.className = "account-cycle";
  if (!cycle) {
    const emptyCost = document.createElement("strong"), emptyTokens = document.createElement("small");
    emptyCost.textContent = "折合 $—";
    emptyTokens.textContent = "— Token";
    node.append(emptyCost, emptyTokens);
    node.title = previous ? "暂无上轮" : "尚未开始";
    return node;
  }
  const totals = cycle.totals;
  const cost = document.createElement("strong"), counts = document.createElement("small");
  const priced = totals.priced_requests || !totals.records;
  const incomplete = totals.unpriced_requests || totals.unmetered_requests || totals.legacy_records;
  cost.textContent = "折合 " + (priced ? money(totals.total_nano_usd) : "$—") + (priced && incomplete ? "*" : "");
  counts.textContent = `${number(totals.input_estimate + totals.output_estimate)} Token`;
  node.append(cost, counts);
  node.title = `本地估算，非上游额度；${date(cycle.start)} → ${date(cycle.end)}；输入 ${exactNumber(totals.input_estimate)} / 输出 ${exactNumber(totals.output_estimate)}；${exactNumber(totals.requests)} 请求 / ${exactNumber(totals.internal_requests)} 内部`;
  if (incomplete) node.title += "；金额仅含已计价记录，失败或未计价用量未知";
  if (cycle.fallback_records) node.title += "；含历史完成时间推算";
  return node;
}
function updateSelection() {
  $("account-selected").textContent = `已选 ${selectedAccounts.size} 个`;
  $("show-batch-proxy").disabled = !selectedAccounts.size;
}
function renderModels(data) {
  latestModels = data;
  renderUpstreamCatalog(data.upstream_catalog);
  if (modelDirty || $("model-form").contains(document.activeElement)) return;
  modelDraft = structuredClone(data.models);
  if (
    !modelDraft.enabled &&
    !modelDraft.entries.length &&
    data.default_models?.length
  )
    modelDraft.entries = structuredClone(data.default_models);
  modelRevision = data.revision;
  $("model-mode").checked = modelDraft.enabled;
  $("model-dirty").hidden = true;
  renderModelRows();
}
function renderUpstreamCatalog(catalog) {
  upstreamCatalog = catalog;
  $("import-default-models").disabled = !catalog?.models.length;
  $("upstream-model-status").textContent = catalog
    ? `${
        accountNames.get(catalog.account_id) || catalog.account_id.slice(0, 8)
      } · ${catalog.models.length} 个模型 · 上次获取 ${date(
        catalog.fetched_at
      )}`
    : "尚未获取，请手动查询";
  table(
    "upstream-model-table",
    ["默认原生模型 ID", "操作"],
    (catalog?.models || []).map((id) => [
      id,
      button("加入配置", () => addDefaultModels([id]), false, false),
    ])
  );
}
function addDefaultModels(ids, announce = true) {
  const existing = new Set(modelDraft.entries.map((e) => e.id));
  const additions = [...new Set(ids)].filter((id) => !existing.has(id));
  if (modelDraft.entries.length + additions.length > 256) {
    notify("最多配置 256 个模型", true);
    return null;
  }
  for (const id of additions)
    modelDraft.entries.push({
      id,
      display_name: id,
      upstream_model: id,
      enabled: true,
    });
  if (additions.length || announce) dirtyModels();
  renderModelRows();
  window.SelectUI?.sync();
  if (announce)
    notify(
      additions.length
        ? `已加入 ${additions.length} 个模型，请审核后保存`
        : "列表已在当前草稿中，可直接修改并保存"
    );
  return additions.length;
}
function renderTokens(data) {
  table(
    "token-table",
    ["名称", "状态", "使用中 / 并发上限", "创建时间", "到期时间", "操作"],
    data.tokens.map((token) => {
      const actions = document.createElement("div");
      actions.append(button("编辑", () => {
        tokenEdit = {id:token.id, revision, expires_at:token.expires_at};
        $("token-edit-label").value = token.label;
        $("token-edit-concurrency").value = token.max_concurrency;
        $("token-edit-never").checked = !token.expires_at;
        $("token-edit-expiration").value = token.expires_at ? localDateTime(token.expires_at) : "";
        syncTokenEditExpiry();
        $("token-edit-form").hidden = false;
        $("token-edit-label").focus();
      }, false, false));
      actions.append(button("删除", async () => {
        if (!confirm(`删除令牌“${token.label}”？新的请求将无法使用它。`)) return;
        await api("/tokens/" + token.id, "DELETE", {expected_revision: revision});
        if (tokenEdit?.id === token.id) {tokenEdit = undefined; $("token-edit-form").hidden = true;}
        notify("令牌已删除");
      }));
      return [
      token.label,
      tag(token.expired ? "已过期" : "有效", token.expired ? "error" : ""),
      `${token.active_requests} / ${token.max_concurrency}`,
      date(token.created_at),
      token.expires_at ? date(token.expires_at) : "永不过期",
      actions,
    ];})
  );
}
async function loadLogs(older = false) {
  const request = ++logRequest,
    filter = logQuery();
  const data = await api(logQuery(older ? logBefore : null));
  if (request !== logRequest || filter !== logQuery() || !key) return;
  renderLogs(data, older);
}
function refresh({ manual = false } = {}) {
  if (!key) return Promise.resolve();
  if (pendingRefresh) return pendingRefresh;
  clearTimeout(pollTimer);
  if (manual) {
    pagedLogs = false;
    logRequest++;
  }
  const expectedEpoch = epoch,
    filter = logQuery(),
    usageFilter = analyticsQuery(),
    shouldReadLogs = !pagedLogs;
  const group = new AbortController();
  $("refresh").disabled = true;
  const refreshTask = (async () => {
    try {
      const [
        info,
        usage,
        accounts,
        sessions,
        config,
        events,
        logs,
        models,
        tokens,
      ] = await Promise.all([
        api("/info", "GET", null, group.signal),
        api(usageFilter, "GET", null, group.signal),
        api("/accounts", "GET", null, group.signal),
        api("/sessions", "GET", null, group.signal),
        api("/settings", "GET", null, group.signal),
        api("/events?limit=100", "GET", null, group.signal),
        shouldReadLogs
          ? api(filter, "GET", null, group.signal)
          : Promise.resolve(null),
        api("/models", "GET", null, group.signal),
        api("/tokens", "GET", null, group.signal),
      ]);
      if (expectedEpoch !== epoch || !key) return;
      revision = config.revision;
      latestSettings = config;
      profiles = info.browser_profiles;
      defaultProfile = info.default_browser_profile;
      accountNames = new Map(accounts.accounts.map((a) => [a.id, a.label]));
      const chosen = $("model-account").value;
      $("model-account").replaceChildren();
      for (const [id, name] of [
        ["", "自动选择空闲账号"],
        ...accounts.accounts.map((a) => [a.id, a.label]),
      ]) {
        const o = document.createElement("option");
        o.value = id;
        o.textContent = name;
        $("model-account").append(o);
      }
      $("model-account").value = chosen;
      tokenNames = new Map(tokens.tokens.map((t) => [t.id, t.label]));
      connection(
        info.healthy,
        info.healthy ? "后端已连接 · " + info.version : "状态存储异常"
      );
      window.VersionManager?.setVersion(info.version);
      $("last-update").textContent =
        "最后同步 " + new Date().toLocaleTimeString("zh-CN");
      if (usageFilter === analyticsQuery())
        window.UsageCharts.render(usage, info, accountNames, latestSettings.settings);
      renderAccounts(accounts);
      table(
        "session-table",
        ["会话", "账号", "模型", "历史 / 上下文估算", "到期时间", "操作"],
        sessions.sessions.map((session) => [
          session.id.slice(0, 12),
          accountNames.get(session.account_id) || "—",
          session.model || "—",
          `${session.turns} / ${number(session.context_estimate)}${
            session.checkpoint ? " · 有摘要" : ""
          }`,
          date(session.expires_at),
          button(
            session.active ? "执行中" : "清理",
            async () => {
              if (confirm("释放此会话并排队清理远端会话？"))
                await api("/sessions/" + session.id, "DELETE", {
                  expected_revision: revision,
                });
            },
            session.active
          ),
        ])
      );
      table(
        "event-table",
        ["时间", "操作", "记录"],
        events.events
          .slice()
          .reverse()
          .map((event) => [
            date(event.at),
            event.kind,
            JSON.stringify(event.data),
          ])
      );
      renderSettings();
      renderModels(models);
      renderTokens(tokens);
      window.SelectUI?.sync();
      if (logs && filter === logQuery() && !pagedLogs) renderLogs(logs);
    } catch (error) {
      group.abort();
      if (expectedEpoch === epoch && key) {
        connection(false);
        if (error.status === 401) {
          clearSession();
          notify("管理密钥无效，请重新登录", true);
        } else if (manual) notify(error.message, true);
      }
      throw error;
    }
  })().finally(() => {
    if (pendingRefresh === refreshTask) pendingRefresh = null;
    if (expectedEpoch === epoch) {
      $("refresh").disabled = false;
      if (key) schedule();
    }
  });
  pendingRefresh = refreshTask;
  return pendingRefresh;
}
async function refreshAfterWrite() {
  const expectedEpoch = epoch;
  // A poll that started before the mutation may contain the old revision.
  if (pendingRefresh) await pendingRefresh.catch(() => {});
  if (expectedEpoch === epoch && key) await refresh({ manual: true });
}

async function connect(credential) {
  clearTimeout(pollTimer);
  epoch++;
  key = credential;
  $("key").value = "";
  $("login-button").disabled = true;
  try {
    await refresh();
    if (!key) return;
    $("login").hidden = true;
    $("console").hidden =
      $("logout").hidden =
      $("refresh-controls").hidden =
        false;
    window.VersionManager?.connected();
  } catch (error) {
    clearSession();
    notify(error.message, true);
  } finally {
    $("login-button").disabled = false;
  }
}
$("login-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  await connect($("key").value);
});
$("logout").addEventListener("click", clearSession);
$("refresh").addEventListener("click", () =>
  refresh({ manual: true }).catch(() => {})
);
$("refresh-interval").addEventListener("change", schedule);
const sidebarMedia = matchMedia("(max-width:720px)");
function syncSidebar() {
  const expanded = sidebarMedia.matches
    ? document.body.classList.contains("sidebar-expanded-mobile")
    : !document.body.classList.contains("sidebar-collapsed");
  $("collapse").textContent = expanded ? "‹" : "›";
  $("collapse").setAttribute("aria-expanded", String(expanded));
  $("collapse").setAttribute("aria-label", expanded ? "折叠导航" : "展开导航");
  $("collapse").title = expanded ? "折叠导航" : "展开导航";
}
sidebarMedia.addEventListener("change", syncSidebar, {signal:globalEvents.signal});
syncSidebar();
$("collapse").addEventListener("click", () => {
  document.body.classList.toggle(
    sidebarMedia.matches ? "sidebar-expanded-mobile" : "sidebar-collapsed"
  );
  syncSidebar();
});
document.querySelectorAll("[data-tab]").forEach((button) =>
  button.addEventListener("click", () => {
    document
      .querySelectorAll(".tab")
      .forEach((tab) => (tab.hidden = tab.id !== button.dataset.tab));
    document
      .querySelectorAll("[data-tab]")
      .forEach((item) => item.classList.toggle("selected", item === button));
    document.body.classList.remove("sidebar-expanded-mobile");
    syncSidebar();
  })
);
$("show-import").addEventListener("click", () => {
  $("import-form").hidden = false;
  profileOptions("import-profile", defaultProfile);
  $("credentials").focus();
});
$("cancel-import").addEventListener("click", () => {
  $("credentials").value = $("import-proxy").value = "";
  $("import-form").hidden = true;
});
$("select-accounts").addEventListener("click", () => {
  for (const a of latestAccounts) selectedAccounts.add(a.id);
  renderAccounts({accounts:latestAccounts});
});
$("clear-accounts").addEventListener("click", () => {
  selectedAccounts.clear(); renderAccounts({accounts:latestAccounts});
});
$("cancel-account-name").addEventListener("click", () => {
  nameEdit = undefined; $("account-name-form").hidden = true;
});
$("account-name-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!nameEdit) return;
  const captured = nameEdit, expectedEpoch = epoch;
  const submit = event.submitter;
  submit.disabled = true;
  try {
    await api("/accounts/" + captured.id, "PATCH", {expected_revision:captured.revision, label:$("account-name").value});
    if (expectedEpoch !== epoch || nameEdit !== captured) return;
    nameEdit = undefined; $("account-name-form").hidden = true;
    await refreshAfterWrite(); notify("账号名称已保存");
  } catch (error) {if (expectedEpoch === epoch) notify(error.message, true);}
  finally { if (expectedEpoch === epoch) submit.disabled = false; }
});
$("show-batch-proxy").addEventListener("click", () => {
  batchEdit = {ids:[...selectedAccounts], revision};
  $("batch-proxy-targets").textContent = `将更新 ${batchEdit.ids.length} 个账号；之后的勾选不会改变本次范围。`;
  $("batch-proxy-mode").value = "replace";
  $("batch-proxy-url").value = "";
  $("batch-proxy-url").required = true;
  $("batch-proxy-field").hidden = false;
  $("batch-proxy-form").hidden = false;
  window.SelectUI?.sync();
});
$("batch-proxy-mode").addEventListener("change", () => {
  const direct = $("batch-proxy-mode").value === "direct";
  $("batch-proxy-field").hidden = direct;
  $("batch-proxy-url").required = !direct;
  if (direct) $("batch-proxy-url").value = "";
});
$("cancel-batch-proxy").addEventListener("click", () => {
  batchEdit = undefined; $("batch-proxy-form").hidden = true; $("batch-proxy-url").value = "";
});
$("batch-proxy-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!batchEdit) return;
  const captured = batchEdit, expectedEpoch = epoch;
  const proxy = $("batch-proxy-mode").value === "direct" ? "" : $("batch-proxy-url").value;
  $("batch-proxy-url").value = "";
  $("save-batch-proxy").disabled = true;
  try {
    await api("/accounts/proxy", "PATCH", {expected_revision:captured.revision, account_ids:captured.ids, proxy_url:proxy});
    if (expectedEpoch !== epoch || batchEdit !== captured) return;
    batchEdit = undefined; $("batch-proxy-form").hidden = true;
    await refreshAfterWrite(); notify("所选账号代理已一起更新");
  } catch (error) {if (expectedEpoch === epoch) notify(error.message, true);}
  finally {if (expectedEpoch === epoch) $("save-batch-proxy").disabled = false;}
});
$("import-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const raw = $("credentials").value,
    proxy = $("import-proxy").value;
  $("credentials").value = $("import-proxy").value = "";
  try {
    const result = await api("/accounts/import", "POST", {
      expected_revision: revision,
      accounts: raw
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter((line) => line && !line.startsWith("#"))
        .map((credential) => ({
          credential,
          browser_profile: $("import-profile").value,
          ...(proxy ? { proxy_url: proxy } : {}),
        })),
    });
    $("import-form").hidden = true;
    notify(`已新增 ${result.added} 个账号`);
    await refreshAfterWrite();
  } catch (error) {
    notify(error.message, true);
  }
});
$("proxy-mode").addEventListener("change", () => {
  $("proxy-field").hidden = $("proxy-mode").value !== "replace";
  $("proxy-url").required = $("proxy-mode").value === "replace";
});
$("cancel-transport").addEventListener("click", () => {
  $("transport-form").hidden = true;
  $("proxy-url").value = "";
  editAccount = "";
});
$("transport-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const proxy = $("proxy-url").value;
  $("proxy-url").value = "";
  const patch = {
    expected_revision: editRevision,
    label: $("account-label").value,
    browser_profile: $("browser-profile").value,
  };
  if ($("proxy-mode").value === "replace") patch.proxy_url = proxy;
  if ($("proxy-mode").value === "direct") patch.proxy_url = "";
  try {
    await api("/accounts/" + editAccount, "PATCH", patch);
    $("transport-form").hidden = true;
    notify("账号配置已保存");
    await refreshAfterWrite();
  } catch (error) {
    notify(error.message, true);
  }
});
$("settings-form").addEventListener("input", () => {
  settingsDirty = true;
  $("settings-dirty").hidden = false;
});
$("reload-settings").addEventListener("click", () => renderSettings(true));
$("settings-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const settings = {};
  for (const name of Object.keys(settingFields)) {
    const input = $("setting-" + name);
    settings[name] =
      input.type === "checkbox" ? input.checked : Number(input.value);
  }
  try {
    await api("/settings", "PUT", {
      expected_revision: settingsRevision,
      settings,
    });
    settingsDirty = false;
    $("settings-dirty").hidden = true;
    notify("配置已保存");
    await refreshAfterWrite();
  } catch (error) {
    notify(error.message, true);
  }
});
for (const id of ["log-level", "log-account"])
  $(id).addEventListener("change", () => {
    pagedLogs = false;
    loadLogs().catch((error) => notify(error.message, true));
  });
$("older-logs").addEventListener("click", () =>
  loadLogs(true).catch((error) => notify(error.message, true))
);
window.addEventListener("pagehide", clearSession, {signal:globalEvents.signal});

$("model-mode").addEventListener("change", () => {
  modelDraft.enabled = $("model-mode").checked;
  dirtyModels(false);
});
$("fetch-upstream-models").addEventListener("click", async () => {
  const expectedEpoch = epoch;
  $("fetch-upstream-models").disabled = true;
  try {
    const value = $("model-account").value;
    const result = await api("/models/upstream", "POST", {
      account_id: value || null,
    });
    if (epoch !== expectedEpoch) return;
    renderUpstreamCatalog(result.upstream_catalog);
    const added = addDefaultModels(result.upstream_catalog.models, false);
    await refreshAfterWrite();
    if (added !== null)
      notify(
        `已获取 ${result.default_models.length} 个上游模型，补入 ${added} 个；保留已有修改，保存后生效`
      );
  } catch (error) {
    if (epoch === expectedEpoch) notify(error.message, true);
  } finally {
    if (epoch === expectedEpoch) $("fetch-upstream-models").disabled = false;
  }
});
$("import-default-models").addEventListener("click", () =>
  addDefaultModels(upstreamCatalog?.models || [])
);
$("add-model").addEventListener("click", () => {
  modelDraft.entries.push({
    id: "",
    display_name: "",
    upstream_model: "",
    enabled: true,
  });
  dirtyModels();
  renderModelRows();
});
$("reload-models").addEventListener("click", () => {
  if (modelDirty && !confirm("放弃未保存的模型配置？")) return;
  modelDirty = false;
  $("reload-models").blur();
  renderModels(latestModels);
});
$("model-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const expectedEpoch = epoch;
  $("save-models").disabled = true;
  try {
    await api("/models", "PUT", {
      expected_revision: modelRevision,
      models: modelDraft,
    });
    if (expectedEpoch !== epoch) return;
    modelDirty = false;
    $("save-models").blur();
    await refreshAfterWrite();
    notify("模型配置已保存");
  } catch (error) {
    if (expectedEpoch === epoch) notify(error.message, true);
  } finally {
    $("save-models").disabled = false;
  }
});
function defaultTokenExpiry() {
  const value = new Date(Date.now() + 30 * 86400000);
  value.setMinutes(value.getMinutes() - value.getTimezoneOffset());
  $("token-expiration").value = value.toISOString().slice(0, 16);
}
function localDateTime(seconds) {
  const value = new Date(seconds * 1000);
  value.setMinutes(value.getMinutes() - value.getTimezoneOffset());
  return value.toISOString().slice(0, 16);
}
function syncTokenEditExpiry() {
  $("token-edit-expiration").disabled = $("token-edit-never").checked;
  $("token-edit-expiration").required = !$("token-edit-never").checked;
}
$("token-edit-never").addEventListener("change", syncTokenEditExpiry);
$("cancel-token-edit").addEventListener("click", () => {
  tokenEdit = undefined; $("token-edit-form").hidden = true;
});
$("token-edit-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!tokenEdit) return;
  const captured = tokenEdit, expectedEpoch = epoch;
  $("save-token-edit").disabled = true;
  try {
    await api("/tokens/" + captured.id, "PATCH", {
      expected_revision:captured.revision,
      label:$("token-edit-label").value,
      max_concurrency:Number($("token-edit-concurrency").value),
      expires_at:$("token-edit-never").checked ? null
        : captured.expires_at && localDateTime(captured.expires_at) === $("token-edit-expiration").value
        ? captured.expires_at : Math.floor(new Date($("token-edit-expiration").value).getTime()/1000),
    });
    if (expectedEpoch !== epoch || tokenEdit !== captured) return;
    tokenEdit = undefined; $("token-edit-form").hidden = true;
    await refreshAfterWrite(); notify("令牌已更新，原密钥保持有效");
  } catch (error) {if (expectedEpoch === epoch) notify(error.message, true);}
  finally {if (expectedEpoch === epoch) $("save-token-edit").disabled = false;}
});
defaultTokenExpiry();
$("token-never").addEventListener("change", () => {
  $("token-expiration").disabled = $("token-never").checked;
  $("token-expiration").required = !$("token-never").checked;
});
$("token-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!$("token-reveal").hidden) {
    notify("请先保存并关闭刚创建的令牌", true);
    return;
  }
  const expectedEpoch = epoch;
  $("create-token").disabled = true;
  const value = {
    expected_revision: revision,
    label: $("token-label").value,
    max_concurrency: Number($("token-concurrency").value),
    expires_at: $("token-never").checked
      ? null
      : Math.floor(new Date($("token-expiration").value).getTime() / 1000),
  };
  try {
    const result = await api("/tokens", "POST", value);
    if (expectedEpoch !== epoch) return;
    $("token-secret").type = "password";
    $("token-secret").value = result.token;
    $("token-reveal").hidden = false;
    $("token-label").value = "";
    await refreshAfterWrite();
    notify("令牌已创建，请立即保存");
  } catch (error) {
    if (expectedEpoch === epoch) notify(error.message, true);
  } finally {
    if (expectedEpoch === epoch)
      $("create-token").disabled = !$("token-reveal").hidden;
  }
});
$("show-token").addEventListener("click", () => {
  $("token-secret").type =
    $("token-secret").type === "password" ? "text" : "password";
});
$("copy-token").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("token-secret").value);
    notify("令牌已复制");
  } catch {
    $("token-secret").focus();
    $("token-secret").select();
    notify("请手动复制选中的令牌");
  }
});
$("dismiss-token").addEventListener("click", () => {
  $("token-secret").value = "";
  $("token-reveal").hidden = true;
  $("create-token").disabled = false;
});

for (const id of ["usage-range", "usage-granularity"]) {
  $(id).addEventListener("change", () => refreshAfterWrite().catch(() => {}));
}
$("usage-metric").addEventListener("change", () =>
  window.UsageCharts?.redraw()
);

function analyticsQuery() {
  return (
    "/analytics?" +
    new URLSearchParams({
      range: $("usage-range").value,
      granularity: $("usage-granularity").value,
    })
  );
}
function hasDraft() {
  return settingsDirty || modelDirty || Boolean(nameEdit || batchEdit || tokenEdit) || !$("transport-form").hidden || !$("token-reveal").hidden || !$("import-form").hidden;
}
async function reloadPanel() {
  if (hasDraft()) throw new Error("请先保存或关闭未完成的编辑");
  const credential = key;
  if (!credential) return;
  const tab = document.querySelector("[data-tab].selected")?.dataset.tab;
  const response = await fetch("/?reload=" + Date.now(), {cache:"no-store"});
  if (!response.ok) throw new Error("新版面板暂时无法加载，请稍后重试");
  const parsed = new DOMParser().parseFromString(await response.text(), "text/html");
  const scripts = [...parsed.querySelectorAll("script")].map(script => {
    const url = new URL(script.getAttribute("src") || "", location.href);
    if (url.origin !== location.origin || !/^\/[a-z0-9-]+\.js$/.test(url.pathname) || script.textContent.trim()) throw new Error("新版面板资源格式不兼容");
    url.searchParams.set("reload",String(Date.now()));
    script.remove(); return url.href;
  });
  if (!parsed.getElementById("login-form") || !scripts.length) throw new Error("新版面板内容不完整");
  for (const link of parsed.querySelectorAll('link[rel="stylesheet"]')) {
    const url = new URL(link.getAttribute("href"), location.href);
    if (url.origin !== location.origin) throw new Error("新版面板样式来源不兼容");
    url.searchParams.set("reload",String(Date.now()));link.setAttribute("href",url.href);
  }
  clearSession(); globalEvents.abort(); window.VersionManager?.dispose(); window.SelectUI?.dispose();
  document.documentElement.replaceWith(document.importNode(parsed.documentElement,true));
  try {
    for (const src of scripts) await new Promise((resolve,reject) => {
      const script=document.createElement("script"); script.src=src;script.onload=resolve;script.onerror=()=>reject(new Error("新版面板脚本加载失败"));document.head.append(script);
    });
    await window.Claud2ApiUI.connect(credential);
    if (!window.Claud2ApiUI.loggedIn()) throw new Error("新版后端连接失败");
    if (tab) document.querySelector(`[data-tab="${tab}"]`)?.click();
  } catch (error) {
    const notice=document.getElementById("notice");if (notice) {notice.hidden=false;notice.textContent=error.message + "，请刷新页面重新连接";}
    throw error;
  }
}
window.Claud2ApiUI = {request:api,refresh,connect,reload:reloadPanel,hasDraft,table,loggedIn:()=>Boolean(key)};
})();
