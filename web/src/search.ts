const input = document.querySelector<HTMLInputElement>("#search-query");
const suggestions = document.querySelector<HTMLDataListElement>("#search-suggestions");
let request: AbortController | undefined;
let timer: ReturnType<typeof setTimeout>;
input?.addEventListener("input", () => {
  clearTimeout(timer);
  request?.abort();
  suggestions?.replaceChildren();
  timer = setTimeout(async () => {
    const value = input.value;
    let start = 0;
    let quoted = false;
    for (let i = 0; i < value.length; i++) {
      if (value[i] === '"') quoted = !quoted;
      else if (/\s/.test(value[i]) && !quoted) start = i + 1;
    }
    const term = value.slice(start);
    const colon = term.indexOf(":");
    const kind = colon < 0 ? undefined : term.slice(0, colon);
    if (kind && !["tag", "place", "camera", "lens"].includes(kind)) return;
    const prefix = (colon < 0 ? term : term.slice(colon + 1)).replace(/^"/, "").replace(/"$/, "");
    if (!prefix || prefix.length > 80 || !suggestions) return;
    const controller = new AbortController();
    request = controller;
    try {
      const response = await fetch(`/api/v1/suggest?q=${encodeURIComponent(prefix)}`, { signal: controller.signal });
      if (!response.ok) return;
      const data: unknown = await response.json();
      if (!Array.isArray(data) || input.value !== value) return;
      for (const item of data.slice(0, 20)) {
        if (!item || !["tag", "place", "camera", "lens"].includes(item.kind) || typeof item.value !== "string") continue;
        if (kind && item.kind !== kind) continue;
        if (/["\x00-\x1f\x7f]/.test(item.value)) continue;
        const option = document.createElement("option");
        option.value = `${value.slice(0, start)}${item.kind}:"${item.value}"`;
        suggestions.append(option);
      }
    } catch { /* Another keystroke or a temporary service failure. */ }
  }, 150);
});
