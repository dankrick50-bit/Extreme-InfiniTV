// The look and the extra controls of the built-in player on the main screens: a centre
// play/pause with ten-second skips (channel down / up on Live TV), a speed button, labelled
// subtitle and audio buttons, and the title of what is playing when the player fills the screen.
//
// It sits on top of whichever player the page mounted (ArtPlayer on desktop, Video.js on
// Android) and only drives it through the shared handle and the <video> element, so the
// players' own menus, shortcuts and fullscreen keep working as before. The styling is in
// player-chrome.css; everything there is scoped to the "kd-player" class added here.

import "@/styles/player-chrome.css"
import { t } from "@/scripts/lib/i18n.js"
import { log } from "@/scripts/lib/log.js"

export const SKIP_SECONDS = 10
export const SPEEDS = [1, 1.25, 1.5, 2, 0.75]
// Room for the text labels beside the subtitle and audio icons.
const WIDE_PLAYER_PX = 720
const LARGE_PLAYER_PX = 1280

/** A film, episode or catch-up programme has a length; a live channel does not. */
export function isSeekable(duration: number): boolean {
  return Number.isFinite(duration) && duration > 0
}

/** Where a skip lands, or null when there is nothing to skip through. */
export function skipTarget(currentTime: number, duration: number, deltaSeconds: number): number | null {
  if (!isSeekable(duration) || !Number.isFinite(currentTime)) return null
  return Math.min(Math.max(currentTime + deltaSeconds, 0), duration)
}

/** The speed after the given one in the cycle; an unlisted speed goes back to normal. */
export function nextSpeed(current: number): number {
  const index = SPEEDS.findIndex((speed) => Math.abs(speed - current) < 0.01)
  if (index < 0) return SPEEDS[0]
  return SPEEDS[(index + 1) % SPEEDS.length]
}

export function speedLabel(rate: number): string {
  const rounded = Math.round(rate * 100) / 100
  return `${rounded}×`
}

// What the page tells the player: one player per page, so these are page-wide.
let channelStepper: ((delta: number) => void) | null = null
let playingTitle = ""
let playingDetail = ""
const refreshers = new Set<() => void>()

/** Live TV hands over channel stepping; the centre buttons then change channel on a live picture. */
export function setChannelStepper(step: ((delta: number) => void) | null): void {
  channelStepper = step
  for (const refresh of refreshers) refresh()
}

/** What is playing, shown along the top of the player in fullscreen. */
export function setPlayerTitle(title: string, detail = ""): void {
  playingTitle = title
  playingDetail = detail
  for (const refresh of refreshers) refresh()
}

interface ChromeHandle {
  on(event: string, fn: () => void): void
  play(): Promise<unknown> | void
  pause(): void
  currentTime?(value?: number): number
  el?(): HTMLElement | undefined
  getMediaElement?(): HTMLVideoElement | null
}

const ICON_ATTRS = 'viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"'
const TEN = '<text x="12" y="15.6" text-anchor="middle" font-size="7.4" font-weight="700" fill="currentColor" stroke="none">10</text>'
const ICONS = {
  previous: `<svg ${ICON_ATTRS}><path d="M18.5 5.6v12.8L8.8 12z" fill="currentColor"/><path d="M5.5 5.5v13"/></svg>`,
  next: `<svg ${ICON_ATTRS}><path d="M5.5 5.6v12.8l9.7-6.4z" fill="currentColor"/><path d="M18.5 5.5v13"/></svg>`,
  back: `<svg ${ICON_ATTRS}><path d="M3.2 12a8.8 8.8 0 1 0 2.9-6.5"/><path d="M3.4 3.6v4.6H8"/>${TEN}</svg>`,
  forward: `<svg ${ICON_ATTRS}><path d="M20.8 12a8.8 8.8 0 1 1-2.9-6.5"/><path d="M20.6 3.6v4.6H16"/>${TEN}</svg>`,
  play: '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M8.2 5.3a1 1 0 0 1 1.5-.86l11 6.7a1 1 0 0 1 0 1.72l-11 6.7a1 1 0 0 1-1.5-.86z"/></svg>',
  pause: '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><rect x="6" y="4.5" width="4.4" height="15" rx="1.3"/><rect x="13.6" y="4.5" width="4.4" height="15" rx="1.3"/></svg>',
}

function button(className: string, label: string, html: string): HTMLButtonElement {
  const element = document.createElement("button")
  element.type = "button"
  element.className = className
  element.setAttribute("aria-label", label)
  element.title = label
  element.innerHTML = html
  // The player underneath treats a click as play/pause and a double click as fullscreen.
  for (const type of ["click", "dblclick"]) {
    element.addEventListener(type, (event) => event.stopPropagation())
  }
  return element
}

