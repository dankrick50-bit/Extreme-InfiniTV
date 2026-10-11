// @vitest-environment jsdom
// The native download engine is told how many connections it may use. Getting that wrong either
// starves downloads or, on a one-connection account, makes the provider cut the stream being watched.
import { describe, it, expect, vi } from "vitest"

vi.mock("@/scripts/lib/app-settings.js", () => ({
  getUserAgent: () => "",
  getDownloadConcurrency: () => 2,
  getNetworkTimeoutSeconds: () => 15,
}))
vi.mock("@/scripts/lib/account-info.js", () => ({
  getMaxConnectionsSync: () => 1,
  getActivePlaylistIdSync: () => "playlist",
}))

import { allowedDownloads, isStreamOpen } from "../src/scripts/lib/download-native.js"
import { handleTrimLevel, onMemoryPressure } from "../src/scripts/lib/memory-pressure.js"

describe("allowedDownloads", () => {
  it("uses the configured number when the provider's limit is unknown", () => {
    expect(allowedDownloads({ configured: 3, providerCap: 0, streamOpen: false })).toBe(3)
    expect(allowedDownloads({ configured: 3, providerCap: 0, streamOpen: true })).toBe(3)
  })

  it("never exceeds the provider's connection limit", () => {
    expect(allowedDownloads({ configured: 4, providerCap: 2, streamOpen: false })).toBe(2)
  })

  it("parks every download while a stream is open on a one-connection account", () => {
    expect(allowedDownloads({ configured: 2, providerCap: 1, streamOpen: true })).toBe(0)
    expect(allowedDownloads({ configured: 2, providerCap: 1, streamOpen: false })).toBe(1)
  })

  it("leaves one connection for the stream on larger accounts", () => {
    expect(allowedDownloads({ configured: 3, providerCap: 3, streamOpen: true })).toBe(2)
  })

  it("falls back to one download for a missing or invalid setting", () => {
    expect(allowedDownloads({ configured: 0, providerCap: 0, streamOpen: false })).toBe(1)
    expect(allowedDownloads({ configured: undefined, providerCap: undefined, streamOpen: false })).toBe(1)
  })
})

describe("isStreamOpen", () => {
  const video = (overrides: Record<string, unknown>) =>
    ({ ended: false, currentSrc: "", src: "", networkState: 2, ...overrides }) as unknown as HTMLVideoElement

  it("counts a remote or MSE source as an open stream, playing or paused", () => {
    expect(isStreamOpen(video({ currentSrc: "http://provider.example/movie/u/p/1.mp4" }))).toBe(true)
    expect(isStreamOpen(video({ currentSrc: "blob:http://tauri.localhost/abc" }))).toBe(true)
  })

  it("ignores a saved file played from disk", () => {
    expect(isStreamOpen(video({ currentSrc: "http://asset.localhost/C%3A/Videos/a.mp4" }))).toBe(false)
    expect(isStreamOpen(video({ currentSrc: "asset://localhost/home/me/a.mp4" }))).toBe(false)
  })

  it("ignores a player with nothing loaded, or one that has finished", () => {
    expect(isStreamOpen(video({}))).toBe(false)
    expect(isStreamOpen(video({ currentSrc: "http://provider.example/a.mp4", networkState: 0 }))).toBe(false)
    expect(isStreamOpen(video({ currentSrc: "http://provider.example/a.mp4", networkState: 3 }))).toBe(false)
    expect(isStreamOpen(video({ currentSrc: "http://provider.example/a.mp4", ended: true }))).toBe(false)
    expect(isStreamOpen(null as unknown as HTMLVideoElement)).toBe(false)
  })
})

describe("memory pressure", () => {
  it("runs the registered handlers only from 'running low' upwards", () => {
    const handler = vi.fn()
    const remove = onMemoryPressure(handler)

    expect(handleTrimLevel(5)).toBe(false)
    expect(handler).not.toHaveBeenCalled()

    expect(handleTrimLevel(10)).toBe(true)
    expect(handleTrimLevel(20)).toBe(true)
    expect(handler).toHaveBeenCalledTimes(2)

    remove()
    handleTrimLevel(80)
    expect(handler).toHaveBeenCalledTimes(2)
  })

  it("keeps going when one handler throws", () => {
    const survivor = vi.fn()
    const removeBroken = onMemoryPressure(() => {
      throw new Error("boom")
    })
    const removeSurvivor = onMemoryPressure(survivor)

    expect(handleTrimLevel(15)).toBe(true)
    expect(survivor).toHaveBeenCalledTimes(1)
    removeBroken()
    removeSurvivor()
  })

  it("is what Android calls through the page's global hook", () => {
    expect((window as unknown as { __xtTrimMemory: unknown }).__xtTrimMemory).toBe(handleTrimLevel)
  })
})
