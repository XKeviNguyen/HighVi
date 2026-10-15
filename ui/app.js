const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { t } = I18N;

const $ = (selector) => document.querySelector(selector);
const elements = {
  form: $("#add-form"),
  pageFields: $("#page-fields"),
  pageUrls: $("#page-urls"),
  directFields: $("#direct-fields"),
  playlistInput: $("#playlist-input"),
  refererInput: $("#referer-input"),
  addButton: $("#add-button"),
  addResult: $("#add-result"),
  depsCard: $("#deps-card"),
  depsMessage: $("#deps-message"),
  browserNote: $("#browser-note"),
  outdatedNote: $("#outdated-note"),
  outdatedMessage: $("#outdated-message"),
  queueStatus: $("#queue-status"),
  queueCounts: $("#queue-counts"),
  queuePaused: $("#queue-paused"),
  startButton: $("#start-button"),
  concurrency: $("#concurrency"),
  jobList: $("#job-list"),
  queueEmpty: $("#queue-empty"),
  cancelAll: $("#cancel-all-button"),
  retryFailed: $("#retry-failed-button"),
  clearCompleted: $("#clear-button"),
  openFolder: $("#open-folder-button"),
  errorCard: $("#error-card"),
  errorMessage: $("#error-message"),
  template: $("#job-template"),
};

// The newest queue state from Rust; Rust owns it, the page only shows it.
let snapshot = null;
let renderPending = false;
let lastAdd = null;
let lastError = null;
let missingTools = [];
let browserMissing = false;
let outdated = null;
// Diagnostic reports already fetched, by "id:attempt".
const reports = new Map();

const stageKeys = {
  discovering: "progress.discovering",
  browsing: "progress.browsing",
  downloadingVideo: "progress.downloadingVideo",
  downloadingAudio: "progress.downloadingAudio",
  merging: "progress.merging",
  saving: "progress.saving",
};

function setHidden(element, hidden) {
  element.classList.toggle("hidden", hidden);
}

function selectedMode() {
  return elements.form.querySelector('input[name="mode"]:checked').value;
}

function applyMode() {
  const direct = selectedMode() === "directHls";
  setHidden(elements.pageFields, direct);
  setHidden(elements.directFields, !direct);
}

// ------------------------------------------------------------- messages

function errorText(error, kind) {
  const code = typeof error?.code === "string" ? error.code : "";
  if (code === "access_denied" || code === "host_firewall_block") {
    return t(`error.${code}.${kind ?? "page"}`);
  }
  if (code === "output_publish_failed") {
    // Rust's message carries the recovery path, which the user needs.
    return `${t("error.output_publish_failed")}\n${error.message ?? ""}`;
  }
  if (code && I18N.has(`error.${code}`)) {
    return t(`error.${code}`);
  }
  return typeof error?.message === "string" && error.message ? error.message : t("error.unknown");
}

// Rust sends the hint's values; they are short version strings.
function hintText(hint) {
  if (hint?.kind !== "updateYtDlp" || typeof hint.installed !== "string") {
    return "";
  }
  return typeof hint.minimum === "string"
    ? t("hint.updateYtDlp.minimum", { installed: hint.installed, minimum: hint.minimum })
    : t("hint.updateYtDlp.stale", { installed: hint.installed });
}

function showError(error) {
  lastError = error;
  elements.errorMessage.textContent = errorText(error, "page");
  setHidden(elements.errorCard, false);
}

function renderMissingTools() {
  setHidden(elements.depsCard, missingTools.length === 0);
  elements.depsMessage.textContent = missingTools.length
    ? t("deps.missing", { tools: missingTools.join(", ") })
    : "";
  setHidden(elements.browserNote, !browserMissing || missingTools.length > 0);
  setHidden(elements.outdatedNote, !outdated);
  elements.outdatedMessage.textContent = outdated ? t("deps.outdated", outdated) : "";
}

// ------------------------------------------------------------ adding

function readSources() {
  if (selectedMode() === "directHls") {
    return {
      lines: null,
      sources: [
        {
          mode: "directHls",
          playlistUrl: elements.playlistInput.value.trim(),
          refererPageUrl: elements.refererInput.value.trim(),
        },
      ],
    };
  }
  const lines = elements.pageUrls.value
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  return { lines, sources: lines.map((pageUrl) => ({ mode: "page", pageUrl })) };
}

