/** Reinitialize islands after a document swap and release detached-page listeners. */
export function enhance(init: (signal: AbortSignal) => void): void {
  let controller: AbortController;
  const run = () => { controller?.abort(); controller = new AbortController(); init(controller.signal); };
  document.addEventListener("yhotos:navigate", run);
  run();
}
export function plainClick(event: MouseEvent): boolean {
  return event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey;
}
export function reducedData(): boolean {
  return !!(navigator as Navigator & { connection?: { saveData?: boolean } }).connection?.saveData || matchMedia("(prefers-reduced-data: reduce)").matches;
}
