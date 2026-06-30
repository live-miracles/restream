export type OutputControlIntent = "starting" | "stopping";

const pendingOutputControlIntents = new Map<string, OutputControlIntent>();

function outputControlKey(pipeId: string, outId: string): string {
  return `${pipeId}:${outId}`;
}

export function beginOutputControlIntent(
  pipeId: string,
  outId: string,
  intent: OutputControlIntent,
): void {
  pendingOutputControlIntents.set(outputControlKey(pipeId, outId), intent);
}

export function finishOutputControlIntent(pipeId: string, outId: string): void {
  pendingOutputControlIntents.delete(outputControlKey(pipeId, outId));
}

export function getOutputControlIntent(
  pipeId: string,
  outId: string,
): OutputControlIntent | null {
  return (
    pendingOutputControlIntents.get(outputControlKey(pipeId, outId)) ?? null
  );
}
