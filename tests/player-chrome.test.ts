/**
 * @vitest-environment jsdom
 */
// The player's own controls: centre play/pause, ten-second skips on films, channel down / up
// on Live TV, the speed button, and the title shown in fullscreen.
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("@/scripts/lib/i18n.js", () => ({ t: (key: string) => key }))

import {
  attachPlayerChrome,
  isSeekable,
  nextSpeed,
  setChannelStepper,
  setPlayerTitle,
  skipTarget,
  speedLabel,
} from "../src/scripts/lib/player-chrome"

describe("skipping", () => {
  it("moves ten seconds either way inside a film", () => {
    expect(skipTarget(50, 120, 10)).toBe(60)
    expect(skipTarget(50, 120, -10)).toBe(40)
  })

  it("stops at the start and at the end", () => {
    expect(skipTarget(4, 120, -10)).toBe(0)
    expect(skipTarget(115, 120, 10)).toBe(120)
  })

  it("does nothing on a live channel, which has no length", () => {
    expect(skipTarget(50, Number.POSITIVE_INFINITY, 10)).toBeNull()
    expect(skipTarget(50, Number.NaN, 10)).toBeNull()
    expect(isSeekable(Number.POSITIVE_INFINITY)).toBe(false)
    expect(isSeekable(0)).toBe(false)
    expect(isSeekable(75)).toBe(true)
  })
})

describe("speed button", () => {
  it("steps through the speeds and comes back to normal", () => {
    const seen = [1]
    for (let step = 0; step < 5; step += 1) seen.push(nextSpeed(seen[seen.length - 1]))
    expect(seen).toEqual([1, 1.25, 1.5, 2, 0.75, 1])
  })

  it("returns to normal from a speed it does not list", () => {
    expect(nextSpeed(3)).toBe(1)
  })

  it("is labelled the way people say it", () => {
    expect(speedLabel(1)).toBe("1×")
    expect(speedLabel(1.25)).toBe("1.25×")
  })
})

class FakeVideo {
  currentTime = 0
  duration = Number.NaN
  paused = true
  playbackRate = 1
  private root: HTMLElement
  constructor(root: HTMLElement) {
    this.root = root
  }
  closest(): HTMLElement {
    return this.root
  }
}

function mountFakePlayer(kind: "art" | "vjs") {
  document.body.innerHTML =
    kind === "art"
      ? '<div class="art-video-player"><div class="art-controls-right"></div></div>'
      : '<div class="video-js"><div class="vjs-control-bar"><button class="vjs-fullscreen-control"></button></div></div>'
  const root = document.body.firstElementChild as HTMLElement
  const video = new FakeVideo(root)
  const listeners = new Map<string, Array<() => void>>()
  const handle = {
    on(event: string, fn: () => void) {
      listeners.set(event, [...(listeners.get(event) || []), fn])
    },
    play: vi.fn(() => {
      video.paused = false
      fire("play")
    }),
    pause: vi.fn(() => {
      video.paused = true
      fire("pause")
    }),
    currentTime: vi.fn((value?: number) => {
      if (value !== undefined) video.currentTime = value
      return video.currentTime
    }),
    getMediaElement: () => video as unknown as HTMLVideoElement,
  }
  function fire(event: string) {
    for (const fn of listeners.get(event) || []) fn()
  }
  attachPlayerChrome(handle)
  const click = (selector: string, index = 0) =>
    (root.querySelectorAll<HTMLElement>(selector)[index] as HTMLElement).dispatchEvent(new MouseEvent("click", { bubbles: true }))
  return { root, video, handle, fire, click }
}

describe("controls on the player", () => {
  beforeEach(() => {
    setChannelStepper(null)
    setPlayerTitle("")
  })

  it("plays and pauses from the centre button and follows the player's state", () => {
    const player = mountFakePlayer("art")
    expect(player.root.hasAttribute("data-kd-paused")).toBe(true)

    player.click(".kd-toggle")
    expect(player.handle.play).toHaveBeenCalledTimes(1)
    expect(player.root.hasAttribute("data-kd-paused")).toBe(false)
    expect(player.root.querySelector(".kd-toggle")?.getAttribute("aria-label")).toBe("player.chrome.pause")

    player.click(".kd-toggle")
    expect(player.handle.pause).toHaveBeenCalledTimes(1)
    expect(player.root.querySelector(".kd-toggle")?.getAttribute("aria-label")).toBe("player.chrome.play")
  })

  it("skips ten seconds on a film", () => {
    const player = mountFakePlayer("art")
    player.video.duration = 120
    player.video.currentTime = 50
    player.fire("loadedmetadata")
    expect(player.root.hasAttribute("data-kd-seekable")).toBe(true)

    player.click(".kd-skip", 1)
    expect(player.video.currentTime).toBe(60)
    player.click(".kd-skip", 0)
    player.click(".kd-skip", 0)
    expect(player.video.currentTime).toBe(40)
  })

  it("offers channel down / up on a live channel when the page can step channels", () => {
    const steps: number[] = []
    const player = mountFakePlayer("art")
    player.video.duration = Number.POSITIVE_INFINITY
    player.fire("loadedmetadata")
    expect(player.root.hasAttribute("data-kd-channels")).toBe(false)

    setChannelStepper((delta) => steps.push(delta))
    expect(player.root.hasAttribute("data-kd-channels")).toBe(true)
    expect(player.root.hasAttribute("data-kd-seekable")).toBe(false)
    player.click(".kd-channel", 1)
    player.click(".kd-channel", 0)
    expect(steps).toEqual([1, -1])
  })

  it("keeps the skips, not the channel buttons, for a catch-up programme on Live TV", () => {
    setChannelStepper(() => {})
    const player = mountFakePlayer("art")
    player.video.duration = 3600
    player.fire("loadedmetadata")
    expect(player.root.hasAttribute("data-kd-seekable")).toBe(true)
    expect(player.root.hasAttribute("data-kd-channels")).toBe(false)
  })

  it("changes speed from the speed button and shows it", () => {
    const player = mountFakePlayer("art")
    const speed = player.root.querySelector(".kd-speed") as HTMLElement
    player.fire("loadedmetadata")
    expect(speed.textContent).toBe("1×")

    player.click(".kd-speed")
    player.fire("ratechange")
    expect(player.video.playbackRate).toBe(1.25)
    expect(speed.textContent).toBe("1.25×")
    expect(speed.hasAttribute("data-kd-changed")).toBe(true)
  })

  it("adds the same speed button to the Video.js bar, before fullscreen", () => {
    const player = mountFakePlayer("vjs")
    const bar = player.root.querySelector(".vjs-control-bar") as HTMLElement
    expect(bar.children[0].classList.contains("kd-speed")).toBe(true)
    expect(bar.children[1].classList.contains("vjs-fullscreen-control")).toBe(true)
  })

  it("shows what is playing", () => {
    const player = mountFakePlayer("art")
    setPlayerTitle("Some Series", "S1E2 · The One")
    expect(player.root.hasAttribute("data-kd-titled")).toBe(true)
    expect(player.root.querySelector(".kd-title")?.textContent).toBe("Some Series")
    expect(player.root.querySelector(".kd-title-detail")?.textContent).toBe("S1E2 · The One")
  })

  it("adds its controls once, however often the player reloads", () => {
    const player = mountFakePlayer("art")
    player.fire("loadstart")
    player.fire("loadedmetadata")
    expect(player.root.querySelectorAll(".kd-transport")).toHaveLength(1)
    expect(player.root.querySelectorAll(".kd-speed")).toHaveLength(1)
  })
})
