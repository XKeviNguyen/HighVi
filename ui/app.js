const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const elements = {
  discoverForm: document.querySelector("#discover-form"),
  discoverButton: document.querySelector("#discover-button"),
  pageUrl: document.querySelector("#page-url"),
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

let currentPageUrl = "";

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

elements.discoverForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  clearMessages();
  setHidden(elements.resultCard, true);
  setHidden(elements.statusCard, true);
  setBusy(elements.discoverButton, true, "Đang tìm…");

  try {
    currentPageUrl = elements.pageUrl.value.trim();
    const result = await invoke("discover_video", { pageUrl: currentPageUrl });
    elements.resultHeading.textContent = result.title;
    elements.playlistUrl.textContent = result.playlistUrl;
    elements.refererUrl.textContent = result.referer;
    elements.filename.value = result.suggestedFilename;
    setHidden(elements.resultCard, false);
    elements.filename.focus();
  } catch (error) {
    showError(error);
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
        pageUrl: currentPageUrl,
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
