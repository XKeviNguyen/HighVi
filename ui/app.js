const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const elements = {
  discoverForm: document.querySelector("#discover-form"),
  discoverButton: document.querySelector("#discover-button"),
  pageFields: document.querySelector("#page-fields"),
  pageUrl: document.querySelector("#page-url"),
  directFields: document.querySelector("#direct-fields"),
  playlistInput: document.querySelector("#playlist-input"),
  refererInput: document.querySelector("#referer-input"),
  resultCard: document.querySelector("#result-card"),
  resultHeading: document.querySelector("#result-heading"),
  playlistUrl: document.querySelector("#playlist-url"),
  refererUrl: document.querySelector("#referer-url"),
  downloadForm: document.querySelector("#download-form"),
  downloadButton: document.querySelector("#download-button"),
  filename: document.querySelector("#filename"),
  statusCard: document.querySelector("#status-card"),
  statusMessage: document.querySelector("#status-message"),
  statusPercent: document.querySelector("#status-percent"),
  statusSpeed: document.querySelector("#status-speed"),
  statusEta: document.querySelector("#status-eta"),
  progressTrack: document.querySelector("#progress-track"),
  progressBar: document.querySelector("#progress-bar"),
  errorCard: document.querySelector("#error-card"),
  errorMessage: document.querySelector("#error-message"),
  successCard: document.querySelector("#success-card"),
  savedPath: document.querySelector("#saved-path"),
};

// The source of the last successful discovery. Rust re-validates it and
// rediscovers before downloading, so this is not a security boundary.
let currentSource = null;
// Incremented whenever the inputs change, so a discovery that finishes
// afterwards cannot show results or errors for a source the user has edited.
let discoveryGeneration = 0;

function invalidateDiscovery() {
  discoveryGeneration += 1;
  currentSource = null;
  setHidden(elements.resultCard, true);
}

function selectedMode() {
  return elements.discoverForm.querySelector('input[name="mode"]:checked').value;
}

function applyMode() {
  const direct = selectedMode() === "directHls";
  setHidden(elements.pageFields, direct);
  setHidden(elements.directFields, !direct);
  elements.pageUrl.required = !direct;
  elements.playlistInput.required = direct;
  elements.refererInput.required = direct;
  invalidateDiscovery();
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

function setHidden(element, hidden) {
  element.classList.toggle("hidden", hidden);
}

function clearMessages() {
  setHidden(elements.errorCard, true);
  setHidden(elements.successCard, true);
}

const { t } = I18N;

// Kept so the visible error and progress text can be re-rendered when the
// user switches language.
let lastError = null;
let lastProgress = null;

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
    // Not a URL; Rust has already rejected it.
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
  return error?.message || t("error.unknown");
}

function showError(error, source = null) {
  lastError = { error, kind: sourceKind(source) };
  elements.errorMessage.textContent = errorText(lastError);
  setHidden(elements.errorCard, false);
}

function setBusy(button, busy, busyKey) {
  button.disabled = busy;
  button.textContent = t(busy ? busyKey : button.dataset.i18n);
  button.dataset.busyKey = busy ? busyKey : "";
}

const progressKeys = {
  recheck: "progress.recheck",
  checking: "progress.checking",
  downloading: "progress.downloading",
  processing: "progress.processing",
  finished: "progress.finished",
};

function updateProgress(progress) {
  lastProgress = progress;
  setHidden(elements.statusCard, false);
  elements.statusMessage.textContent = t(progressKeys[progress.stage] ?? "progress.working");
  elements.statusPercent.textContent = progress.percent || "";
  elements.statusSpeed.textContent = progress.speed
    ? t("progress.speed", { value: progress.speed })
    : "";
  elements.statusEta.textContent = progress.eta ? t("progress.eta", { value: progress.eta }) : "";

  const parsed = Number.parseFloat(progress.percent || "");
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

listen("download-progress", ({ payload }) => updateProgress(payload)).catch((error) =>
  showError(error),
);

for (const button of document.querySelectorAll("[data-lang]")) {
  button.addEventListener("click", () => I18N.set(button.dataset.lang));
}

document.addEventListener("highvi:language", () => {
  for (const button of [elements.discoverButton, elements.downloadButton]) {
    if (button.dataset.busyKey) {
      button.textContent = t(button.dataset.busyKey);
    }
  }
  if (lastError && !elements.errorCard.classList.contains("hidden")) {
    elements.errorMessage.textContent = errorText(lastError);
  }
  if (lastProgress && !elements.statusCard.classList.contains("hidden")) {
    updateProgress(lastProgress);
  }
  if (currentSource && !elements.resultCard.classList.contains("hidden")) {
    elements.refererUrl.textContent = elements.refererUrl.dataset.value || t("result.refererNone");
  }
});
I18N.apply();

for (const radio of elements.discoverForm.querySelectorAll('input[name="mode"]')) {
  radio.addEventListener("change", () => {
    clearMessages();
    applyMode();
  });
}
for (const input of [elements.pageUrl, elements.playlistInput, elements.refererInput]) {
  input.addEventListener("input", invalidateDiscovery);
}
applyMode();

elements.discoverForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearMessages();
  invalidateDiscovery();
  const generation = discoveryGeneration;
  setHidden(elements.statusCard, true);
  setBusy(elements.discoverButton, true, "discover.busy");
  const source = readSource();

  try {
    const result = await invoke("discover_video", { source });
    if (generation !== discoveryGeneration) {
      return;
    }
    currentSource = source;
    elements.resultHeading.textContent = result.title;
    elements.playlistUrl.textContent = result.playlistDisplay;
    elements.refererUrl.dataset.value = result.refererDisplay || "";
    elements.refererUrl.textContent = result.refererDisplay || t("result.refererNone");
    elements.filename.value = result.suggestedFilename;
    setHidden(elements.resultCard, false);
    elements.filename.focus();
  } catch (error) {
    if (generation === discoveryGeneration) {
      showError(error, source);
    }
  } finally {
    setBusy(elements.discoverButton, false, "");
  }
});
elements.downloadForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearMessages();
  setBusy(elements.downloadButton, true, "download.busy");
  updateProgress({ stage: "recheck" });
  const source = currentSource;

  try {
    const result = await invoke("download_video", {
      request: {
        source,
        filename: elements.filename.value,
      },
    });
    elements.savedPath.textContent = result.savedTo;
    setHidden(elements.successCard, false);
  } catch (error) {
    setHidden(elements.statusCard, true);
    showError(error, source);
  } finally {
    setBusy(elements.downloadButton, false, "");
  }
});
