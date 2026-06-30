import type { OutputView } from "../types.js";

function isOutputIntentStopped(output: OutputView | null | undefined): boolean {
  return output?.desiredState === "stopped";
}

function isOutputRunning(output: OutputView | null | undefined): boolean {
  return (
    output?.status === "on" ||
    output?.status === "running" ||
    output?.status === "warning"
  );
}

function isOutputRetrying(output: OutputView | null | undefined): boolean {
  return output?.status === "retrying" || output?.retrying === true;
}

function isOutputFlapping(output: OutputView | null | undefined): boolean {
  return output?.flapping === true;
}

function isOutputManagedActive(output: OutputView | null | undefined): boolean {
  return isOutputRunning(output) || isOutputRetrying(output);
}

function isOutputUnexpectedlyDown(
  output: OutputView | null | undefined,
): boolean {
  return !isOutputIntentStopped(output) && !isOutputManagedActive(output);
}

export {
  isOutputIntentStopped,
  isOutputFlapping,
  isOutputManagedActive,
  isOutputRunning,
  isOutputRetrying,
  isOutputUnexpectedlyDown,
};
