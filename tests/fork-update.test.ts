/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest"
import { readFileSync } from "node:fs"

const providerFetch = vi.fn()
vi.mock("@/scripts/lib/provider-fetch.js", () => ({ providerFetch: (...args: unknown[]) => providerFetch(...args) }))
const toast = vi.fn()
vi.mock("@/scripts/lib/toast.js", () => ({ toast: (options: unknown) => toast(options) }))
const invoke = vi.fn()
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }))
const relaunch = vi.fn()
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: () => relaunch() }))
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.9.0" }))

import {
  describeBuild,
  detectPlatform,
  downloadFor,
  forkBuild,
  forkFeedUrl,
  forkOwnsUpdates,
  forkReleasePage,
  forkUpdateStatus,
  installForkUpdateInApp,
  isNewerBuild,
  parseForkFeed,
  runForkStartupUpdate,
  statusFromFeed,
  type ForkBuildInfo,
} from "@/scripts/lib/fork-update"
import { makeUpdateFeed } from "../build-tools/make-update-feed.mjs"
import { buildInfo, releaseUrls } from "../build-tools/build-info.mjs"

const INSTALLED: ForkBuildInfo = { stamp: 1000, commit: "aaaaaaa", repo: "someone/their-app", tag: "fork-latest" }
const DOWNLOAD = "https://github.com/someone/their-app/releases/download/fork-latest"

const WINDOWS_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
const ANDROID_UA = "Mozilla/5.0 (Linux; Android 11; KFTRWI) AppleWebKit/537.36"
const LINUX_UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"
const MAC_UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15"

function publishedFeed(build: number, overrides: Record<string, unknown> = {}) {
  return {
    version: "1.9.0",
    notes: "Build bbbbbbb",
    pub_date: new Date(build * 1000).toISOString(),
    platforms: { "windows-x86_64": { signature: "sig", url: `${DOWNLOAD}/App-windows-x64-setup.exe` } },
    kd: {
      build,
      commit: "bbbbbbb",
      page: "https://github.com/someone/their-app/releases/tag/fork-latest",
      assets: {
        windows: `${DOWNLOAD}/App-windows-x64-setup.exe`,
        android: `${DOWNLOAD}/App-android-fireos.apk`,
        macos: `${DOWNLOAD}/App-macos-universal.dmg`,
        deb: `${DOWNLOAD}/App-ubuntu-amd64.deb`,
      },
    },
    ...overrides,
  }
}

function feedResponse(body: unknown, ok = true) {
  return { ok, status: ok ? 200 : 404, json: async () => body }
}

class MemoryStorage {
  private store = new Map<string, string>()
  getItem(key: string): string | null {
    return this.store.has(key) ? this.store.get(key)! : null
  }
  setItem(key: string, value: string): void {
    this.store.set(key, String(value))
  }
  removeItem(key: string): void {
    this.store.delete(key)
  }
  clear(): void {
    this.store.clear()
  }
}

function useDevice(userAgent: string, extras: Record<string, unknown> = {}): void {
  vi.stubGlobal("navigator", { userAgent })
  for (const [name, value] of Object.entries(extras)) (window as any)[name] = value
}

beforeEach(() => {
  vi.stubGlobal("__KD_BUILD__", { ...INSTALLED })
  vi.stubGlobal("localStorage", new MemoryStorage())
  vi.stubGlobal("sessionStorage", new MemoryStorage())
  ;(window as any).__TAURI_INTERNALS__ = {}
  providerFetch.mockReset()
  toast.mockReset()
  invoke.mockReset()
  relaunch.mockReset()
})

afterEach(() => {
  delete (window as any).__TAURI_INTERNALS__
  delete (window as any).AndroidUpdate
  vi.unstubAllGlobals()
})

describe("which build this is", () => {
  it("reads what the build wrote in", () => {
    expect(forkBuild()).toEqual(INSTALLED)
    expect(forkOwnsUpdates()).toBe(true)
  })

  it("is not a fork build when nothing was written in (the original app, unit tests)", () => {
    vi.stubGlobal("__KD_BUILD__", undefined)
    expect(forkBuild()).toBeNull()
    expect(forkOwnsUpdates()).toBe(false)
    expect(forkFeedUrl()).toBeNull()
    expect(forkReleasePage()).toBeNull()
  })

  it("refuses a repository or tag that could redirect the feed somewhere else", () => {
    vi.stubGlobal("__KD_BUILD__", { ...INSTALLED, repo: "someone/their-app/../../evil" })
    expect(forkBuild()).toBeNull()
    vi.stubGlobal("__KD_BUILD__", { ...INSTALLED, tag: "x/../y" })
    expect(forkBuild()).toBeNull()
  })

  it("puts the installed build's time on the feed address for the native updater", () => {
    expect(forkFeedUrl()).toBe(`${DOWNLOAD}/latest.json?build=1000`)
    expect(forkReleasePage()).toBe("https://github.com/someone/their-app/releases/tag/fork-latest")
  })
})

