// "Picture & sound": sharpening, volume levelling and dialogue boost for the built-in player.
// Opened from the Display mode dialog; every change applies at once to whatever is playing.

import { attachDialogSpatialNav } from "@/scripts/lib/dialog-spatial-nav.js"
import { t } from "@/scripts/lib/i18n.js"
import { log } from "@/scripts/lib/log.js"
import { isSafeAudioSourceUrl } from "@/scripts/lib/audio-effects.ts"
import {
  getEnhanceSettings,
  setEnhanceSettings,
  LEVELLING_MODES,
  SHARPEN_LEVELS,
  type EnhanceSettings,
  type LevellingMode,
  type SharpenLevel,
} from "@/scripts/lib/enhance-settings"

const DIALOG_ID = "kd-enhance-dialog"

// This screen exists only in this fork, so its wording lives here rather than in the shared translation files.
const TEXT = {
  entry: "Picture & sound",
  title: "Picture & sound",
  picture: "Picture",
  sharpen: "Sharpen",
  sharpenHelp: "Crisper edges. It cannot add detail the stream does not have.",
  sound: "Sound",
  levelling: "Volume levelling",
  levellingHelp: "Evens out loud adverts and quiet speech. Night keeps everything close together for low volume. Plays in stereo.",
  dialogue: "Dialogue boost",
  dialogueHelp: "Lifts voices above music and effects.",
  soundUnavailable: "Sound options work on live channels. This video comes straight from the provider's file, so they cannot be applied to it.",
  upscaling: "AI upscaling (experimental)",
  upscalingToggle: "Use the NVIDIA graphics card",
  upscalingHelp:
    "Lets an NVIDIA RTX card upscale the video itself. Also switch on RTX Video Super Resolution in the NVIDIA app, and keep Sharpen off. Uses more battery.",
  upscalingRestart: "Restart the app to apply.",
  upscalingFailed: "The setting could not be saved.",
}

const SHARPEN_LABELS: Record<SharpenLevel, string> = { off: "Off", light: "Light", strong: "Strong" }
const LEVELLING_LABELS: Record<LevellingMode, string> = { off: "Off", on: "On", night: "Night" }

const CHOICE_CLASS =
  "flex-1 min-h-11 px-3 rounded-xl border border-line text-sm transition-colors hover:bg-surface-2 focus-visible:bg-surface-2 focus-visible:border-accent"

let dlg: HTMLDialogElement | null = null

function choiceRow<T extends string>(role: string, values: readonly T[], labels: Record<T, string>): string {
  const buttons = values
    .map((value) => `<button type="button" role="radio" data-role="${role}" data-value="${value}" class="${CHOICE_CLASS}">${labels[value]}</button>`)
    .join("")
  return `<div role="radiogroup" class="flex gap-2">${buttons}</div>`
}

