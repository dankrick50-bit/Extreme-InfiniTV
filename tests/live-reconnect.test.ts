// A live channel has no end: when its connection closes or fails after it was playing, the
// player opens it again. A few tries in a row at most, earned back by a connection that holds.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest"
import { createReconnectBudget } from "../src/scripts/lib/live-reconnect"
import type { CastDescriptorV1 } from "../src/scripts/lib/tv-cast-descriptor"
import type {
  EmbeddedEngineDom,
  ReceiverEngine,
  ReceiverStatePartial,
} from "../src/scripts/receiver/engines"

describe("reconnect allowance", () => {
  it("allows a few reconnects in a row and then stops", () => {
    const budget = createReconnectBudget({ max: 3, now: () => 0 })
    expect([budget.take(), budget.take(), budget.take(), budget.take()]).toEqual([true, true, true, false])
  })

  it("is earned back by a connection that holds", () => {
    let clock = 0
    const budget = createReconnectBudget({ max: 2, refillAfterMs: 30_000, now: () => clock })
    budget.take()
    budget.take()
    budget.notePlaying()
    clock += 30_000
    expect(budget.take()).toBe(true)
  })

  it("is not earned back by a connection that drops again quickly", () => {
    let clock = 0
    const budget = createReconnectBudget({ max: 2, refillAfterMs: 30_000, now: () => clock })
    budget.take()
    budget.notePlaying()
    clock += 5_000
    budget.take()
    budget.notePlaying()
    clock += 5_000
    expect(budget.take()).toBe(false)
  })

  it("measures from when playback started, not from the latest playing event", () => {
    let clock = 0
    const budget = createReconnectBudget({ max: 1, refillAfterMs: 30_000, now: () => clock })
    budget.take()
    budget.notePlaying()
    clock += 20_000
    budget.notePlaying()
    clock += 10_000
    expect(budget.take()).toBe(true)
  })

  it("starts full again on a new tune", () => {
    const budget = createReconnectBudget({ max: 1, now: () => 0 })
    budget.take()
    budget.reset()
    expect(budget.take()).toBe(true)
  })
})

class FakeMediaElement {
  volume = 1
  muted = false
  currentTime = 0
  duration = Number.NaN
  paused = false
  ended = false
  videoWidth = 640
  videoHeight = 360
  error: unknown = null
  addEventListener(): void {}
}

class FakeHandle {
  private handlers = new Map<string, Array<() => void>>()
  mediaEl = new FakeMediaElement()
  sources: string[] = []
  on(type: string, handler: () => void): void {
    const forType = this.handlers.get(type) || []
    forType.push(handler)
    this.handlers.set(type, forType)
  }
  fire(type: string): void {
    for (const handler of this.handlers.get(type) || []) handler()
  }
  src(source: { src: string }): void {
    this.sources.push(source.src)
  }
  pause(): void {}
  reset(): void {}
  duration(): number {
    return 0
  }
  currentTime(): void {}
  muted(): void {}
  getMediaElement(): FakeMediaElement {
    return this.mediaEl
  }
  codecInfo(): { videoCodec: null; audioCodec: null; errorDetail: null } {
    return { videoCodec: null, audioCodec: null, errorDetail: null }
  }
}

let currentHandle = new FakeHandle()
let playCalls = 0

vi.mock("@/scripts/lib/player-runtime", () => ({
  mountPlayer: async () => ({ kind: "embedded", handle: currentHandle }),
  playWhenReady: () => {
    playCalls += 1
  },
}))

vi.mock("@/scripts/lib/i18n.js", () => ({ t: (key: string) => key }))

vi.mock("@/scripts/lib/manifest-probe.js", () => ({
  probeManifestSource: async () => "inconclusive",
  messageKeyForProbeVerdict: () => null,
}))

function fakeElement(): HTMLElement {
  return {
    textContent: "",
    offsetWidth: 0,
    classList: { add: () => {}, remove: () => {}, toggle: () => {}, contains: () => false },
    focus: () => {},
  } as unknown as HTMLElement
}

function embeddedDom(): EmbeddedEngineDom {
  return {
    idleEl: fakeElement(),
    playerViewEl: fakeElement(),
    videoEl: currentHandle.mediaEl as unknown as HTMLVideoElement,
    titleWrapEl: fakeElement(),
    titleEl: fakeElement(),
    loadingEl: fakeElement(),
    loadingTitleEl: fakeElement(),
    pausedEl: fakeElement(),
    errorEl: fakeElement(),
    errorMessageEl: fakeElement(),
    errorCountdownEl: fakeElement(),
    errorRetryEl: fakeElement(),
  }
}

let createEmbeddedReceiverEngine: (
  dom: EmbeddedEngineDom,
  callbacks: { report(partial: ReceiverStatePartial): void; onSessionEnded(): void },
) => ReceiverEngine