describe("parseForkFeed", () => {
  it("reads the build, its date and the installers", () => {
    const feed = parseForkFeed(publishedFeed(2000), INSTALLED)!
    expect(feed.build).toBe(2000)
    expect(feed.commit).toBe("bbbbbbb")
    expect(feed.version).toBe("1.9.0")
    expect(feed.assets.android).toBe(`${DOWNLOAD}/App-android-fireos.apk`)
    expect(feed.page).toBe("https://github.com/someone/their-app/releases/tag/fork-latest")
  })

  it("drops installer links that point outside the fork's own release", () => {
    const raw = publishedFeed(2000)
    raw.kd.assets.android = "https://evil.example/App.apk"
    raw.kd.assets.windows = "https://github.com/someone-else/their-app/releases/download/fork-latest/App.exe"
    raw.kd.page = "https://evil.example/page"
    const feed = parseForkFeed(raw, INSTALLED)!
    expect(feed.assets.android).toBeUndefined()
    expect(feed.assets.windows).toBeUndefined()
    expect(feed.assets.macos).toBe(`${DOWNLOAD}/App-macos-universal.dmg`)
    expect(feed.page).toBe("https://github.com/someone/their-app/releases/tag/fork-latest")
  })

  it("rejects anything that does not say which build it is", () => {
    expect(parseForkFeed(null, INSTALLED)).toBeNull()
    expect(parseForkFeed("latest", INSTALLED)).toBeNull()
    expect(parseForkFeed({ version: "1.9.0" }, INSTALLED)).toBeNull()
    expect(parseForkFeed(publishedFeed(0), INSTALLED)).toBeNull()
    expect(parseForkFeed({ version: "1.9.0", kd: { build: "soon" } }, INSTALLED)).toBeNull()
  })
})

describe("what counts as newer", () => {
  const feedAt = (build: number) => parseForkFeed(publishedFeed(build), INSTALLED)!

  it("is only a build made from a later commit", () => {
    expect(isNewerBuild(feedAt(1001), INSTALLED)).toBe(true)
    expect(isNewerBuild(feedAt(1000), INSTALLED)).toBe(false)
    expect(isNewerBuild(feedAt(999), INSTALLED)).toBe(false)
  })

  it("is never anything for a build that does not know what it is", () => {
    expect(isNewerBuild(feedAt(5000), { ...INSTALLED, stamp: 0 })).toBe(false)
  })
})

describe("what each device is offered", () => {
  const feed = () => parseForkFeed(publishedFeed(2000), INSTALLED)!

  it("tells the platforms apart, Android before Linux", () => {
    expect(detectPlatform(ANDROID_UA)).toBe("android")
    expect(detectPlatform(WINDOWS_UA)).toBe("windows")
    expect(detectPlatform(MAC_UA)).toBe("macos")
    expect(detectPlatform(LINUX_UA)).toBe("linux")
    expect(detectPlatform("")).toBe("other")
  })

  it("hands each its own installer, and Linux the page since there are two kinds", () => {
    expect(downloadFor(feed(), "android")).toMatch(/\.apk$/)
    expect(downloadFor(feed(), "windows")).toMatch(/setup\.exe$/)
    expect(downloadFor(feed(), "macos")).toMatch(/\.dmg$/)
    expect(downloadFor(feed(), "linux")).toBe(feed().page)
    expect(downloadFor(feed(), "other")).toBe(feed().page)
  })

  it("describes a build by when it was made, since the version number does not change", () => {
    const status = statusFromFeed(feed(), INSTALLED, "1.9.0", "android")
    expect(status.updateAvailable).toBe(true)
    expect(status.current).toBe("1.9.0")
    expect(status.latest).toContain("1.9.0")
    expect(status.latest).toContain(describeBuild(2000))
    expect(status.latestUrl).toMatch(/\.apk$/)
    expect(describeBuild(0)).toBe("")
  })
})

