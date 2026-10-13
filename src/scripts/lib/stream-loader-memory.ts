// Remembers which hosts the WebView's own fetch could not read a stream from (the provider
// sends no cross-site permission header), so later connections go straight to the Rust-side
// loader. Without this every tune and every reconnect opens a doomed first connection, and a
// provider that allows one connection per account counts it against the real one.
//
// Kept for the app session: pages are separate documents, so a plain Set would forget on each
// navigation.

const STORAGE_KEY = "kd_stream_loader_hosts"

function originOf(url: string): string | null {
  try {
    return new URL(url).origin
  } catch {
    return null
  }
}

function readOrigins(): string[] {
  try {
    const parsed = JSON.parse(sessionStorage.getItem(STORAGE_KEY) || "[]")
    return Array.isArray(parsed) ? parsed.filter((entry) => typeof entry === "string") : []
  } catch {
    return []
  }
}

/** True when a stream from this host is known to need the Rust-side loader. */
export function needsRustLoader(url: string): boolean {
  const origin = originOf(url)
  return origin !== null && readOrigins().includes(origin)
}

/** Called when the WebView's fetch failed before any of the stream could be read. */
export function rememberRustLoader(url: string): void {
  const origin = originOf(url)
  if (origin === null) return
  const origins = readOrigins()
  if (origins.includes(origin)) return
  try {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify([...origins, origin].slice(-50)))
  } catch {}
}