beforeAll(async () => {
  ;(globalThis as { document?: unknown }).document = { addEventListener: () => {} }
  Object.defineProperty(globalThis, "navigator", {
    value: { userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36" },
    configurable: true,
  })
  ;(globalThis as { window?: unknown }).window = {}
  ;(globalThis as { MediaError?: unknown }).MediaError = {
    MEDIA_ERR_SRC_NOT_SUPPORTED: 4,
    MEDIA_ERR_DECODE: 3,
    MEDIA_ERR_NETWORK: 2,
  }
  ;({ createEmbeddedReceiverEngine } = await import("../src/scripts/receiver/engines"))
})

const LIVE_SRC = "http://tv.example/live/user/pass/9.ts"
const MOVIE_SRC = "http://tv.example/movie/user/pass/9.mp4"

function descriptor(isLive: boolean): CastDescriptorV1 {
  return {
    v: 1,
    src: isLive ? LIVE_SRC : MOVIE_SRC,
    mime: isLive ? "video/mp2t" : "video/mp4",
    isLive,
    title: isLive ? "Some Channel" : "Some Movie",
  } as CastDescriptorV1
}

describe("TV player on a live channel that loses its connection", () => {
  let reports: ReceiverStatePartial[]
  let sessionsEnded: number
  let engine: ReceiverEngine

  const states = () => reports.map((report) => report.state)

  // The element pauses and flags itself ended when its data runs out.
  function connectionCloses(): void {
    currentHandle.mediaEl.ended = true
    currentHandle.fire("pause")
    currentHandle.fire("ended")
  }

  function playbackResumes(): void {
    currentHandle.mediaEl.ended = false
    currentHandle.fire("playing")
  }

  beforeEach(() => {
    vi.useFakeTimers()
    currentHandle = new FakeHandle()
    playCalls = 0
    reports = []
    sessionsEnded = 0
    engine = createEmbeddedReceiverEngine(embeddedDom(), {
      report: (partial) => reports.push(partial),
      onSessionEnded: () => {
        sessionsEnded += 1
      },
    })
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it("opens the channel again when the provider closes the connection", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    reports = []
    connectionCloses()
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toEqual([LIVE_SRC, LIVE_SRC])
    // Started once for the tune and once more for the reconnect.
    expect(playCalls).toBe(2)
    expect(states()).toContain("buffering")
    expect(states()).not.toContain("paused")
    expect(states()).not.toContain("ended")
    expect(sessionsEnded).toBe(0)
  })

  it("opens the channel again when the connection breaks with an error", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    currentHandle.fire("error")
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toEqual([LIVE_SRC, LIVE_SRC])
    expect(states()).not.toContain("error")
  })

  it("opens the channel again when it goes silent mid-play", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    currentHandle.fire("waiting")
    await vi.advanceTimersByTimeAsync(32000)

    expect(currentHandle.sources).toEqual([LIVE_SRC, LIVE_SRC])
    expect(states()).not.toContain("error")
  })

  it("keeps recovering for as long as each connection holds a while", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    for (let drop = 0; drop < 8; drop += 1) {
      await vi.advanceTimersByTimeAsync(60000)
      connectionCloses()
      await vi.advanceTimersByTimeAsync(1100)
      playbackResumes()
    }

    expect(currentHandle.sources).toHaveLength(9)
    expect(sessionsEnded).toBe(0)
  })

  it("stops after a few tries when the channel will not stay up", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    for (let drop = 0; drop < 3; drop += 1) {
      connectionCloses()
      await vi.advanceTimersByTimeAsync(1100)
      playbackResumes()
    }
    connectionCloses()
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toHaveLength(4)
    expect(states()).toContain("ended")
    expect(sessionsEnded).toBe(1)
  })

  it("does not retry a channel that never started", async () => {
    await engine.play(descriptor(true))
    currentHandle.fire("error")
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toEqual([LIVE_SRC])
    expect(states()).toContain("error")
  })

  it("leaves a film that reaches its end alone", async () => {
    await engine.play(descriptor(false))
    playbackResumes()
    connectionCloses()
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toEqual([MOVIE_SRC])
    expect(states()).toContain("ended")
    expect(sessionsEnded).toBe(1)
  })

  it("does not reconnect a channel the viewer has left", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    connectionCloses()
    engine.teardown()
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toEqual([LIVE_SRC])
  })

  it("gives a newly chosen channel the full allowance", async () => {
    await engine.play(descriptor(true))
    playbackResumes()
    for (let drop = 0; drop < 3; drop += 1) {
      connectionCloses()
      await vi.advanceTimersByTimeAsync(1100)
      playbackResumes()
    }
    await engine.play(descriptor(true))
    playbackResumes()
    connectionCloses()
    await vi.advanceTimersByTimeAsync(1100)

    expect(currentHandle.sources).toHaveLength(6)
    expect(sessionsEnded).toBe(0)
  })
})

describe("hosts the WebView cannot read a stream from", () => {
  let needsRustLoader: (url: string) => boolean
  let rememberRustLoader: (url: string) => void

  beforeAll(async () => {
    const stored = new Map<string, string>()
    ;(globalThis as { sessionStorage?: unknown }).sessionStorage = {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => void stored.set(key, value),
    }
    ;({ needsRustLoader, rememberRustLoader } = await import("../src/scripts/lib/stream-loader-memory"))
  })

  it("are remembered by host, so the next channel there skips the doomed first connection", () => {
    expect(needsRustLoader("http://tv.example:8080/live/user/pass/1.ts")).toBe(false)
    rememberRustLoader("http://tv.example:8080/live/user/pass/1.ts")
    expect(needsRustLoader("http://tv.example:8080/live/user/pass/2.ts")).toBe(true)
  })

  it("leave other hosts alone", () => {
    expect(needsRustLoader("http://other.example/live/user/pass/1.ts")).toBe(false)
    expect(needsRustLoader("http://tv.example:9090/live/user/pass/1.ts")).toBe(false)
  })

  it("ignore an address that is not a web address", () => {
    rememberRustLoader("not a url")
    expect(needsRustLoader("not a url")).toBe(false)
  })
})
