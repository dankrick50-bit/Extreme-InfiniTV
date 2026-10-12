// Keeps downloads from competing with a cast. On an account that allows one connection, whatever
// is playing on the TV is that connection, and a download starting beside it gets the stream cut.
import { getCastSession, CAST_SESSION_EVENT, type CastSession } from "@/scripts/lib/tv-cast.js"
import { subscribeCastStateFeed } from "@/scripts/lib/tv-cast-state-feed"
import { noteCastStreaming } from "@/scripts/lib/download-native.js"

/** Only matters if the TV has to be polled; a slow beat is enough to notice it has stopped. */
const WATCH_CADENCE_MS = 15000
const HOLDING_STATES = new Set(["loading", "buffering", "playing", "paused"])

/** Whether a TV reporting this state has a provider stream open (a paused film still holds its link). */
export function castStateHoldsConnection(state: string): boolean {
  return HOLDING_STATES.has(state)
}

/**
 * The guess to go on before the TV has reported: a session that was started to play something is
 * taken to be playing. A dismissed one is left for the TV's own report, which lands within moments,
 * so a TV that has since gone away cannot hold downloads back.
 */
export function sessionLikelyStreaming(session: Pick<CastSession, "connectedOnly" | "dismissed"> | null): boolean {
  return !!session && session.connectedOnly !== true && session.dismissed !== true
}

let stopWatching: (() => void) | null = null

function follow(): void {
  const session = getCastSession()
  if (!session) {
    stopWatching?.()
    stopWatching = null
    noteCastStreaming(false)
    return
  }
  if (stopWatching) return
  noteCastStreaming(sessionLikelyStreaming(session))
  stopWatching = subscribeCastStateFeed((state) => noteCastStreaming(castStateHoldsConnection(state.state)), {
    cadenceMs: WATCH_CADENCE_MS,
    // The feed drops every listener when the TV stops answering; nothing is left to unsubscribe.
    onLost: () => {
      stopWatching = null
      noteCastStreaming(false)
    },
  })
}

let initialised = false

/** Call once per page: follows whatever is being cast and parks or releases downloads to match. */
export function initCastDownloadGuard(): void {
  if (initialised || typeof document === "undefined") return
  initialised = true
  document.addEventListener(CAST_SESSION_EVENT, follow)
  follow()
}