function ensureDialog(): HTMLDialogElement {
  if (dlg && document.body.contains(dlg)) return dlg
  const node = document.createElement("dialog")
  node.id = DIALOG_ID
  node.setAttribute("aria-labelledby", `${DIALOG_ID}-title`)
  node.className = [
    "fixed inset-0 m-auto rounded-2xl border border-line bg-surface text-fg p-0",
    "w-[min(26rem,calc(100vw-2rem))] max-h-[calc(100dvh-2rem)]",
    "open:flex flex-col overflow-hidden",
    "backdrop:bg-black/30",
  ].join(" ")
  node.innerHTML = `
    <div class="flex flex-col flex-auto min-h-0 gap-4 p-5 overflow-y-auto">
      <h2 id="${DIALOG_ID}-title" class="text-base font-semibold">${TEXT.title}</h2>

      <section class="flex flex-col gap-2">
        <h3 class="text-xs font-medium text-fg-3 uppercase tracking-wide">${TEXT.picture}</h3>
        <div class="text-sm font-medium">${TEXT.sharpen}</div>
        ${choiceRow("sharpen", SHARPEN_LEVELS, SHARPEN_LABELS)}
        <p class="text-xs text-fg-3">${TEXT.sharpenHelp}</p>
      </section>

      <section class="flex flex-col gap-2">
        <h3 class="text-xs font-medium text-fg-3 uppercase tracking-wide">${TEXT.sound}</h3>
        <p data-role="sound-unavailable" class="hidden text-xs text-warn">${TEXT.soundUnavailable}</p>
        <div class="text-sm font-medium">${TEXT.levelling}</div>
        ${choiceRow("levelling", LEVELLING_MODES, LEVELLING_LABELS)}
        <p class="text-xs text-fg-3">${TEXT.levellingHelp}</p>
        <label class="flex items-center gap-2.5 min-h-11 text-sm cursor-pointer select-none">
          <span class="relative inline-flex w-9 h-5 shrink-0">
            <input type="checkbox" data-role="dialogue" class="sr-only peer" />
            <span class="absolute inset-0 rounded-full border border-line bg-surface-2 transition-colors peer-checked:bg-accent peer-checked:border-accent peer-focus-visible:ring-1 peer-focus-visible:ring-accent peer-focus-visible:ring-offset-2 peer-focus-visible:ring-offset-surface"></span>
            <span class="absolute top-0.5 left-0.5 size-4 rounded-full bg-fg-2 transition-transform peer-checked:translate-x-4 peer-checked:bg-bg"></span>
          </span>
          <span class="font-medium">${TEXT.dialogue}</span>
        </label>
        <p class="text-xs text-fg-3">${TEXT.dialogueHelp}</p>
      </section>

      <section data-role="upscaling" class="hidden flex-col gap-2">
        <h3 class="text-xs font-medium text-fg-3 uppercase tracking-wide">${TEXT.upscaling}</h3>
        <label class="flex items-center gap-2.5 min-h-11 text-sm cursor-pointer select-none">
          <span class="relative inline-flex w-9 h-5 shrink-0">
            <input type="checkbox" data-role="upscaling-toggle" class="sr-only peer" />
            <span class="absolute inset-0 rounded-full border border-line bg-surface-2 transition-colors peer-checked:bg-accent peer-checked:border-accent peer-focus-visible:ring-1 peer-focus-visible:ring-accent peer-focus-visible:ring-offset-2 peer-focus-visible:ring-offset-surface"></span>
            <span class="absolute top-0.5 left-0.5 size-4 rounded-full bg-fg-2 transition-transform peer-checked:translate-x-4 peer-checked:bg-bg"></span>
          </span>
          <span class="font-medium">${TEXT.upscalingToggle}</span>
        </label>
        <p class="text-xs text-fg-3">${TEXT.upscalingHelp}</p>
        <p data-role="upscaling-note" class="hidden text-xs text-accent"></p>
      </section>

      <div class="flex justify-end">
        <button type="button" data-role="close" class="inline-flex items-center justify-center min-h-11 rounded-xl border border-line px-4 text-sm transition-colors hover:bg-surface-2 focus-visible:bg-surface-2 focus-visible:border-accent"></button>
      </div>
    </div>
  `
  document.body.appendChild(node)
  attachDialogSpatialNav(node, { defaultElement: `#${DIALOG_ID} [data-role="sharpen"][aria-checked="true"]` })

  node.addEventListener("click", (event) => {
    const target = event.target as HTMLElement | null
    if (!target) return
    if (target === node || target.closest('[data-role="close"]')) {
      node.close()
      return
    }
    const sharpen = target.closest<HTMLElement>('[data-role="sharpen"]')
    if (sharpen) render(setEnhanceSettings({ sharpen: sharpen.dataset.value as SharpenLevel }))
    const levelling = target.closest<HTMLElement>('[data-role="levelling"]')
    if (levelling) render(setEnhanceSettings({ levelling: levelling.dataset.value as LevellingMode }))
  })
  node.querySelector<HTMLInputElement>('[data-role="dialogue"]')?.addEventListener("change", (event) => {
    render(setEnhanceSettings({ dialogue: (event.target as HTMLInputElement).checked }))
  })

  node.querySelector<HTMLInputElement>('[data-role="upscaling-toggle"]')?.addEventListener("change", (event) => {
    void saveGraphicsChoice(node, event.target as HTMLInputElement)
  })

  dlg = node
  return node
}

/** Only a Windows desktop build can move its window to another graphics card. */
function graphicsChoiceAvailable(): boolean {
  const isTauri = typeof window !== "undefined" && (!!(window as any).__TAURI_INTERNALS__ || !!(window as any).__TAURI__)
  const userAgent = typeof navigator !== "undefined" ? navigator.userAgent || "" : ""
  return isTauri && /Windows/i.test(userAgent)
}

