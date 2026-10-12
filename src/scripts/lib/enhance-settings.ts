// Picture and sound options of this fork's built-in player: sharpening, volume levelling and
// dialogue boost. Stored per device; every change is announced so a playing video follows at once.

export type SharpenLevel = "off" | "light" | "strong"
/** "night" squeezes loud and quiet much closer together than "on", for watching at low volume. */
export type LevellingMode = "off" | "on" | "night"

export interface EnhanceSettings {
  sharpen: SharpenLevel
  levelling: LevellingMode
  dialogue: boolean
}

export const ENHANCE_EVENT = "kd:enhance-changed"

export const SHARPEN_LEVELS: readonly SharpenLevel[] = ["off", "light", "strong"]
export const LEVELLING_MODES: readonly LevellingMode[] = ["off", "on", "night"]

const KEY_SHARPEN = "kd_enhance_sharpen"
const KEY_LEVELLING = "kd_enhance_levelling"
const KEY_DIALOGUE = "kd_enhance_dialogue"

function read(key: string): string {
  try {
    return (typeof localStorage !== "undefined" && localStorage.getItem(key)) || ""
  } catch {
    return ""
  }
}

function write(key: string, value: string): void {
  try {
    if (typeof localStorage === "undefined") return
    if (value) localStorage.setItem(key, value)
    else localStorage.removeItem(key)
  } catch {}
}

function oneOf<T extends string>(value: string, allowed: readonly T[], fallback: T): T {
  return (allowed as readonly string[]).includes(value) ? (value as T) : fallback
}

/** Everything is off until switched on: the player then behaves exactly as it did before. */
export function getEnhanceSettings(): EnhanceSettings {
  return {
    sharpen: oneOf(read(KEY_SHARPEN), SHARPEN_LEVELS, "off"),
    levelling: oneOf(read(KEY_LEVELLING), LEVELLING_MODES, "off"),
    dialogue: read(KEY_DIALOGUE) === "1",
  }
}

export function setEnhanceSettings(patch: Partial<EnhanceSettings>): EnhanceSettings {
  const next = { ...getEnhanceSettings(), ...patch }
  write(KEY_SHARPEN, next.sharpen === "off" ? "" : next.sharpen)
  write(KEY_LEVELLING, next.levelling === "off" ? "" : next.levelling)
  write(KEY_DIALOGUE, next.dialogue ? "1" : "")
  if (typeof document !== "undefined") {
    document.dispatchEvent(new CustomEvent(ENHANCE_EVENT, { detail: next }))
  }
  return next
}

/** Whether any sound option is on, i.e. whether the player's audio needs routing through the effects. */
export function audioEnhanceWanted(settings: EnhanceSettings = getEnhanceSettings()): boolean {
  return settings.levelling !== "off" || settings.dialogue
}