describe("forkUpdateStatus", () => {
  it("asks the fork's own feed and reports a newer build", async () => {
    useDevice(ANDROID_UA)
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(2000)))
    const status = await forkUpdateStatus()
    expect(providerFetch.mock.calls[0][0]).toBe(`${DOWNLOAD}/latest.json?build=1000`)
    expect(status?.updateAvailable).toBe(true)
    expect(status?.latestUrl).toMatch(/\.apk$/)
  })

  it("says up to date when the published build is this one", async () => {
    useDevice(WINDOWS_UA)
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(1000)))
    expect((await forkUpdateStatus())?.updateAvailable).toBe(false)
  })

  it("is null while the release is being replaced or the network is down", async () => {
    useDevice(WINDOWS_UA)
    providerFetch.mockResolvedValueOnce(feedResponse({}, false))
    expect(await forkUpdateStatus()).toBeNull()
    providerFetch.mockRejectedValueOnce(new Error("offline"))
    expect(await forkUpdateStatus()).toBeNull()
  })
})

describe("installForkUpdateInApp", () => {
  it("hands an APK to the Android installer", () => {
    const downloadAndInstall = vi.fn(() => true)
    useDevice(ANDROID_UA, { AndroidUpdate: { downloadAndInstall } })
    expect(installForkUpdateInApp(`${DOWNLOAD}/App-android-fireos.apk`)).toBe(true)
    expect(downloadAndInstall).toHaveBeenCalledWith(`${DOWNLOAD}/App-android-fireos.apk`)
    expect(toast).toHaveBeenCalledTimes(1)
  })

  it("leaves everything else to the caller: no installer here, or not an APK", () => {
    useDevice(WINDOWS_UA)
    expect(installForkUpdateInApp(`${DOWNLOAD}/App-android-fireos.apk`)).toBe(false)
    const downloadAndInstall = vi.fn(() => true)
    useDevice(ANDROID_UA, { AndroidUpdate: { downloadAndInstall } })
    expect(installForkUpdateInApp("https://github.com/someone/their-app/releases/tag/fork-latest")).toBe(false)
    expect(downloadAndInstall).not.toHaveBeenCalled()
  })

  it("reports false when the device refuses the address", () => {
    useDevice(ANDROID_UA, { AndroidUpdate: { downloadAndInstall: () => false } })
    expect(installForkUpdateInApp(`${DOWNLOAD}/App-android-fireos.apk`)).toBe(false)
    expect(toast).not.toHaveBeenCalled()
  })
})

describe("runForkStartupUpdate", () => {
  it("installs a newer build on Windows and restarts", async () => {
    useDevice(WINDOWS_UA)
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(2000)))
    invoke.mockResolvedValueOnce({ version: "1.9.0" }).mockResolvedValueOnce(undefined)
    await runForkStartupUpdate()
    expect(invoke).toHaveBeenNthCalledWith(1, "updater_check_from", { url: `${DOWNLOAD}/latest.json?build=1000` })
    expect(invoke).toHaveBeenNthCalledWith(2, "updater_install")
    expect(relaunch).toHaveBeenCalledTimes(1)
    expect(toast).not.toHaveBeenCalled()
  })

  it("does nothing when the published build is the installed one", async () => {
    useDevice(WINDOWS_UA)
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(1000)))
    await runForkStartupUpdate()
    expect(invoke).not.toHaveBeenCalled()
    expect(toast).not.toHaveBeenCalled()
  })

  it("only tells the user when automatic updates are switched off", async () => {
    useDevice(WINDOWS_UA)
    localStorage.setItem("xt_auto_update", "0")
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(2000)))
    await runForkStartupUpdate()
    expect(invoke).not.toHaveBeenCalled()
    expect(toast).toHaveBeenCalledTimes(1)
  })

  it("falls back to telling the user when the install cannot run (a .deb install, a failed download)", async () => {
    useDevice(LINUX_UA)
    providerFetch.mockResolvedValueOnce(feedResponse(publishedFeed(2000)))
    invoke.mockRejectedValueOnce("OTHER:no matching target")
    await runForkStartupUpdate()
    expect(relaunch).not.toHaveBeenCalled()
    expect(toast).toHaveBeenCalledTimes(1)
  })

  it("offers Android a one-tap update, once per build", async () => {
    const downloadAndInstall = vi.fn(() => true)
    useDevice(ANDROID_UA, { AndroidUpdate: { downloadAndInstall } })
    providerFetch.mockResolvedValue(feedResponse(publishedFeed(2000)))
    await runForkStartupUpdate()

    expect(invoke).not.toHaveBeenCalled()
    const offer = toast.mock.calls[0][0] as { duration: number; action: { label: string; onClick: () => void } }
    expect(offer.duration).toBe(0)
    offer.action.onClick()
    expect(downloadAndInstall).toHaveBeenCalledWith(`${DOWNLOAD}/App-android-fireos.apk`)

    // A later app start with the same build published does not ask again.
    sessionStorage.clear()
    toast.mockClear()
    await runForkStartupUpdate()
    expect(toast).not.toHaveBeenCalled()
  })

  it("checks once per app session, not on every page", async () => {
    useDevice(WINDOWS_UA)
    providerFetch.mockResolvedValue(feedResponse(publishedFeed(1000)))
    await runForkStartupUpdate()
    await runForkStartupUpdate()
    expect(providerFetch).toHaveBeenCalledTimes(1)
  })

  it("never runs for a build that does not know what it is", async () => {
    vi.stubGlobal("__KD_BUILD__", { ...INSTALLED, stamp: 0 })
    useDevice(WINDOWS_UA)
    await runForkStartupUpdate()
    expect(providerFetch).not.toHaveBeenCalled()
  })
})

