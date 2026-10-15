const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { t } = I18N;

const elements = {
  form: document.querySelector("#job-form"),
  pageFields: document.querySelector("#page-fields"),
  pageUrl: document.querySelector("#page-url"),
  directFields: document.querySelector("#direct-fields"),
  playlistInput: document.querySelector("#playlist-input"),
  refererInput: document.querySelector("#referer-input"),
  downloadButton: document.querySelector("#download-button"),
  depsCard: document.querySelector("#deps-card"),
  depsMessage: document.querySelector("#deps-message"),
  browserNote: document.querySelector("#browser-note"),
  statusCard: document.querySelector("#status-card"),
  statusTitle: document.querySelector("#status-title"),
  statusMessage: document.querySelector("#status-message"),
  statusPercent: document.querySelector("#status-percent"),
  statusSpeed: document.querySelector("#status-speed"),
  statusEta: document.querySelector("#status-eta"),
  progressTrack: document.querySelector("#progress-track"),
  progressBar: document.querySelector("#progress-bar"),
  cancelButton: document.querySelector("#cancel-button"),
  errorCard: document.querySelector("#error-card"),
  errorMessage: document.querySelector("#error-message"),
  errorHint: document.querySelector("#error-hint"),
  diagnostics: document.querySelector("#diagnostics"),
  diagnosticsText: document.querySelector("#diagnostics-text"),
  copyDiagnostics: document.querySelector("#copy-diagnostics"),
  copyStatus: document.querySelector("#copy-status"),
  outdatedNote: document.querySelector("#outdated-note"),
  outdatedMessage: document.querySelector("#outdated-message"),
  cancelledCard: document.querySelector("#cancelled-card"),
  successCard: document.querySelector("#success-card"),
  savedPath: document.querySelector("#saved-path"),
  openFolderButton: document.querySelector("#open-folder-button"),
};

// The running job's ID. Progress events carry it, so a late event from an
// earlier job can never change what the current one shows.
let currentJob = null;
// Kept so visible text can be re-rendered when the language changes.
let lastError = null;
let lastUpdate = null;
let lastTitle = null;
let missingTools = [];
let browserMissing = false;
let outdated = null;

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

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// Status and results appear below the form; at small sizes or high zoom
// they would otherwise be off screen.
function reveal(element) {
  element.scrollIntoView({ block: "nearest", behavior: reducedMotion.matches ? "auto" : "smooth" });
}

function selectedMode() {
  return elements.form.querySelector('input[name="mode"]:checked').value;
}

function applyMode() {
  const direct = selectedMode() === "directHls";
  setHidden(elements.pageFields, direct);
  setHidden(elements.directFields, !direct);
  elements.pageUrl.required = !direct;
  elements.playlistInput.required = direct;
  elements.refererInput.required = direct;
}

function readSource() {
  if (selectedMode() === "directHls") {
    return {
      mode: "directHls",
      playlistUrl: elements.playlistInput.value.trim(),
      refererPageUrl: elements.refererInput.value.trim(),
    };
  }
  return { mode: "page", pageUrl: elements.pageUrl.value.trim() };
}

// Which advice fits a 403 depends on how the user gave us the source.
function sourceKind(source) {
  if (source?.mode === "directHls") {
    return "direct";
  }
  try {
    if (new URL(source?.pageUrl ?? "").pathname.toLowerCase().endsWith(".m3u8")) {
      return "bare";
    }
  } catch {
    // Not a URL; Rust rejects it.
  }
  return "page";
}

