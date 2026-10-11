// Client for the native download engine (src-tauri/src/downloader.rs). The engine owns the queue and
// the byte moving, so downloads keep going across page navigations and while the app is backgrounded;
// this module hands it jobs, relays its events, and tells it how many connections it may use.

import { log } from "@/scripts/lib/log.js"
import { getUserAgent, getDownloadConcurrency } from "@/scripts/lib/app-settings.js"
import { getMaxConnectionsSync, getActivePlaylistIdSync } from "@/scripts/lib/account-info.js"
import { splitUrlAuth } from "@/scripts/lib/url-auth"
import { DEFAULT_BROWSER_UA } from "@/scripts/lib/provider-fetch.js"

const isTauri =
  typeof window !== "undefined" &&
  (!!window.__TAURI_INTERNALS__ || !!window.__TAURI__)

const EVENTS = ["xt:dl-progress", "xt:dl-done", "xt:dl-error"]
// A finished download plays from disk through the asset protocol and uses no provider connection.
const LOCAL_SOURCE_RX = /^(asset:|https?:\/\/asset\.localhost)/i
const NETWORK_EMPTY = 0
const NETWORK_NO_SOURCE = 3
const STREAM_RECHECK_MS = 3000

let closeWatch = null
let statusHandler = null
let connectPromise = null
let statusFile = null
let nativePlayerOpen = false
let lastLimit = -1

async function invoke(command, args) {
  const core = await import("@tauri-apps/api/core")
  return core.invoke(command, args)
}

export function nativeDownloadsAvailable() {
  return isTauri
}

/**
 * How many downloads may run right now: the user's setting, capped by the provider's connection
 * limit, minus the connection an open stream is using. Zero parks every download until playback ends.
 */
export function allowedDownloads({ configured, providerCap, streamOpen }) {
  let limit = Math.max(1, Number(configured) || 1)
  const cap = Number(providerCap) || 0
  if (cap > 0) {
    limit = Math.min(limit, cap)
    if (streamOpen) limit = Math.min(limit, Math.max(0, cap - 1))
  }
  return limit
}

/** True while a video element holds a network stream open (playing or paused mid-stream). */
export function isStreamOpen(video) {
  if (!video || video.ended) return false
  const source = video.currentSrc || video.src || ""
  if (!source || LOCAL_SOURCE_RX.test(source)) return false
  return video.networkState !== NETWORK_EMPTY && video.networkState !== NETWORK_NO_SOURCE
}

function anyStreamOpen() {
  if (nativePlayerOpen) return true
  if (typeof document === "undefined") return false
  for (const video of document.querySelectorAll("video")) {
    if (isStreamOpen(video)) return true
  }
  return false
}

export async function syncDownloadLimit() {
  if (!isTauri) return
  const streamOpen = anyStreamOpen()
  watchForStreamClose(streamOpen)
  const limit = allowedDownloads({
    configured: getDownloadConcurrency(),
    providerCap: getMaxConnectionsSync(getActivePlaylistIdSync()),
    streamOpen,
  })
  if (limit === lastLimit) return
  lastLimit = limit
  try {
    await invoke("download_set_limit", { limit })
  } catch (error) {
    lastLimit = -1
    log.warn("[xt:download] couldn't update the download limit:", error)
  }
}

// A player torn down by removing its <video> fires no event the page can see, so while a stream
// is counted as open it is re-checked on a slow timer. Nothing runs when no stream is open.
function watchForStreamClose(streamOpen) {
  if (streamOpen && closeWatch === null) {
    closeWatch = setInterval(syncDownloadLimit, STREAM_RECHECK_MS)
  } else if (!streamOpen && closeWatch !== null) {
    clearInterval(closeWatch)
    closeWatch = null
  }
}

/** Android's native player runs outside the page, so its launcher reports it here. */
export function noteNativePlayerOpen(open) {
  nativePlayerOpen = !!open
  syncDownloadLimit()
}

