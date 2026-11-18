// Check the static HTML contract once at each lookup instead of casting nullable nodes.
export function byId(id: string): HTMLElement;
export function byId<K extends keyof HTMLElementTagNameMap>(
  id: string,
  tag: K,
): HTMLElementTagNameMap[K];
export function byId(id: string, tag?: keyof HTMLElementTagNameMap): HTMLElement {
  const element = document.getElementById(id);
  if (!element || (tag && element.localName !== tag)) {
    throw new Error(`Missing ${tag ?? "element"} #${id}`);
  }
  return element;
}
