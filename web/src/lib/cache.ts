/** Small insertion-ordered LRU cache with a bounded lifetime. */
export class PrefetchCache<T> {
  private items = new Map<string, { value: T; expires: number }>();
  constructor(private max = 10, private ttl = 15_000) {}
  get(key: string, now = Date.now()): T | undefined {
    const item = this.items.get(key);
    if (!item) return;
    this.items.delete(key);
    if (item.expires <= now) return;
    this.items.set(key, item);
    return item.value;
  }
  put(key: string, value: T, now = Date.now()): void {
    this.items.delete(key); this.items.set(key, { value, expires: now + this.ttl });
    while (this.items.size > this.max) this.items.delete(this.items.keys().next().value!);
  }
  clear(): void { this.items.clear(); }
}