function renderAddResult() {
  if (!lastAdd) {
    setHidden(elements.addResult, true);
    return;
  }
  const parts = [];
  if (lastAdd.added) parts.push(t("add.added", { count: lastAdd.added }));
  if (lastAdd.duplicates) parts.push(t("add.duplicates", { count: lastAdd.duplicates }));
  if (lastAdd.rejected.length) {
    parts.push(t("add.rejected", { count: lastAdd.rejected.length }));
    for (const { line, error } of lastAdd.rejected.slice(0, 5)) {
      const where = line ? t("add.line", { line }) : "";
      parts.push(`${where}${errorText(error, lastAdd.kind)}`);
    }
  }
  if (!parts.length) parts.push(t("add.nothing"));
  elements.addResult.textContent = parts.join("\n");
  elements.addResult.classList.toggle("bad", lastAdd.rejected.length > 0);
  setHidden(elements.addResult, false);
}

elements.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const { lines, sources } = readSources();
  if (!sources.length) {
    lastAdd = { added: 0, duplicates: 0, rejected: [], kind: "page" };
    renderAddResult();
    return;
  }
  elements.addButton.disabled = true;
  try {
    const report = await invoke("queue_add", { sources });
    const rejectedIndexes = new Set(report.rejected.map(([index]) => index));
    lastAdd = {
      added: report.added.length,
      duplicates: report.duplicates.length,
      rejected: report.rejected.map(([index, error]) => ({
        line: lines ? index + 1 : null,
        error,
      })),
      kind: lines ? "page" : "direct",
    };
    // Keep only the links that need fixing.
    if (lines) {
      elements.pageUrls.value = lines.filter((_, index) => rejectedIndexes.has(index)).join("\n");
    } else if (!rejectedIndexes.size) {
      elements.playlistInput.value = "";
    }
    renderAddResult();
  } catch (error) {
    showError(error);
  } finally {
    elements.addButton.disabled = false;
  }
});

// ------------------------------------------------------------- the queue

function statusText(item) {
  switch (item.status) {
    case "queued":
      return t("job.queued");
    case "running":
      return t(stageKeys[item.stage] ?? "progress.working");
    case "cancelling":
      return t("cancel.busy");
    case "completed":
      return t("job.completed");
    case "cancelled":
      return t("cancelled.title");
    case "failed":
      return errorText(item.error, item.kind);
    default:
      return "";
  }
}

function setProgress(row, item) {
  const track = row.querySelector(".progress-track");
  const bar = row.querySelector(".progress-bar");
  const active = item.status === "running" || item.status === "cancelling";
  setHidden(track, !active);
  track.setAttribute("aria-label", t("progress.aria"));
  const parsed = Number.parseFloat(item.percent || "");
  if (Number.isFinite(parsed)) {
    const bounded = Math.min(100, Math.max(0, parsed));
    bar.style.width = `${bounded}%`;
    track.setAttribute("aria-valuenow", String(bounded));
    track.classList.remove("indeterminate");
  } else {
    bar.style.width = "35%";
    track.removeAttribute("aria-valuenow");
    track.classList.add("indeterminate");
  }
  row.querySelector(".job-percent").textContent = active ? item.percent || "" : "";
  const metrics = [];
  if (active && item.speed) metrics.push(t("progress.speed", { value: item.speed }));
  if (active && item.eta) metrics.push(t("progress.eta", { value: item.eta }));
  row.querySelector(".job-metrics").textContent = metrics.join("  ");
}

function createRow(item) {
  const row = elements.template.content.firstElementChild.cloneNode(true);
  row.dataset.id = String(item.id);
  row.querySelector(".job-cancel").addEventListener("click", () => {
    row.querySelector(".job-cancel").disabled = true;
    invoke("queue_cancel", { id: item.id }).catch(showError);
  });
  row.querySelector(".job-retry").addEventListener("click", () => {
    invoke("queue_retry", { id: item.id }).catch(showError);
  });
  row.querySelector(".job-remove").addEventListener("click", () => {
    invoke("queue_remove", { id: item.id }).catch(showError);
  });
  row.querySelector(".copy-diagnostics").addEventListener("click", () => copyReport(row));
  row.querySelector("details").addEventListener("toggle", () => loadReport(row));
  return row;
}

