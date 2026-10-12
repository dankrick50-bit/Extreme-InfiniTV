// Updates for this fork. The app asks the fork's own release whether a newer build exists and
// installs from there; the original app's updater would replace this build with the original.
//
// The version number does not change between fork builds, so builds are told apart by the time
// of the commit they were made from (see build-tools/build-info.mjs).
import { log } from "@/scripts/lib/log.js"
import { providerFetch } from "@/scripts/lib/provider-fetch.js"
import { getAutoUpdateEnabled } from "@/scripts/lib/app-settings.js"
import { sandboxRuntimeSync } from "@/scripts/lib/sandbox.ts"
import { toast } from "@/scripts/lib/toast.js"

export interface ForkBuildInfo {
  /** Commit time in Unix seconds; 0 when the build could not tell which commit it is. */
  stamp: number
  commit: string
  /** "owner/name" of the fork on GitHub. */
  repo: string
  /** The rolling release tag its builds are published under. */
  tag: string
}

/** Written in by the build (astro.config.mjs); absent in unit tests and in the original app. */
declare const __KD_BUILD__: ForkBuildInfo | undefined

// This screen text exists only in this fork, so it lives here rather than in the shared translation files.
const TEXT = {
  ready: "A new version of the app is ready",
  update: "Update",
  openAbout: "Open Settings > About to get it.",
  downloading: "Downloading the update…",
  tapInstall: "Tap Install when your device asks.",
  failed: "The update could not be downloaded",
}

const SESSION_FLAG = "kd_update_checked"
const NOTIFIED_KEY = "kd_update_notified"
const FEED_TIMEOUT_MS = 15000

export function forkBuild(): ForkBuildInfo | null {
  try {
    if (typeof __KD_BUILD__ === "undefined" || !__KD_BUILD__) return null
    const info = __KD_BUILD__
    if (typeof info.repo !== "string" || !/^[\w.-]+\/[\w.-]+$/.test(info.repo)) return null
    if (typeof info.tag !== "string" || !/^[\w.-]+$/.test(info.tag)) return null
    return { stamp: Number(info.stamp) || 0, commit: String(info.commit || ""), repo: info.repo, tag: info.tag }
  } catch {
    return null
  }
}

/** True in every build made from this fork: its updates must never come from the original app's releases. */
export function forkOwnsUpdates(): boolean {
  return forkBuild() !== null
}

export function forkReleaseUrls(info: ForkBuildInfo): { download: string; feed: string; page: string } {
  const download = `https://github.com/${info.repo}/releases/download/${info.tag}`
  return { download, feed: `${download}/latest.json`, page: `https://github.com/${info.repo}/releases/tag/${info.tag}` }
}

/**
 * The feed address, carrying this build's own time: the native updater has no other way to learn
 * what is installed, and reads it from here to decide whether the published build is newer.
 */
export function forkFeedUrl(info: ForkBuildInfo | null = forkBuild()): string | null {
  return info ? `${forkReleaseUrls(info).feed}?build=${info.stamp}` : null
}

export function forkReleasePage(info: ForkBuildInfo | null = forkBuild()): string | null {
  return info ? forkReleaseUrls(info).page : null
}

export interface ForkFeed {
  version: string
  build: number
  commit: string
  publishedAt: string | null
  notes: string | null
  page: string
  /** Installer per platform ("windows", "android", "macos", "appimage", "deb"). */
  assets: Record<string, string>
}

/** Reads latest.json. Links that point anywhere but this fork's own release are dropped. */
export function parseForkFeed(value: unknown, info: ForkBuildInfo): ForkFeed | null {
  if (!value || typeof value !== "object") return null
  const feed = value as Record<string, unknown>
  const extra = feed.kd as Record<string, unknown> | undefined
  if (!extra || typeof extra !== "object") return null
  const build = Number(extra.build)
  if (!Number.isFinite(build) || build <= 0) return null

  const urls = forkReleaseUrls(info)
  const ownDownloads = `https://github.com/${info.repo}/releases/download/`
  const assets: Record<string, string> = {}
  if (extra.assets && typeof extra.assets === "object") {
    for (const [name, url] of Object.entries(extra.assets as Record<string, unknown>)) {
      if (typeof url === "string" && url.startsWith(ownDownloads)) assets[name] = url
    }
  }
  const page = typeof extra.page === "string" && extra.page.startsWith(`https://github.com/${info.repo}/`) ? extra.page : urls.page
  return {
    version: typeof feed.version === "string" ? feed.version : "",
    build,
    commit: typeof extra.commit === "string" ? extra.commit : "",
    publishedAt: typeof feed.pub_date === "string" ? feed.pub_date : null,
    notes: typeof feed.notes === "string" && feed.notes ? feed.notes : null,
    page,
    assets,
  }
}

