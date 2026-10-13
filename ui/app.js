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

function showError(error) {
  const fallback = "HighVi gặp lỗi không xác định.";
  elements.errorMessage.textContent = error?.message || fallback;
  setHidden(elements.errorCard, false);
}

function setBusy(button, busy, label) {
  button.disabled = busy;
  button.dataset.defaultLabel ||= button.textContent;
  button.textContent = busy ? label : button.dataset.defaultLabel;
}

function updateProgress(progress) {
  setHidden(elements.statusCard, false);
  elements.statusMessage.textContent = progress.message || "Đang xử lý…";
  elements.statusPercent.textContent = progress.percent || "";
  elements.statusSpeed.textContent = progress.speed ? `Tốc độ: ${progress.speed}` : "";
  elements.statusEta.textContent = progress.eta ? `Còn lại: ${progress.eta}` : "";

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

listen("download-progress", ({ payload }) => updateProgress(payload)).catch(showError);

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
  setBusy(elements.discoverButton, true, "Đang tìm…");

  try {
    const source = readSource();
    const result = await invoke("discover_video", { source });
    if (generation !== discoveryGeneration) {
      return;
    }
    currentSource = source;
    elements.resultHeading.textContent = result.title;
    elements.playlistUrl.textContent = result.playlistDisplay;
    elements.refererUrl.textContent = result.refererDisplay || "Không gửi";
    elements.filename.value = result.suggestedFilename;
    setHidden(elements.resultCard, false);
    elements.filename.focus();
  } catch (error) {
    if (generation === discoveryGeneration) {
      showError(error);
    }
  } finally {
    setBusy(elements.discoverButton, false, "");
  }
});
elements.downloadForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearMessages();
  setBusy(elements.downloadButton, true, "Đang tải…");
  updateProgress({ message: "Đang kiểm tra lại playlist…" });

  try {
    const result = await invoke("download_video", {
      request: {
        source: currentSource,
        filename: elements.filename.value,
      },
    });
    elements.savedPath.textContent = result.savedTo;
    setHidden(elements.successCard, false);
  } catch (error) {
    setHidden(elements.statusCard, true);
    showError(error);
  } finally {
    setBusy(elements.downloadButton, false, "");
  }
});