function renderRow(row, item) {
  row.dataset.attempt = String(item.attempt);
  row.dataset.status = item.status;
  row.className = `job ${item.status}`;
  row.querySelector(".job-title").textContent = item.title || item.host || "";
  row.querySelector(".job-status").textContent = statusText(item);
  setProgress(row, item);

  const failed = item.status === "failed";
  const hint = failed ? hintText(item.error?.hint) : "";
  const hintElement = row.querySelector(".job-hint");
  hintElement.textContent = hint;
  setHidden(hintElement, !hint);
  const saved = row.querySelector(".job-saved");
  saved.textContent = item.savedTo ? t("job.savedTo", { path: item.savedTo }) : "";
  setHidden(saved, !item.savedTo);

  const diagnostics = row.querySelector(".diagnostics");
  setHidden(diagnostics, !(failed && item.hasDiagnostics));
  row.querySelector(".copy-diagnostics").textContent = t("diagnostics.copy");
  row.querySelector("summary").textContent = t("diagnostics.show");
  const key = `${item.id}:${item.attempt}`;
  if (row.dataset.report !== key) {
    row.dataset.report = key;
    row.querySelector("pre").textContent = reports.get(key) ?? "";
    row.querySelector(".copy-status").textContent = "";
  }

  const cancel = row.querySelector(".job-cancel");
  cancel.textContent = t(item.status === "cancelling" ? "cancel.busy" : "cancel.button");
  setHidden(cancel, !["queued", "running", "cancelling"].includes(item.status));
  cancel.disabled = item.status === "cancelling";
  const retry = row.querySelector(".job-retry");
  retry.textContent = t("job.retry");
  setHidden(retry, !["failed", "cancelled"].includes(item.status));
  const remove = row.querySelector(".job-remove");
  remove.textContent = t("job.remove");
  setHidden(remove, ["running", "cancelling"].includes(item.status));
}

async function loadReport(row) {
  const [id, attempt] = (row.dataset.report || "").split(":").map(Number);
  const key = `${id}:${attempt}`;
  if (!id || reports.has(key)) {
    return reports.get(key) ?? "";
  }
  const text = (await invoke("queue_diagnostics", { id }).catch(() => null)) ?? "";
  if (row.dataset.report === key) {
    reports.set(key, text);
    row.querySelector("pre").textContent = text;
  }
  return text;
}

// The report is plain text without URLs. The clipboard API needs the click
// that triggered it; selecting the text is the fallback if it is refused.
async function copyReport(row) {
  const status = row.querySelector(".copy-status");
  const text = await loadReport(row);
  try {
    await navigator.clipboard.writeText(text);
    status.textContent = t("diagnostics.copied");
  } catch {
    const details = row.querySelector("details");
    details.open = true;
    const range = document.createRange();
    range.selectNodeContents(row.querySelector("pre"));
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    status.textContent = t("diagnostics.copyFailed");
  }
}

function render() {
  renderPending = false;
  if (!snapshot) return;
  const { items, counts, active, desired, started, max } = snapshot;

  // Rows are kept and updated in place, in queue order.
  const rows = new Map([...elements.jobList.children].map((row) => [row.dataset.id, row]));
  let previous = null;
  for (const item of items) {
    const key = String(item.id);
    let row = rows.get(key);
    if (row) {
      rows.delete(key);
    } else {
      row = createRow(item);
    }
    renderRow(row, item);
    const expected = previous ? previous.nextElementSibling : elements.jobList.firstElementChild;
    if (expected !== row) {
      elements.jobList.insertBefore(row, expected);
    }
    previous = row;
  }
  for (const row of rows.values()) {
    row.remove();
  }
  setHidden(elements.queueEmpty, items.length > 0);

  const parts = [t("queue.running", { active, desired })];
  if (active > desired) parts.push(t("queue.draining"));
  else if (!started && counts.queued > 0) parts.push(t("queue.notStarted"));
  elements.queueStatus.textContent = parts.join(" · ");
  elements.queueCounts.textContent = t("queue.counts", counts);
  setHidden(elements.queuePaused, snapshot.pausedBy !== "missing_dependency");
  elements.queuePaused.textContent =
    snapshot.pausedBy === "missing_dependency" ? t("queue.pausedMissing") : "";

  elements.startButton.textContent = t(started ? "queue.pause" : "queue.start");
  elements.startButton.classList.toggle("primary", !started);
  for (const radio of elements.concurrency.querySelectorAll("input")) {
    radio.checked = Number(radio.value) === desired;
    radio.disabled = Number(radio.value) > max;
  }
  elements.cancelAll.disabled = counts.queued + counts.running === 0;
  elements.retryFailed.disabled = counts.failed === 0;
  elements.clearCompleted.disabled = counts.completed === 0;
  document.dispatchEvent(new CustomEvent("highvi:busy", { detail: active > 0 }));
}