describe("the published feed (build-tools/make-update-feed.mjs)", () => {
  const urls = releaseUrls("someone/their-app", "fork-latest")
  const files = [
    "App-windows-x64-setup.exe",
    "App-linux-x86_64.AppImage",
    "App-ubuntu-amd64.deb",
    "App-macos-universal.dmg",
    "App-android-fireos.apk",
  ]

  it("lists every installer, and offers self-install only where there is a signature", () => {
    const feed = makeUpdateFeed({
      files,
      signatures: { "App-windows-x64-setup.exe": "win-sig", "App-linux-x86_64.AppImage": "appimage-sig" },
      version: "1.9.0",
      build: { stamp: 2000, commit: "bbbbbbb" },
      urls,
    })
    expect(feed.version).toBe("1.9.0")
    expect(feed.pub_date).toBe("1970-01-01T00:33:20Z")
    expect(Object.keys(feed.platforms).sort()).toEqual(["linux-x86_64-appimage", "windows-x86_64", "windows-x86_64-nsis"])
    expect((feed.platforms as Record<string, unknown>)["windows-x86_64-nsis"]).toEqual({ signature: "win-sig", url: `${DOWNLOAD}/App-windows-x64-setup.exe` })
    expect(Object.keys(feed.kd.assets).sort()).toEqual(["android", "appimage", "deb", "macos", "windows"])
    expect(feed.kd.build).toBe(2000)
  })

  it("is read back by the app as the same build", () => {
    const feed = makeUpdateFeed({ files, signatures: {}, version: "1.9.0", build: { stamp: 2000, commit: "bbbbbbb" }, urls })
    const parsed = parseForkFeed(JSON.parse(JSON.stringify(feed)), INSTALLED)!
    expect(parsed.build).toBe(2000)
    expect(parsed.assets.android).toBe(`${DOWNLOAD}/App-android-fireos.apk`)
    expect(new Date(parsed.publishedAt!).getTime()).toBe(2000 * 1000)
    expect(Object.keys(feed.platforms)).toEqual([])
  })
})

describe("the fork's identity is written in three places that must agree", () => {
  const brand = JSON.parse(readFileSync("build-tools/brand.json", "utf8"))
  const rust = readFileSync("src-tauri/src/fork_update.rs", "utf8")
  const strings = readFileSync("src-tauri/gen/android/app/src/main/res/values/strings.xml", "utf8")

  it("the desktop updater only trusts the fork's own release path and key", () => {
    expect(rust).toContain(`pub const FEED_PATH_PREFIX: &str = "/${brand.updateRepo}/releases/download/";`)
    expect(rust).toContain(`pub const PUBKEY: &str = "${brand.updaterPubkey}";`)
  })

  it("the Android installer only downloads from the fork's own release path", () => {
    expect(strings).toContain(`>https://github.com/${brand.updateRepo}/releases/download/</string>`)
  })

  it("a build made here knows its commit and where its updates live", () => {
    const info = buildInfo({})
    expect(info.repo).toBe(brand.updateRepo)
    expect(info.tag).toBe(brand.updateTag)
    expect(info.stamp).toBeGreaterThan(1_700_000_000)
    expect(buildInfo({ KD_BUILD_STAMP: "42", KD_BUILD_COMMIT: "abc1234" })).toMatchObject({ stamp: 42, commit: "abc1234" })
    expect(releaseUrls().feed).toBe(`https://github.com/${brand.updateRepo}/releases/download/${brand.updateTag}/latest.json`)
  })
})
