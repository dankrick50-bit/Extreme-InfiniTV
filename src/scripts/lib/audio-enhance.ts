// Volume levelling and dialogue boost for the built-in player, as a small Web Audio chain that
// audio-effects.ts places between the video's sound and the speakers.
//
//   levelling  a compressor: loud passages (adverts, explosions) are pulled down and the whole
//              signal is brought back up, so quiet speech and loud scenes sit closer together
//   dialogue   lifts the band speech lives in and eases the rumble below it

import { getEnhanceSettings, type EnhanceSettings, type LevellingMode } from "@/scripts/lib/enhance-settings"

export interface CompressorParams {
  threshold: number
  knee: number
  ratio: number
  attack: number
  release: number
}

export interface EnhanceParams {
  /** null bypasses the compressor altogether. */
  compressor: CompressorParams | null
  /** Gain of the speech band, in dB. */
  speechDb: number
  /** Gain below the speech band, in dB. */
  rumbleDb: number
  /** Final trim, linear. Leaves headroom for the speech lift when nothing else tames the peaks. */
  output: number
}

const LEVELLING: Record<Exclude<LevellingMode, "off">, CompressorParams> = {
  on: { threshold: -24, knee: 20, ratio: 4, attack: 0.01, release: 0.25 },
  night: { threshold: -42, knee: 12, ratio: 12, attack: 0.003, release: 0.3 },
}

const SPEECH_HZ = 2600
const RUMBLE_HZ = 160
const SPEECH_LIFT_DB = 6
const RUMBLE_CUT_DB = -3

/** Pure: what each node should be set to for these settings. */
export function enhanceParams(settings: EnhanceSettings): EnhanceParams {
  const compressor = settings.levelling === "off" ? null : LEVELLING[settings.levelling]
  return {
    compressor,
    speechDb: settings.dialogue ? SPEECH_LIFT_DB : 0,
    rumbleDb: settings.dialogue ? RUMBLE_CUT_DB : 0,
    // The compressor catches the lifted peaks itself; without it, trim so the lift cannot clip.
    output: settings.dialogue && !compressor ? 0.8 : 1,
  }
}

export interface EnhanceChain {
  /** Re-reads the settings and applies them to the running chain. */
  update(settings?: EnhanceSettings): void
}

/**
 * Connects `source` to `output` through the effects and returns a handle to retune them.
 * With everything off the signal passes straight through, untouched.
 */
export function connectEnhanceChain(ctx: AudioContext, source: AudioNode, output: AudioNode): EnhanceChain {
  const compressor = ctx.createDynamicsCompressor()
  const speech = ctx.createBiquadFilter()
  speech.type = "peaking"
  speech.frequency.value = SPEECH_HZ
  speech.Q.value = 0.8
  const rumble = ctx.createBiquadFilter()
  rumble.type = "lowshelf"
  rumble.frequency.value = RUMBLE_HZ
  const trim = ctx.createGain()

  speech.connect(rumble)
  rumble.connect(trim)
  trim.connect(output)

  let compressing: boolean | null = null

  function route(useCompressor: boolean): void {
    if (compressing === useCompressor) return
    compressing = useCompressor
    // The compressor only works in stereo, so it is taken out of the path when off: surround
    // sound then reaches the speakers with all its channels.
    try {
      source.disconnect()
    } catch {}
    try {
      compressor.disconnect()
    } catch {}
    if (useCompressor) {
      source.connect(compressor)
      compressor.connect(speech)
    } else {
      source.connect(speech)
    }
  }

  function update(settings: EnhanceSettings = getEnhanceSettings()): void {
    const params = enhanceParams(settings)
    if (params.compressor) {
      compressor.threshold.value = params.compressor.threshold
      compressor.knee.value = params.compressor.knee
      compressor.ratio.value = params.compressor.ratio
      compressor.attack.value = params.compressor.attack
      compressor.release.value = params.compressor.release
    }
    speech.gain.value = params.speechDb
    rumble.gain.value = params.rumbleDb
    trim.gain.value = params.output
    route(!!params.compressor)
  }

  update()
  return { update }
}
