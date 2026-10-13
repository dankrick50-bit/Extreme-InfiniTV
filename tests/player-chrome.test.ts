/**
 * @vitest-environment jsdom
 */
// The player's own controls: centre play/pause, ten-second skips on films, channel down / up
// on Live TV, the speed button, and the title shown in fullscreen.
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("@/scripts/lib/i18n.js", () => ({ t: (key: string) => key }))

import {
  attachPlayerChrome,
  chooseSubtitle,
  isSeekable,
  nextSpeed,
  setChannelStepper,
  setPlayerTitle,
  skipTarget,
  speedLabel,
  subtitleChoices,
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

  it("adds its subtitle and speed buttons to the Video.js bar, before fullscreen", () => {
    const player = mountFakePlayer("vjs")
    const bar = player.root.querySelector(".vjs-control-bar") as HTMLElement
    expect(Array.from(bar.children).map((child) => child.className.split(" ").pop())).toEqual([
      "kd-subs",
      "kd-speed",
      "vjs-fullscreen-control",
    ])
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

function fakeTracks(...tracks: Array<{ kind: string; label: string; language: string; mode: string }>) {
  return Object.assign([...tracks], { addEventListener() {}, removeEventListener() {} })
}

describe("subtitle menu", () => {
  it("offers off first, then each subtitle track by name", () => {
    const tracks = fakeTracks(
      { kind: "subtitles", label: "English", language: "en", mode: "showing" },
      { kind: "metadata", label: "", language: "", mode: "hidden" },
      { kind: "captions", label: "", language: "es", mode: "disabled" },
    )
    expect(subtitleChoices(tracks, "Off")).toEqual([
      { index: -1, label: "Off", active: false },
      { index: 0, label: "English", active: true },
      { index: 1, label: "es", active: false },
    ])
  })

  it("offers nothing when the video has no subtitles", () => {
    expect(subtitleChoices(fakeTracks({ kind: "metadata", label: "", language: "", mode: "hidden" }), "Off")).toEqual([])
    expect(subtitleChoices(null, "Off")).toEqual([])
  })

  it("shows the chosen track and switches the rest off", () => {
    const tracks = fakeTracks(
      { kind: "subtitles", label: "English", language: "en", mode: "showing" },
      { kind: "subtitles", label: "Español", language: "es", mode: "disabled" },
    )
    chooseSubtitle(tracks, 1)
    expect(tracks.map((track) => track.mode)).toEqual(["disabled", "showing"])
    chooseSubtitle(tracks, -1)
    expect(tracks.map((track) => track.mode)).toEqual(["disabled", "disabled"])
  })

  it("appears on the Video.js bar when the video has subtitles the player does not list", () => {
    const player = mountFakePlayer("vjs")
    const tracks = fakeTracks(
      { kind: "subtitles", label: "English", language: "en", mode: "disabled" },
      { kind: "subtitles", label: "Español", language: "es", mode: "disabled" },
    )
    ;(player.video as unknown as { textTracks: unknown }).textTracks = tracks
    player.fire("loadedmetadata")
    expect(player.root.hasAttribute("data-kd-subtitles")).toBe(true)

    player.click(".kd-subs")
    const items = Array.from(player.root.querySelectorAll<HTMLElement>(".kd-menu-item"))
    expect(items.map((item) => item.textContent)).toEqual(["player.subtitles.off", "English", "Español"])

    items[2].dispatchEvent(new MouseEvent("click", { bubbles: true }))
    expect(tracks.map((track) => track.mode)).toEqual(["disabled", "showing"])
    expect(player.root.querySelector(".kd-menu")).toBeNull()
    expect(player.root.querySelector(".kd-subs")?.hasAttribute("data-kd-changed")).toBe(true)
  })

  it("stays away when Video.js shows its own subtitle button", () => {
    const player = mountFakePlayer("vjs")
    const own = document.createElement("button")
    own.className = "vjs-subs-caps-button"
    player.root.querySelector(".vjs-control-bar")?.appendChild(own)
    ;(player.video as unknown as { textTracks: unknown }).textTracks = fakeTracks({
      kind: "subtitles",
      label: "English",
      language: "en",
      mode: "disabled",
    })
    player.fire("loadedmetadata")
    expect(player.root.hasAttribute("data-kd-subtitles")).toBe(false)
  })

  it("is not added to ArtPlayer, which has its own for every stream", () => {
    const player = mountFakePlayer("art")
    expect(player.root.querySelector(".kd-subs")).toBeNull()
  })
})
