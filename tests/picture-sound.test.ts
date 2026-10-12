/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest"

import {
  audioEnhanceWanted,
  ENHANCE_EVENT,
  getEnhanceSettings,
  setEnhanceSettings,
  type EnhanceSettings,
} from "@/scripts/lib/enhance-settings"
import { connectEnhanceChain, enhanceParams } from "@/scripts/lib/audio-enhance"
import { applySharpen, sharpenFilterId, sharpenKernel } from "@/scripts/lib/video-sharpen"
import { describeAudio, describeVideo, mediaBadges } from "@/scripts/lib/media-badges"
import { soundOptionsApply } from "@/scripts/lib/enhance-dialog"

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

const OFF: EnhanceSettings = { sharpen: "off", levelling: "off", dialogue: false }

beforeEach(() => {
  vi.stubGlobal("localStorage", new MemoryStorage())
})

afterEach(() => {
  vi.unstubAllGlobals()
  document.body.innerHTML = ""
})

describe("picture and sound settings", () => {
  it("are all off until switched on, so the player is unchanged by default", () => {
    expect(getEnhanceSettings()).toEqual(OFF)
    expect(audioEnhanceWanted()).toBe(false)
  })

  it("remember a change and announce it", () => {
    const heard = vi.fn()
    document.addEventListener(ENHANCE_EVENT, heard)
    setEnhanceSettings({ sharpen: "light", levelling: "night" })
    document.removeEventListener(ENHANCE_EVENT, heard)

    expect(getEnhanceSettings()).toEqual({ sharpen: "light", levelling: "night", dialogue: false })
    expect(heard).toHaveBeenCalledTimes(1)
    expect(audioEnhanceWanted()).toBe(true)
  })

  it("fall back to off for a stored value that is not a known choice", () => {
    localStorage.setItem("kd_enhance_sharpen", "ultra")
    localStorage.setItem("kd_enhance_levelling", "loud")
    expect(getEnhanceSettings()).toEqual(OFF)
  })

  it("need the sound effects only when a sound option is on", () => {
    expect(audioEnhanceWanted({ ...OFF, sharpen: "strong" })).toBe(false)
    expect(audioEnhanceWanted({ ...OFF, dialogue: true })).toBe(true)
    expect(audioEnhanceWanted({ ...OFF, levelling: "on" })).toBe(true)
  })
})

describe("enhanceParams", () => {
  it("leaves the sound untouched with everything off", () => {
    expect(enhanceParams(OFF)).toEqual({ compressor: null, speechDb: 0, rumbleDb: 0, output: 1 })
  })

  it("squeezes harder in night mode than in normal levelling", () => {
    const normal = enhanceParams({ ...OFF, levelling: "on" }).compressor!
    const night = enhanceParams({ ...OFF, levelling: "night" }).compressor!
    expect(night.ratio).toBeGreaterThan(normal.ratio)
    expect(night.threshold).toBeLessThan(normal.threshold)
  })

  it("lifts speech, eases rumble, and leaves headroom when nothing else tames the peaks", () => {
    const alone = enhanceParams({ ...OFF, dialogue: true })
    expect(alone.speechDb).toBeGreaterThan(0)
    expect(alone.rumbleDb).toBeLessThan(0)
    expect(alone.output).toBeLessThan(1)
    // With levelling on, the compressor catches the lifted peaks itself.
    expect(enhanceParams({ ...OFF, dialogue: true, levelling: "on" }).output).toBe(1)
  })
})

describe("connectEnhanceChain", () => {
  function fakeNode(name: string, log: string[]) {
    const param = () => ({ value: 0 })
    return {
      name,
      type: "",
      threshold: param(),
      knee: param(),
      ratio: param(),
      attack: param(),
      release: param(),
      frequency: param(),
      Q: param(),
      gain: param(),
      connect(target: { name: string }) {
        log.push(`${name}>${target.name}`)
      },
      disconnect() {
        log.push(`${name}x`)
      },
    }
  }

  function setup() {
    const log: string[] = []
    const filters = ["speech", "rumble"]
    const ctx = {
      createDynamicsCompressor: () => fakeNode("compressor", log),
      createBiquadFilter: () => fakeNode(filters.shift()!, log),
      createGain: () => fakeNode("trim", log),
    } as unknown as AudioContext
    const source = fakeNode("source", log)
    const output = fakeNode("out", log)
    const chain = connectEnhanceChain(ctx, source as unknown as AudioNode, output as unknown as AudioNode)
    return { chain, log }
  }

  // What the source currently feeds, read off the order of connect/disconnect calls.
  const sourceFeeds = (log: string[]) => log.slice(log.lastIndexOf("sourcex") + 1).filter((entry) => entry.startsWith("source>"))

  it("passes straight through when levelling is off, so surround keeps all its channels", () => {
    const { log } = setup()
    expect(sourceFeeds(log)).toEqual(["source>speech"])
    expect(log).toContain("trim>out")
  })

  it("routes through the compressor only while levelling is on", () => {
    const { chain, log } = setup()
    chain.update({ ...OFF, levelling: "on" })
    expect(sourceFeeds(log)).toEqual(["source>compressor"])
    expect(log).toContain("compressor>speech")

    chain.update(OFF)
    expect(sourceFeeds(log)).toEqual(["source>speech"])
  })

  it("does not rewire when only a tone setting changes", () => {
    const { chain, log } = setup()
    const before = log.length
    chain.update({ ...OFF, dialogue: true })
    expect(log.length).toBe(before)
  })
})

