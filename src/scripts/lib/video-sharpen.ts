// Sharpening for the built-in player: a small convolution applied to the video as it is drawn,
// after it has been scaled to the screen. It crisps edges; it cannot add detail that was never
// in the stream.

import { ENHANCE_EVENT, getEnhanceSettings, type SharpenLevel } from "@/scripts/lib/enhance-settings"
import { log } from "@/scripts/lib/log.js"

const FILTER_HOST_ID = "kd-sharpen-filters"

/** How much of each neighbouring pixel is taken away; the centre is raised to keep brightness equal. */
const STRENGTH: Record<Exclude<SharpenLevel, "off">, number> = { light: 0.22, strong: 0.5 }

/** Pure: the 3x3 kernel for a strength. Its entries always sum to 1, so overall brightness is unchanged. */
export function sharpenKernel(strength: number): number[] {
  const edge = -strength
  const centre = 1 + 4 * strength
  return [0, edge, 0, edge, centre, edge, 0, edge, 0]
}

export function sharpenFilterId(level: SharpenLevel): string | null {
  return level === "off" ? null : `kd-sharpen-${level}`
}

function ensureFilters(): void {
  if (typeof document === "undefined" || document.getElementById(FILTER_HOST_ID)) return
  const filters = (Object.keys(STRENGTH) as Array<Exclude<SharpenLevel, "off">>)
    .map(
      (level) =>
        `<filter id="kd-sharpen-${level}" color-interpolation-filters="sRGB">` +
        `<feConvolveMatrix order="3" preserveAlpha="true" kernelMatrix="${sharpenKernel(STRENGTH[level]).join(" ")}"/>` +
        `</filter>`
    )
    .join("")
  const host = document.createElement("div")
  host.id = FILTER_HOST_ID
  host.setAttribute("aria-hidden", "true")
  host.style.cssText = "position:absolute;width:0;height:0;overflow:hidden;pointer-events:none"
  host.innerHTML = `<svg xmlns="http://www.w3.org/2000/svg" width="0" height="0"><defs>${filters}</defs></svg>`
  document.body.appendChild(host)
}

export function applySharpen(videoEl: HTMLVideoElement | null, level: SharpenLevel = getEnhanceSettings().sharpen): void {
  if (!videoEl) return
  const id = sharpenFilterId(level)
  if (!id) {
    // Only clear what this module set, never a filter something else put there.
    if (videoEl.dataset.kdSharpen) {
      videoEl.style.filter = ""
      delete videoEl.dataset.kdSharpen
    }
    return
  }
  ensureFilters()
  videoEl.style.filter = `url(#${id})`
  videoEl.dataset.kdSharpen = level
}

interface PictureHandle {
  getMediaElement?(): HTMLVideoElement | null
  on(event: string, fn: (...args: unknown[]) => void): void
}

let currentHandle: PictureHandle | null = null
let listening = false

function applyToCurrent(): void {
  try {
    applySharpen(currentHandle?.getMediaElement?.() ?? null)
  } catch (err) {
    log.warn("[kd:sharpen] apply failed", err)
  }
}

/** Keeps a mounted player's picture in step with the setting; call once per mounted player. */
export function bindPictureEnhance(handle: PictureHandle): void {
  currentHandle = handle
  try {
    handle.on("loadedmetadata", applyToCurrent)
  } catch (err) {
    log.warn("[kd:sharpen] failed to bind", err)
  }
  applyToCurrent()
  if (listening || typeof document === "undefined") return
  listening = true
  document.addEventListener(ENHANCE_EVENT, applyToCurrent)
}