function installPlaybackGuard() {
  if (typeof document === "undefined") return
  // Media events don't bubble, so they are caught on the way down instead.
  for (const type of ["loadstart", "playing", "emptied", "ended", "abort"]) {
    document.addEventListener(type, (event) => {
      if (event.target instanceof HTMLVideoElement) syncDownloadLimit()
    }, true)
  }
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") {
      syncDownloadLimit()
      refreshNativeDownloads()
    }
  })
}

function deliver(job) {
  if (!job || typeof job.id !== "string" || !statusHandler) return
  try {
    statusHandler(job)
  } catch (error) {
    log.error("[xt:download] status handler failed:", error)
  }
}

/** Asks the engine what it is doing and replays that through the status handler. */
export async function refreshNativeDownloads() {
  if (!isTauri || !statusHandler) return
  try {
    const status = await invoke("download_status")
    statusFile = status?.statusFile || null
    for (const job of status?.jobs || []) deliver(job)
  } catch (error) {
    log.warn("[xt:download] couldn't read the download engine status:", error)
  }
}

/**
 * Starts relaying engine events to `onStatus` and catches up on anything that happened while no
 * page was listening. Safe to call more than once.
 */
export function connectNativeDownloads(onStatus) {
  if (!isTauri) return Promise.resolve(false)
  statusHandler = onStatus
  if (!connectPromise) {
    connectPromise = (async () => {
      const { listen } = await import("@tauri-apps/api/event")
      for (const name of EVENTS) {
        await listen(name, (event) => deliver(event.payload))
      }
      await refreshNativeDownloads()
      await syncDownloadLimit()
      return true
    })().catch((error) => {
      log.error("[xt:download] native download engine unavailable:", error)
      connectPromise = null
      return false
    })
  }
  return connectPromise
}

export async function enqueueNativeDownload({ id, url, title, path, fd }) {
  const { url: requestUrl, authorization } = splitUrlAuth(String(url))
  const spec = {
    id,
    url: requestUrl,
    title: title || "",
    userAgent: getUserAgent() || DEFAULT_BROWSER_UA,
    authorization: authorization || null,
    path: typeof fd === "number" ? null : path,
    fd: typeof fd === "number" ? fd : null,
  }
  await syncDownloadLimit()
  const job = await invoke("download_enqueue", { spec })
  startAndroidKeepAlive()
  return job
}

/** Resolves once the engine has let go of the file, so it can be deleted or reopened. */
export async function cancelNativeDownload(id) {
  if (!isTauri) return
  try {
    await invoke("download_cancel", { id })
  } catch (error) {
    log.warn("[xt:download] cancel failed:", error)
  }
}

export async function acknowledgeNativeDownload(id) {
  if (!isTauri) return
  try {
    await invoke("download_ack", { id })
  } catch {}
}

// ---------------------------------------------------------------------------
// Android: the engine writes through a descriptor the app opens, and a foreground
// service keeps the process alive while the screen is off.
// ---------------------------------------------------------------------------

function androidBridge() {
  return typeof window !== "undefined" ? window.AndroidDownloads : null
}

/** Opens a content:// destination for read/write. Returns -1 when the device can't provide one. */
export function openAndroidDescriptor(uri) {
  const bridge = androidBridge()
  if (!bridge?.openDescriptor) return -1
  const plain = typeof uri === "string" && uri.startsWith("{") ? safeUri(uri) : uri?.uri || uri
  if (typeof plain !== "string" || !plain.startsWith("content://")) return -1
  try {
    const descriptor = Number(bridge.openDescriptor(plain))
    return Number.isInteger(descriptor) && descriptor > 2 ? descriptor : -1
  } catch (error) {
    log.warn("[xt:download] couldn't open the destination for the native engine:", error)
    return -1
  }
}

function safeUri(serialized) {
  try {
    return JSON.parse(serialized)?.uri
  } catch {
    return null
  }
}

async function startAndroidKeepAlive() {
  const bridge = androidBridge()
  if (!bridge?.startService) return
  try {
    if (!statusFile) {
      const status = await invoke("download_status")
      statusFile = status?.statusFile || null
    }
    if (statusFile) bridge.startService(statusFile)
  } catch (error) {
    log.warn("[xt:download] couldn't start the background download service:", error)
  }
}

if (isTauri) {
  installPlaybackGuard()
  syncDownloadLimit()
}