function scheduleRender() {
  if (!renderPending) {
    renderPending = true;
    requestAnimationFrame(render);
  }
}

function acceptSnapshot(next) {
  // Events arrive in order, but a command reply may be older than an event.
  if (next && (!snapshot || next.revision > snapshot.revision)) {
    snapshot = next;
    scheduleRender();
  }
}

listen("queue-changed", ({ payload }) => acceptSnapshot(payload)).catch(showError);

listen("queue-progress", ({ payload }) => {
  if (!snapshot || !payload) return;
  const item = snapshot.items.find((candidate) => candidate.id === payload.id);
  // Only the current attempt of a running item may change what is shown.
  if (!item || item.attempt !== payload.attempt || !["running", "cancelling"].includes(item.status)) {
    return;
  }
  Object.assign(item, {
    stage: payload.stage,
    title: payload.title ?? item.title,
    percent: payload.percent ?? null,
    speed: payload.speed ?? null,
    eta: payload.eta ?? null,
  });
  const row = elements.jobList.querySelector(`[data-id="${item.id}"]`);
  if (row) {
    row.querySelector(".job-title").textContent = item.title || item.host || "";
    row.querySelector(".job-status").textContent = statusText(item);
    setProgress(row, item);
  }
}).catch(showError);

elements.startButton.addEventListener("click", () => {
  invoke(snapshot?.started ? "queue_pause" : "queue_start").catch(showError);
});

elements.concurrency.addEventListener("change", (event) => {
  const desired = Number(event.target.value);
  invoke("queue_set_concurrency", { desired }).catch(showError);
});

elements.cancelAll.addEventListener("click", () => invoke("queue_cancel_all").catch(showError));
elements.retryFailed.addEventListener("click", () => invoke("queue_retry_failed").catch(showError));
elements.clearCompleted.addEventListener("click", () =>
  invoke("queue_clear_completed").catch(showError),
);
elements.openFolder.addEventListener("click", () =>
  invoke("open_output_folder").catch(showError),
);

// ------------------------------------------------------------ language

for (const button of document.querySelectorAll("[data-lang]")) {
  button.addEventListener("click", () => I18N.set(button.dataset.lang));
}

document.addEventListener("highvi:language", () => {
  renderMissingTools();
  renderAddResult();
  if (lastError) {
    elements.errorMessage.textContent = errorText(lastError, "page");
  }
  scheduleRender();
});
I18N.apply();

for (const radio of elements.form.querySelectorAll('input[name="mode"]')) {
  radio.addEventListener("change", applyMode);
}
applyMode();

invoke("queue_snapshot").then(acceptSnapshot).catch(showError);

invoke("dependency_status")
  .then((status) => {
    missingTools = [
      ...(status.ytDlp ? [] : ["yt-dlp"]),
      ...(status.ffmpeg ? [] : ["FFmpeg"]),
    ];
    browserMissing = !status.browser;
    outdated =
      typeof status.ytDlpRecommended === "string" && typeof status.ytDlpVersion === "string"
        ? { installed: status.ytDlpVersion, recommended: status.ytDlpRecommended }
        : null;
    renderMissingTools();
  })
  .catch(() => {
    // Not fatal: a job reports missing tools itself.
  });
