import { startIngest, stopIngest } from "../core/api.js";
import { state } from "../core/state.js";
import { getUrlParam } from "../core/utils.js";
import { refreshDashboard } from "./dashboard.js";

export function renderFileIngestControl(): void {
  const button = document.getElementById(
    "file-ingest-pipe-btn",
  ) as HTMLButtonElement | null;
  if (!button) return;

  const selectedPipeId = getUrlParam("p");
  if (!selectedPipeId) {
    hideFileIngestControl(button);
    return;
  }

  const pipe = state.pipelines.find((entry) => entry.id === selectedPipeId);
  const isFileSource = (pipe?.inputSource || "").startsWith("file:");
  const fileIngest = pipe?.fileIngest || null;
  const configured = Boolean(isFileSource && fileIngest?.configured);
  if (!configured || !pipe) {
    hideFileIngestControl(button);
    return;
  }

  const running = Boolean(fileIngest?.running);
  button.classList.remove("hidden");
  button.textContent = running ? "Stop File" : "Start File";
  button.classList.toggle("btn-error", running);
  button.classList.toggle("btn-accent", !running);
  button.classList.toggle("btn-outline", !running);
  button.disabled = !fileIngest?.id;
  button.classList.toggle("btn-disabled", !fileIngest?.id);
  button.title = fileIngest?.filename
    ? `${running ? "Stop" : "Start"} file ingest for ${fileIngest.filename}`
    : "";
  button.onclick = async () => {
    if (!fileIngest?.id) return;
    if (running) {
      await stopIngest(fileIngest.id);
    } else {
      await startIngest(fileIngest.id);
    }
    await refreshDashboard();
  };
}

function hideFileIngestControl(button: HTMLButtonElement): void {
  button.classList.add("hidden");
  button.disabled = true;
  button.classList.add("btn-disabled");
  button.title = "";
  button.onclick = null;
}
