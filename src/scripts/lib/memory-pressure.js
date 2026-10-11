// Android reports when memory runs short or the app leaves the screen (MainActivity forwards
// onTrimMemory to window.__xtTrimMemory). Modules register here what they can drop and rebuild.

// ComponentCallbacks2 levels: below RUNNING_LOW nothing needs to go yet.
const TRIM_RUNNING_LOW = 10

const handlers = new Set()
let lowMemoryDevice = null

/** Registers a callback run whenever the system asks the app to free memory. */
export function onMemoryPressure(handler) {
  handlers.add(handler)
  return () => handlers.delete(handler)
}

export function handleTrimLevel(level) {
  if ((Number(level) || 0) < TRIM_RUNNING_LOW) return false
  for (const handler of handlers) {
    try {
      handler()
    } catch {}
  }
  return true
}

/** True on Android devices with little RAM, and on TV sticks and boxes, which rarely have more. */
export function isLowMemoryDevice() {
  if (lowMemoryDevice === null) {
    lowMemoryDevice = false
    try {
      const info = typeof window !== "undefined" ? window.AndroidDeviceInfo : null
      if (info) {
        const lowRam = typeof info.isLowRam === "function" && info.isLowRam()
        const tv = typeof info.isTv === "function" && info.isTv()
        lowMemoryDevice = !!(lowRam || tv)
      }
    } catch {}
  }
  return lowMemoryDevice
}

if (typeof window !== "undefined") {
  window.__xtTrimMemory = handleTrimLevel
}