describe("sharpening", () => {
  it("uses a kernel that keeps overall brightness the same at any strength", () => {
    for (const strength of [0.22, 0.5, 1]) {
      const sum = sharpenKernel(strength).reduce((total, value) => total + value, 0)
      expect(sum).toBeCloseTo(1, 10)
    }
    expect(sharpenKernel(0)).toEqual([0, -0, 0, -0, 1, -0, 0, -0, 0])
  })

  it("applies and removes its filter without touching anyone else's", () => {
    const video = document.createElement("video")
    document.body.appendChild(video)

    applySharpen(video, "light")
    expect(video.style.filter).toContain(sharpenFilterId("light")!)
    expect(document.querySelectorAll("#kd-sharpen-filters filter")).toHaveLength(2)

    applySharpen(video, "strong")
    expect(video.style.filter).toContain(sharpenFilterId("strong")!)
    // Filters are defined once, however often they are applied.
    expect(document.querySelectorAll("#kd-sharpen-filters")).toHaveLength(1)

    applySharpen(video, "off")
    expect(video.style.filter).toBe("")

    video.style.filter = "brightness(1.1)"
    applySharpen(video, "off")
    expect(video.style.filter).toBe("brightness(1.1)")
  })
})

describe("what a film carries", () => {
  it("names Atmos when the provider's details mention it", () => {
    expect(describeAudio({ codec_name: "eac3", profile: "Dolby Digital Plus + Dolby Atmos", channels: 6, channel_layout: "5.1(side)" })).toBe("Dolby Atmos")
    expect(describeAudio({ codec_name: "truehd", profile: "Dolby TrueHD + Dolby Atmos", channels: 8 })).toBe("Dolby Atmos")
  })

  it("names the format and the channels otherwise", () => {
    expect(describeAudio({ codec_name: "eac3", channels: 6, channel_layout: "5.1(side)" })).toBe("Dolby Digital+ 5.1")
    expect(describeAudio({ codec_name: "ac3", channels: 6 })).toBe("Dolby Digital 5.1")
    expect(describeAudio({ codec_name: "aac", channels: 2, channel_layout: "stereo" })).toBe("AAC Stereo")
    expect(describeAudio({ codec_name: "dts", profile: "DTS-HD MA", channels: 8, channel_layout: "7.1" })).toBe("DTS-HD MA 7.1")
    expect(describeAudio({ codec_name: "mp3", channels: 1 })).toBe("MP3 Mono")
  })

  it("names the picture by size, format and HDR", () => {
    expect(describeVideo({ codec_name: "hevc", width: 3840, height: 2160, color_transfer: "smpte2084" })).toBe("4K HEVC HDR")
    expect(describeVideo({ codec_name: "h264", width: 1920, height: 1080 })).toBe("1080p H.264")
    // A widescreen 4K film is cropped to well under 2160 lines.
    expect(describeVideo({ codec_name: "hevc", width: 3840, height: 1600 })).toBe("4K HEVC")
    expect(describeVideo({ codec_name: "h264", width: 1280, height: 536 })).toBe("720p H.264")
  })

  it("says nothing when the provider says nothing", () => {
    expect(describeAudio(null)).toBeNull()
    expect(describeAudio([])).toBeNull()
    expect(describeAudio({})).toBeNull()
    expect(describeVideo({})).toBeNull()
    expect(mediaBadges(null)).toEqual([])
    expect(mediaBadges({ video: [], audio: [] })).toEqual([])
  })

  it("lists picture before sound", () => {
    expect(mediaBadges({ video: { codec_name: "h264", width: 1920, height: 1080 }, audio: { codec_name: "ac3", channels: 6 } })).toEqual([
      "1080p H.264",
      "Dolby Digital 5.1",
    ])
  })
})

describe("soundOptionsApply", () => {
  function videoPlaying(src: string): HTMLVideoElement {
    const video = document.createElement("video")
    Object.defineProperty(video, "currentSrc", { value: src })
    return video
  }

  it("is true for live channels, which play through a buffer the page owns", () => {
    expect(soundOptionsApply(videoPlaying("blob:http://tauri.localhost/1234"))).toBe(true)
  })

  it("is false for a film played straight from the provider's file", () => {
    expect(soundOptionsApply(videoPlaying("http://provider.example/movie/u/p/9.mp4"))).toBe(false)
    expect(soundOptionsApply(videoPlaying("http://127.0.0.1:5123/token/stream.mkv"))).toBe(false)
  })

  it("does not warn when nothing is loaded yet", () => {
    expect(soundOptionsApply(null)).toBe(true)
    expect(soundOptionsApply(videoPlaying(""))).toBe(true)
  })
})