function errorText({ error, kind }) {
  const code = typeof error?.code === "string" ? error.code : "";
  if (code === "access_denied" || code === "host_firewall_block") {
    return t(`error.${code}.${kind}`);
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

function renderError() {
  elements.errorMessage.textContent = errorText(lastError);
  const hint = hintText(lastError.error?.hint);
  elements.errorHint.textContent = hint;
  setHidden(elements.errorHint, !hint);
  const report = typeof lastError.error?.diagnostics === "string" ? lastError.error.diagnostics : "";
  elements.diagnosticsText.textContent = report;
  elements.copyStatus.textContent = "";
  setHidden(elements.diagnostics, !report);
}

function clearMessages() {
  for (const card of [elements.errorCard, elements.successCard, elements.cancelledCard]) {
    setHidden(card, true);
  }
  lastError = null;
}

function showError(error, source) {
  lastError = { error, kind: sourceKind(source) };
  renderError();
  setHidden(elements.errorCard, false);
}

function setRunning(running) {
  document.dispatchEvent(new CustomEvent("highvi:busy", { detail: running }));
  elements.downloadButton.disabled = running;
  elements.downloadButton.textContent = t(running ? "download.busy" : "download.button");
  elements.cancelButton.disabled = false;
  elements.cancelButton.textContent = t("cancel.button");
  for (const input of elements.form.querySelectorAll("input")) {
    input.disabled = running;
  }
  setHidden(elements.statusCard, !running);
}

function renderUpdate(update) {
  lastUpdate = update;
  if (update.title) {
    lastTitle = update.title;
  }
  setHidden(elements.statusTitle, !lastTitle);
  elements.statusTitle.textContent = lastTitle ? t("progress.title", { title: lastTitle }) : "";
  elements.statusMessage.textContent = t(stageKeys[update.stage] ?? "progress.working");
  elements.statusPercent.textContent = update.percent || "";
  elements.statusSpeed.textContent = update.speed ? t("progress.speed", { value: update.speed }) : "";
  elements.statusEta.textContent = update.eta ? t("progress.eta", { value: update.eta }) : "";

  const parsed = Number.parseFloat(update.percent || "");
  if (Number.isFinite(parsed)) {
    const bounded = Math.min(100, Math.max(0, parsed));
    elements.progressBar.style.width = `${bounded}%`;
    elements.progressTrack.setAttribute("aria-valuenow", String(bounded));
    elements.progressTrack.classList.remove("indeterminate");
  } else {
    elements.progressBar.style.width = "35%";
    elements.progressTrack.removeAttribute("aria-valuenow");
    elements.progressTrack.classList.add("indeterminate");
  }
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

listen("job-update", ({ payload }) => {
  if (payload?.jobId && payload.jobId === currentJob) {
    renderUpdate(payload);
  }
}).catch((error) => showError(error, null));

for (const button of document.querySelectorAll("[data-lang]")) {
  button.addEventListener("click", () => I18N.set(button.dataset.lang));
}

document.addEventListener("highvi:language", () => {
  elements.downloadButton.textContent = t(currentJob ? "download.busy" : "download.button");
  elements.cancelButton.textContent = t(elements.cancelButton.disabled ? "cancel.busy" : "cancel.button");
  if (lastError) {
    renderError();
  }
  if (lastUpdate && currentJob) {
    renderUpdate(lastUpdate);
  }
  renderMissingTools();
});
I18N.apply();

for (const radio of elements.form.querySelectorAll('input[name="mode"]')) {
  radio.addEventListener("change", () => {
    clearMessages();
    applyMode();
  });
}
applyMode();

elements.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (currentJob) {
    return;
  }
  const jobId = crypto.randomUUID();
  const source = readSource();
  currentJob = jobId;
  lastUpdate = null;
  lastTitle = null;
  clearMessages();
  setRunning(true);
  renderUpdate({ stage: "discovering" });
  reveal(elements.statusCard);

  try {
    const result = await invoke("start_job", { jobId, source });
    elements.savedPath.textContent = result.savedTo;
    setHidden(elements.successCard, false);
    reveal(elements.successCard);
    elements.openFolderButton.focus({ preventScroll: true });
  } catch (error) {
    if (error?.code === "cancelled") {
      setHidden(elements.cancelledCard, false);
      reveal(elements.cancelledCard);
    } else {
      showError(error, source);
      reveal(elements.errorCard);
    }
  } finally {
    currentJob = null;
    setRunning(false);
  }
});

elements.cancelButton.addEventListener("click", () => {
  if (!currentJob) {
    return;
  }
  elements.cancelButton.disabled = true;
  elements.cancelButton.textContent = t("cancel.busy");
  invoke("cancel_job").catch((error) => showError(error, null));
});

// The report is plain text without URLs; the clipboard API needs the click
// that triggered it, and a selection is the fallback if it is refused.
elements.copyDiagnostics.addEventListener("click", async () => {
  const text = elements.diagnosticsText.textContent;
  try {
    await navigator.clipboard.writeText(text);
    elements.copyStatus.textContent = t("diagnostics.copied");
  } catch {
    elements.diagnosticsText.closest("details").open = true;
    const range = document.createRange();
    range.selectNodeContents(elements.diagnosticsText);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    elements.copyStatus.textContent = t("diagnostics.copyFailed");
  }
});

elements.openFolderButton.addEventListener("click", () => {
  invoke("open_output_folder").catch((error) => showError(error, null));
});

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
