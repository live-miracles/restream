interface MetricParts {
  valueText: string;
  unitText: string;
}

function formatBitrateKbpsParts(
  kbps: number | null | undefined,
): MetricParts | null {
  const value = Number(kbps);
  if (!Number.isFinite(value) || value < 0) return null;
  if (value >= 1000 * 1000) {
    return { valueText: (value / (1000 * 1000)).toFixed(2), unitText: "Gb/s" };
  }
  if (value >= 1000) {
    return { valueText: (value / 1000).toFixed(1), unitText: "Mb/s" };
  }
  return { valueText: value.toFixed(1), unitText: "Kb/s" };
}

function setMetricValueWithSubtleUnit(
  target: Element | null,
  parts: MetricParts | null,
  fallback = "--",
): void {
  if (!target) return;

  if (!parts) {
    if (target.textContent !== fallback) target.textContent = fallback;
    return;
  }

  let valueSpan = target.children[0] as HTMLElement | undefined;
  let unitSpan = target.children[1] as HTMLElement | undefined;
  const hasReusableSpans =
    valueSpan?.dataset.metricRole === "value" &&
    unitSpan?.dataset.metricRole === "unit";

  if (!hasReusableSpans) {
    valueSpan = document.createElement("span");
    valueSpan.dataset.metricRole = "value";

    unitSpan = document.createElement("span");
    unitSpan.dataset.metricRole = "unit";
    unitSpan.className = "ml-1 text-xs opacity-70";

    // Keep the subtle-unit DOM stable so polling only patches text.
    target.replaceChildren(valueSpan, unitSpan);
  }

  if (valueSpan.textContent !== parts.valueText) {
    valueSpan.textContent = parts.valueText;
  }
  if (unitSpan.textContent !== parts.unitText) {
    unitSpan.textContent = parts.unitText;
  }
}

function setBitrateWithSubtleUnit(
  elemId: string,
  kbps: number | null | undefined,
  fallback = "--",
): void {
  const target = document.getElementById(elemId);
  if (!target) return;

  const parts = formatBitrateKbpsParts(kbps);
  setMetricValueWithSubtleUnit(target, parts, fallback);
}

function setBadgeBitrateWithSubtleUnit(
  badgeElem: Element | null,
  kbps: number | null | undefined,
  fallback = "warming...",
): void {
  if (!badgeElem) return;

  const parts = formatBitrateKbpsParts(kbps);
  if (!parts) {
    badgeElem.textContent = fallback;
    return;
  }

  badgeElem.textContent = `${parts.valueText} ${parts.unitText}`;
}

function setMetricsBitrateWithSubtleUnit(
  selector: string,
  kbps: number | null | undefined,
  fallback = "--",
): void {
  const targets = document.querySelectorAll(selector);
  const parts = formatBitrateKbpsParts(kbps);

  targets.forEach((target) => {
    setMetricValueWithSubtleUnit(target, parts, fallback);
  });
}

function setMetricsValueWithSubtleUnit(
  selector: string,
  parts: MetricParts | null,
  fallback = "--",
): void {
  document.querySelectorAll(selector).forEach((target) => {
    setMetricValueWithSubtleUnit(target, parts, fallback);
  });
}

export {
  formatBitrateKbpsParts,
  setMetricValueWithSubtleUnit,
  setBitrateWithSubtleUnit,
  setBadgeBitrateWithSubtleUnit,
  setMetricsBitrateWithSubtleUnit,
  setMetricsValueWithSubtleUnit,
};

export type { MetricParts };
