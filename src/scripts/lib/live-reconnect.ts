// A live channel has no end. When one that was playing ends or fails, the provider closed or
// lost the connection, so the player opens it again instead of stopping on the last picture.
//
// The allowance keeps a dead feed, or an account that is in use somewhere else, from being
// reopened forever: a few tries in a row, and a connection that then holds earns them back.

export const LIVE_RECONNECT_MAX = 3
export const LIVE_RECONNECT_REFILL_MS = 30_000
// Long enough for the provider to notice the old connection is gone before the new one arrives.
export const LIVE_RECONNECT_DELAY_MS = 1000

export interface ReconnectBudgetOptions {
  max?: number
  refillAfterMs?: number
  now?: () => number
}

export interface ReconnectBudget {
  /** True when one more reconnect is allowed, and counts it. */
  take(): boolean
  /** Playback is running; safe to call on every "playing". */
  notePlaying(): void
  /** A new tune starts with the full allowance. */
  reset(): void
}

export function createReconnectBudget(options: ReconnectBudgetOptions = {}): ReconnectBudget {
  const max = options.max ?? LIVE_RECONNECT_MAX
  const refillAfterMs = options.refillAfterMs ?? LIVE_RECONNECT_REFILL_MS
  const now = options.now ?? Date.now
  let used = 0
  let playingSince: number | null = null

  return {
    take() {
      if (playingSince !== null && now() - playingSince >= refillAfterMs) used = 0
      playingSince = null
      if (used >= max) return false
      used += 1
      return true
    },
    notePlaying() {
      if (playingSince === null) playingSince = now()
    },
    reset() {
      used = 0
      playingSince = null
    },
  }
}