/** Adds the controls to the player behind `handle`; safe to call before its <video> exists. */
export function attachPlayerChrome(handle: ChromeHandle): void {
  if (typeof document === "undefined") return
  let root: HTMLElement | null = null
  let toggle: HTMLButtonElement | null = null
  let speed: HTMLElement | null = null
  let titleText: HTMLElement | null = null
  let titleDetail: HTMLElement | null = null

  const media = () => handle.getMediaElement?.() ?? null

  function seekBy(deltaSeconds: number): void {
    const video = media()
    if (!video) return
    const target = skipTarget(video.currentTime, video.duration, deltaSeconds)
    if (target === null) return
    try {
      if (handle.currentTime) handle.currentTime(target)
      else video.currentTime = target
    } catch (err) {
      log.warn("[kd:player] skip failed", err)
    }
  }

  function togglePlay(): void {
    const video = media()
    if (!video) return
    try {
      if (video.paused) void Promise.resolve(handle.play()).catch(() => {})
      else handle.pause()
    } catch (err) {
      log.warn("[kd:player] play/pause failed", err)
    }
  }

  function sync(): void {
    const video = media()
    if (!root || !video) return
    const paused = video.paused
    const seekable = isSeekable(video.duration)
    root.toggleAttribute("data-kd-paused", paused)
    root.toggleAttribute("data-kd-seekable", seekable)
    root.toggleAttribute("data-kd-channels", !seekable && channelStepper !== null)
    root.toggleAttribute("data-kd-titled", playingTitle !== "")
    if (titleText) titleText.textContent = playingTitle
    if (titleDetail) titleDetail.textContent = playingDetail
    if (toggle) {
      const label = paused ? t("player.chrome.play") : t("player.chrome.pause")
      toggle.innerHTML = paused ? ICONS.play : ICONS.pause
      toggle.setAttribute("aria-label", label)
      toggle.title = label
    }
    if (speed) {
      speed.textContent = speedLabel(video.playbackRate || 1)
      speed.toggleAttribute("data-kd-changed", Math.abs((video.playbackRate || 1) - 1) > 0.01)
    }
  }

  function build(found: HTMLElement): void {
    root = found
    root.classList.add("kd-player")
    root.style.setProperty("--kd-label-subtitles", JSON.stringify(t("player.subtitles")))
    root.style.setProperty("--kd-label-audio", JSON.stringify(t("player.audio")))

    const transport = document.createElement("div")
    transport.className = "kd-transport"
    const back = button("kd-transport-button kd-skip", t("player.chrome.back"), ICONS.back)
    toggle = button("kd-transport-button kd-toggle", t("player.chrome.play"), ICONS.play)
    const forward = button("kd-transport-button kd-skip", t("player.chrome.forward"), ICONS.forward)
    const previous = button("kd-transport-button kd-channel", t("keyboardHelp.desc.previousChannel"), ICONS.previous)
    const next = button("kd-transport-button kd-channel", t("keyboardHelp.desc.nextChannel"), ICONS.next)
    back.addEventListener("click", () => {
      pulse(back)
      seekBy(-SKIP_SECONDS)
    })
    forward.addEventListener("click", () => {
      pulse(forward)
      seekBy(SKIP_SECONDS)
    })
    previous.addEventListener("click", () => channelStepper?.(-1))
    next.addEventListener("click", () => channelStepper?.(1))
    toggle.addEventListener("click", togglePlay)
    transport.append(previous, back, toggle, forward, next)
    root.appendChild(transport)

    const topbar = document.createElement("div")
    topbar.className = "kd-topbar"
    titleText = document.createElement("div")
    titleText.className = "kd-title"
    titleDetail = document.createElement("div")
    titleDetail.className = "kd-title-detail"
    topbar.append(titleText, titleDetail)
    root.appendChild(topbar)

    // ArtPlayer keeps speed in its right-click menu only, and Video.js hides its speed button
    // for streams it did not load itself, so both get the same one here.
    const artControls = root.querySelector(".art-controls-right")
    const vjsBar = root.querySelector(".vjs-control-bar")
    if (artControls || vjsBar) {
      const pill = button(`${artControls ? "art-control" : "vjs-control vjs-button"} kd-speed`, t("player.chrome.speed"), "")
      pill.addEventListener("click", () => {
        const current = media()
        if (current) current.playbackRate = nextSpeed(current.playbackRate || 1)
      })
      if (artControls) artControls.prepend(pill)
      else vjsBar?.insertBefore(pill, vjsBar.querySelector(".vjs-picture-in-picture-control, .vjs-fullscreen-control"))
      speed = pill
    }

    if (typeof ResizeObserver !== "undefined") {
      const observer = new ResizeObserver((entries) => {
        const width = entries[0]?.contentRect.width ?? 0
        found.toggleAttribute("data-kd-wide", width >= WIDE_PLAYER_PX)
        found.toggleAttribute("data-kd-large", width >= LARGE_PLAYER_PX)
      })
      observer.observe(found)
    }
  }

  // A short ring so a skip is felt even when the picture barely changes.
  function pulse(element: HTMLElement): void {
    element.classList.remove("kd-pulse")
    void element.offsetWidth
    element.classList.add("kd-pulse")
  }

  function ensure(): void {
    if (root?.isConnected) return
    const video = media()
    if (!video) return
    const found = video.closest<HTMLElement>(".art-video-player, .video-js")
    if (!found || found.querySelector(".kd-transport")) return
    build(found)
  }

  const refresh = () => {
    ensure()
    sync()
  }
  refreshers.add(refresh)
  // Through the handle, not the element: Video.js can swap its <video> when the source changes.
  try {
    for (const type of ["loadstart", "loadedmetadata", "durationchange", "emptied", "play", "playing", "pause", "ratechange", "ended"]) {
      handle.on(type, refresh)
    }
  } catch (err) {
    log.warn("[kd:player] failed to bind", err)
  }
  refresh()
}