export type ForkPlatform = "windows" | "android" | "macos" | "linux" | "other"

export function detectPlatform(userAgent: string = typeof navigator !== "undefined" ? navigator.userAgent || "" : ""): ForkPlatform {
  // Android's WebView also says "Linux", so it has to be asked first.
  if (/Android/i.test(userAgent)) return "android"
  if (/Windows/i.test(userAgent)) return "windows"
  if (/Mac OS X|Macintosh/i.test(userAgent)) return "macos"
  if (/Linux/i.test(userAgent)) return "linux"
  return "other"
}

/** The installer to hand this device, or the release page where there is a choice to make (Linux). */
export function downloadFor(feed: ForkFeed, platform: ForkPlatform): string {
  const asset = platform === "windows" || platform === "android" || platform === "macos" ? feed.assets[platform] : undefined
  return asset || feed.page
}

/** "12 Oct 2026, 07:45" in the device's own language and time zone. */
export function describeBuild(stamp: number, locale?: string): string {
  if (!Number.isFinite(stamp) || stamp <= 0) return ""
  return new Date(stamp * 1000).toLocaleString(locale, { day: "numeric", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" })
}

/** Same shape as update-check.ts's UpdateStatus, so the About screen can show it unchanged. */
export interface ForkUpdateStatus {
  current: string
  latest: string
  latestTag: string
  latestUrl: string
  publishedAt?: string
  notes?: string
  updateAvailable: boolean
}

export function isNewerBuild(feed: ForkFeed, info: ForkBuildInfo): boolean {
  // A build that does not know what it is cannot say what is newer than it.
  return info.stamp > 0 && feed.build > info.stamp
}

export function statusFromFeed(feed: ForkFeed, info: ForkBuildInfo, currentVersion: string, platform: ForkPlatform): ForkUpdateStatus {
  const label = [feed.version, describeBuild(feed.build)].filter(Boolean).join(" · ")
  return {
    current: currentVersion,
    latest: label,
    latestTag: label,
    latestUrl: downloadFor(feed, platform),
    publishedAt: feed.publishedAt ?? undefined,
    notes: feed.notes ?? undefined,
    updateAvailable: isNewerBuild(feed, info),
  }
}

export async function fetchForkFeed(info: ForkBuildInfo): Promise<ForkFeed | null> {
  const url = forkFeedUrl(info)
  if (!url) return null
  const response = await providerFetch(url, { signal: AbortSignal.timeout(FEED_TIMEOUT_MS), logKind: "other" })
  // The release is deleted and re-created on every build, so "not found" for a moment is normal.
  if (!response.ok) return null
  return parseForkFeed(await response.json(), info)
}

async function currentAppVersion(): Promise<string> {
  try {
    const { getVersion } = await import("@tauri-apps/api/app")
    return await getVersion()
  } catch {
    return document.querySelector('meta[name="x-app-version"]')?.getAttribute("content") || ""
  }
}

/** What the About screen and the startup notice show; null when the fork could not be reached. */
export async function forkUpdateStatus(): Promise<ForkUpdateStatus | null> {
  const info = forkBuild()
  if (!info) return null
  try {
    const feed = await fetchForkFeed(info)
    if (!feed) return null
    return statusFromFeed(feed, info, await currentAppVersion(), detectPlatform())
  } catch (error) {
    log.warn("[kd:update] check failed:", error)
    return null
  }
}

// ---------------------------------------------------------------------------
// Android and Fire OS: download in the app, then hand the file to the system installer
// ---------------------------------------------------------------------------

interface AndroidUpdateBridge {
  downloadAndInstall?: (url: string) => boolean
}

function androidUpdateBridge(): AndroidUpdateBridge | undefined {
  return typeof window !== "undefined" ? ((window as any).AndroidUpdate as AndroidUpdateBridge | undefined) : undefined
}

let androidEventsWired = false

function wireAndroidEvents(): void {
  if (androidEventsWired || typeof window === "undefined") return
  androidEventsWired = true
  window.addEventListener("kd:update", (event) => {
    const detail = (event as CustomEvent).detail as { state?: string; detail?: string } | undefined
    if (detail?.state === "ready") {
      toast({ title: TEXT.tapInstall, duration: 8000 })
    } else if (detail?.state === "error") {
      log.warn("[kd:update] android install failed:", detail.detail)
      toast({ title: TEXT.failed, description: detail.detail || undefined, variant: "error", duration: 8000 })
    }
  })
}

/**
 * Starts the in-app download and install of an APK. False when this device cannot do that, in
 * which case the caller falls back to opening the link.
 */
export function installForkUpdateInApp(url: string): boolean {
  const bridge = androidUpdateBridge()
  if (!bridge?.downloadAndInstall || !/\.apk(\?|$)/i.test(url)) return false
  wireAndroidEvents()
  let started = false
  try {
    started = !!bridge.downloadAndInstall(url)
  } catch (error) {
    log.warn("[kd:update] android bridge threw:", error)
  }
  if (started) toast({ title: TEXT.downloading, duration: 6000 })
  return started
}

// ---------------------------------------------------------------------------
// At startup
// ---------------------------------------------------------------------------

function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && (!!(window as any).__TAURI_INTERNALS__ || !!(window as any).__TAURI__)
}

function firstCheckThisSession(): boolean {
  try {
    if (sessionStorage.getItem(SESSION_FLAG)) return false
    sessionStorage.setItem(SESSION_FLAG, "1")
  } catch {}
  return true
}

/** Windows and Linux can replace themselves; true when an update was installed (the app restarts). */
async function installDesktopUpdate(info: ForkBuildInfo): Promise<boolean> {
  const { invoke } = await import("@tauri-apps/api/core")
  const update = await invoke("updater_check_from", { url: forkFeedUrl(info) })
  if (update === null) return false
  await invoke("updater_install")
  // Windows exits mid-install and the installer restarts the app; this matters on Linux AppImage.
  const { relaunch } = await import("@tauri-apps/plugin-process")
  await relaunch()
  return true
}

function notifyOnce(feed: ForkFeed, platform: ForkPlatform): void {
  try {
    if (localStorage.getItem(NOTIFIED_KEY) === String(feed.build)) return
    localStorage.setItem(NOTIFIED_KEY, String(feed.build))
  } catch {}
  const url = downloadFor(feed, platform)
  if (platform === "android" && androidUpdateBridge()?.downloadAndInstall && feed.assets.android) {
    toast({
      title: TEXT.ready,
      description: describeBuild(feed.build),
      // Stays until answered: it is shown once per build and is the one-tap way in.
      duration: 0,
      action: {
        label: TEXT.update,
        onClick: () => {
          if (!installForkUpdateInApp(url)) {
            void import("@/scripts/lib/external-link.ts").then(({ openExternal }) => openExternal(url))
          }
        },
      },
    })
    return
  }
  toast({ title: TEXT.ready, description: TEXT.openAbout, duration: 8000 })
}

/**
 * Once per app session: Windows and Linux install a newer build and restart; Android and the rest
 * are told once per build. Does nothing in a build that cannot say which commit it is.
 */
export async function runForkStartupUpdate(): Promise<void> {
  const info = forkBuild()
  if (!info || info.stamp <= 0) return
  // A dev server run is not an installed app; replacing it would install over the real one.
  if (import.meta.env?.MODE === "development") return
  if (!firstCheckThisSession()) return

  let feed: ForkFeed | null = null
  try {
    feed = await fetchForkFeed(info)
  } catch (error) {
    log.warn("[kd:update] startup check failed:", error)
  }
  if (!feed || !isNewerBuild(feed, info)) return

  const platform = detectPlatform()
  const selfInstalls = isTauriRuntime() && (platform === "windows" || platform === "linux") && !sandboxRuntimeSync()
  if (selfInstalls && getAutoUpdateEnabled()) {
    try {
      if (await installDesktopUpdate(info)) return
    } catch (error) {
      // A .deb install, or the download failing: fall back to telling the user.
      log.warn("[kd:update] automatic install failed:", error)
    }
  }
  notifyOnce(feed, platform)
}