async function showGraphicsChoice(node: HTMLElement): Promise<void> {
  const section = node.querySelector<HTMLElement>('[data-role="upscaling"]')
  const toggle = node.querySelector<HTMLInputElement>('[data-role="upscaling-toggle"]')
  if (!section || !toggle || !graphicsChoiceAvailable()) return
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    toggle.checked = (await invoke<boolean>("graphics_pref_get")) === true
    section.classList.remove("hidden")
    section.classList.add("flex")
  } catch (err) {
    log.warn("[kd:enhance] graphics choice unavailable:", err)
  }
}

async function saveGraphicsChoice(node: HTMLElement, toggle: HTMLInputElement): Promise<void> {
  const note = node.querySelector<HTMLElement>('[data-role="upscaling-note"]')
  try {
    const { invoke } = await import("@tauri-apps/api/core")
    await invoke("graphics_pref_set", { highPerformance: toggle.checked })
    if (note) note.textContent = TEXT.upscalingRestart
  } catch (err) {
    log.warn("[kd:enhance] saving the graphics choice failed:", err)
    toggle.checked = !toggle.checked
    if (note) note.textContent = TEXT.upscalingFailed
  }
  note?.classList.remove("hidden")
}

function markChoice(node: HTMLElement, role: string, current: string): void {
  for (const button of node.querySelectorAll<HTMLElement>(`[data-role="${role}"]`)) {
    const selected = button.dataset.value === current
    button.setAttribute("aria-checked", String(selected))
    button.classList.toggle("border-accent", selected)
    button.classList.toggle("bg-surface-2", selected)
    button.classList.toggle("font-medium", selected)
    button.classList.toggle("border-line", !selected)
  }
}

function render(settings: EnhanceSettings): void {
  if (!dlg) return
  markChoice(dlg, "sharpen", settings.sharpen)
  markChoice(dlg, "levelling", settings.levelling)
  const dialogue = dlg.querySelector<HTMLInputElement>('[data-role="dialogue"]')
  if (dialogue) dialogue.checked = settings.dialogue
}

/** Whether the sound options can reach what is playing; see audio-effects.ts for why some sources are excluded. */
export function soundOptionsApply(video: HTMLVideoElement | null): boolean {
  // Nothing loaded yet says nothing either way, so no warning is shown.
  if (!video || !video.currentSrc) return true
  return isSafeAudioSourceUrl(video.currentSrc)
}

function playingVideo(): HTMLVideoElement | null {
  const videos = Array.from(document.querySelectorAll("video"))
  return videos.find((video) => !!video.currentSrc) ?? videos[0] ?? null
}

export function openEnhanceDialog(): void {
  const node = ensureDialog()
  const close = node.querySelector<HTMLElement>('[data-role="close"]')
  if (close) close.textContent = t("common.close")
  node.querySelector('[data-role="sound-unavailable"]')?.classList.toggle("hidden", soundOptionsApply(playingVideo()))
  render(getEnhanceSettings())
  node.querySelector('[data-role="upscaling-note"]')?.classList.add("hidden")
  void showGraphicsChoice(node)
  try {
    if (!node.open) node.showModal()
  } catch {}
}

/** Adds the "Picture & sound" entry to the Display mode dialog, the one place every player already opens. */
export function mountEnhanceEntry(displayModeDialog: HTMLDialogElement): void {
  const closeButton = displayModeDialog.querySelector<HTMLElement>('[data-role="close"]')
  const row = closeButton?.parentElement
  if (!row || row.querySelector('[data-role="kd-enhance-entry"]')) return
  const entry = document.createElement("button")
  entry.type = "button"
  entry.dataset.role = "kd-enhance-entry"
  entry.className =
    "inline-flex items-center justify-center min-h-11 rounded-xl border border-line px-4 text-sm transition-colors hover:bg-surface-2 focus-visible:bg-surface-2 focus-visible:border-accent"
  entry.textContent = TEXT.entry
  row.classList.remove("justify-end")
  row.classList.add("justify-between", "gap-2")
  row.insertBefore(entry, closeButton)
  entry.addEventListener("click", (event) => {
    // Not a display-mode pick: keep the click from reaching that dialog's own handler.
    event.stopPropagation()
    displayModeDialog.close()
    openEnhanceDialog()
  })
}
