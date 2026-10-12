/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest"

const noteCastStreaming = vi.fn()
vi.mock("@/scripts/lib/download-native.js", () => ({ noteCastStreaming: (streaming: boolean) => noteCastStreaming(streaming) }))

type FeedListener = (state: { state: string; positionSeconds: number }) => void
interface FeedSubscription {
  listener: FeedListener
  onLost?: () => void
  unsubscribed: boolean
}
const subscriptions: FeedSubscription[] = []
vi.mock("@/scripts/lib/tv-cast-state-feed", () => ({
  subscribeCastStateFeed: (listener: FeedListener, opts: { onLost?: () => void }) => {
    const subscription: FeedSubscription = { listener, onLost: opts.onLost, unsubscribed: false }
    subscriptions.push(subscription)
    return () => {
      subscription.unsubscribed = true
    }
  },
}))

import { setCastSession, updateCastSession, clearCastSession, type CastSession } from "@/scripts/lib/tv-cast"
import { castStateHoldsConnection, initCastDownloadGuard, sessionLikelyStreaming } from "@/scripts/lib/cast-download-guard"

// Same in-memory Storage stub as tests/tv-cast-devices.test.ts.
class MemoryStorage {
  private store = new Map<string, string>()
  get length(): number {
    return this.store.size
  }
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
  key(index: number): string | null {
    return Array.from(this.store.keys())[index] ?? null
  }
}

const memoryLocalStorage = new MemoryStorage()
const memorySessionStorage = new MemoryStorage()

function filmSession(overrides: Partial<CastSession> = {}): CastSession {
  return {
    deviceId: "ext-dlna-0a1b2c",
    deviceName: '75" QLED',
    host: "127.0.0.1",
    port: 47830,
    key: "b".repeat(32),
    title: "A Film",
    isLive: false,
    startedAt: 1,
    ...overrides,
  }
}

const lastNote = () => noteCastStreaming.mock.calls.at(-1)?.[0]
const liveSubscription = () => subscriptions.filter((subscription) => !subscription.unsubscribed).at(-1)

beforeEach(() => {
  vi.stubGlobal("Storage", MemoryStorage)
  vi.stubGlobal("localStorage", memoryLocalStorage as unknown as Storage)
  vi.stubGlobal("sessionStorage", memorySessionStorage as unknown as Storage)
  memoryLocalStorage.clear()
  memorySessionStorage.clear()
  // The guard listens for the page's lifetime; start every test from "nothing is being cast".
  initCastDownloadGuard()
  clearCastSession()
  noteCastStreaming.mockClear()
  subscriptions.length = 0
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe("castStateHoldsConnection", () => {
  it("counts every state in which the TV has the stream open", () => {
    for (const state of ["loading", "buffering", "playing", "paused"]) expect(castStateHoldsConnection(state)).toBe(true)
  })

  it("releases once the TV is done with it", () => {
    for (const state of ["idle", "ended", "error", ""]) expect(castStateHoldsConnection(state)).toBe(false)
  })
})

describe("sessionLikelyStreaming", () => {
  it("assumes a session started to play something is playing", () => {
    expect(sessionLikelyStreaming(filmSession())).toBe(true)
  })

  it("does not for a TV that is only connected, a dismissed session, or none", () => {
    expect(sessionLikelyStreaming(filmSession({ connectedOnly: true, title: "" }))).toBe(false)
    expect(sessionLikelyStreaming(filmSession({ dismissed: true }))).toBe(false)
    expect(sessionLikelyStreaming(null)).toBe(false)
  })
})

describe("cast download guard", () => {
  it("parks downloads as soon as a cast starts and follows the TV from there", () => {
    setCastSession(filmSession())
    expect(lastNote()).toBe(true)
    expect(subscriptions).toHaveLength(1)

    liveSubscription()!.listener({ state: "paused", positionSeconds: 30 })
    expect(lastNote()).toBe(true)
    liveSubscription()!.listener({ state: "ended", positionSeconds: 5400 })
    expect(lastNote()).toBe(false)
    // The next episode starting on the same session parks them again.
    liveSubscription()!.listener({ state: "loading", positionSeconds: 0 })
    expect(lastNote()).toBe(true)
  })

  it("does not subscribe twice while the same cast is running", () => {
    setCastSession(filmSession())
    updateCastSession({ title: "Another Film" })
    updateCastSession({ pinnedHostIndex: 0 })
    expect(subscriptions).toHaveLength(1)
  })

  it("releases downloads and stops watching when the cast ends", () => {
    setCastSession(filmSession())
    clearCastSession()
    expect(lastNote()).toBe(false)
    expect(subscriptions[0].unsubscribed).toBe(true)
  })

  it("leaves downloads alone while a TV is connected with nothing playing", () => {
    setCastSession(filmSession({ connectedOnly: true, title: "" }))
    expect(lastNote()).toBe(false)
    liveSubscription()!.listener({ state: "idle", positionSeconds: 0 })
    expect(lastNote()).toBe(false)
    // Picking something to play on that connection parks them.
    liveSubscription()!.listener({ state: "playing", positionSeconds: 1 })
    expect(lastNote()).toBe(true)
  })

  it("releases downloads when the TV stops answering, and watches again for the next cast", () => {
    setCastSession(filmSession())
    liveSubscription()!.onLost?.()
    expect(lastNote()).toBe(false)

    clearCastSession()
    setCastSession(filmSession({ startedAt: 2 }))
    expect(subscriptions).toHaveLength(2)
    expect(lastNote()).toBe(true)
  })
})
