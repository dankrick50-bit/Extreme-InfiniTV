// Chromecast and DLNA smart TVs for "Play on TV". The Rust cast bridge presents each saved TV as
// one of the app's own receivers on this device, so on this side a bridged TV is simply a saved
// TV whose address is 127.0.0.1; the picker, pill and remote need to know nothing more.
import { log } from "@/scripts/lib/log.js"
import {
  listTvDevices,
  saveTvDevice,
  removeTvDevice,
  getCastSession,
  updateCastSession,
  TV_DEVICES_EVENT,
  type TvDevice,
} from "@/scripts/lib/tv-cast.js"

export type BridgedKind = "chromecast" | "dlna"

/** A saved TV as the bridge reports it: `host`/`port` are the bridge, `address` is the TV itself. */
export interface BridgedDevice {
  id: string
  name: string
  host: string
  port: number
  key: string
  kind: BridgedKind
  address: string
}

export interface FoundTv {
  foundId: string
  kind: BridgedKind
  name: string
  address: string
  model: string | null
  alreadyAdded: boolean
}

export interface BridgeStatus {
  port: number | null
  devices: BridgedDevice[]
}

const BRIDGED_ID_PREFIX = "ext-"
/** deviceId -> what the TV really is, for display in place of the loopback address. */
const LABELS_STORAGE_KEY = "kd_cast_bridge_labels"

type BridgedLabels = Record<string, { kind: BridgedKind; address: string }>

function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && (!!(window as any).__TAURI_INTERNALS__ || !!(window as any).__TAURI__)
}

async function invokeBridge<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core")
  return invoke<T>(command, args)
}

function readLabels(): BridgedLabels {
  try {
    const raw = typeof localStorage !== "undefined" ? localStorage.getItem(LABELS_STORAGE_KEY) : null
    const parsed = raw ? JSON.parse(raw) : null
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {}
  } catch {
    return {}
  }
}

function writeLabels(labels: BridgedLabels): void {
  try {
    if (typeof localStorage === "undefined") return
    if (Object.keys(labels).length === 0) localStorage.removeItem(LABELS_STORAGE_KEY)
    else localStorage.setItem(LABELS_STORAGE_KEY, JSON.stringify(labels))
  } catch {}
}

export function isBridgedDeviceId(id: string): boolean {
  return id.startsWith(BRIDGED_ID_PREFIX)
}

export function bridgedKindName(kind: BridgedKind): string {
  return kind === "chromecast" ? "Chromecast" : "Smart TV"
}

/** What to show instead of the loopback address a bridged TV is stored under; null for the app's own receivers. */
export function bridgedAddressLabel(device: Pick<TvDevice, "id">): string | null {
  if (!isBridgedDeviceId(device.id)) return null
  const label = readLabels()[device.id]
  return label ? `${bridgedKindName(label.kind)} · ${label.address}` : "Chromecast or smart TV"
}

export interface BridgeSyncPlan {
  saves: TvDevice[]
  removals: string[]
}

/** Pure reconcile of the saved TVs against what the bridge reports it still has. */
export function planBridgeSync(localDevices: TvDevice[], status: BridgeStatus): BridgeSyncPlan {
  const plan: BridgeSyncPlan = { saves: [], removals: [] }
  // No port means the bridge could not start, so its empty list says nothing about what is saved.
  if (status.port === null) return plan
  const remoteById = new Map(status.devices.map((device) => [device.id, device]))
  for (const local of localDevices) {
    if (!isBridgedDeviceId(local.id)) continue
    const remote = remoteById.get(local.id)
    if (!remote) {
      plan.removals.push(local.id)
      continue
    }
    const moved =
      local.host !== remote.host || local.port !== remote.port || local.key !== remote.key || local.name !== remote.name
    if (moved) {
      plan.saves.push({
        id: local.id,
        name: remote.name,
        host: remote.host,
        port: remote.port,
        key: remote.key,
        createdAt: local.createdAt,
        lastSeenAt: local.lastSeenAt,
      })
    }
  }
  return plan
}

/** One bridge operation at a time: a sync landing between "bridge saved it" and "we saved it" would undo an add. */
let queue: Promise<unknown> = Promise.resolve()

function enqueue<T>(task: () => Promise<T>): Promise<T> {
  const run = queue.then(task, task)
  queue = run.catch(() => {})
  return run
}

function rememberBridgedDevice(bridged: BridgedDevice): TvDevice {
  const existing = listTvDevices().find((device) => device.id === bridged.id)
  const now = Date.now()
  const device: TvDevice = {
    id: bridged.id,
    name: bridged.name,
    host: bridged.host,
    port: bridged.port,
    key: bridged.key,
    createdAt: existing?.createdAt ?? now,
    lastSeenAt: existing?.lastSeenAt ?? now,
  }
  // Label first: saving the device re-renders lists that read it.
  writeLabels({ ...readLabels(), [bridged.id]: { kind: bridged.kind, address: bridged.address } })
  saveTvDevice(device)
  return device
}

/** Scans the home network for Chromecasts and smart TVs; takes about three seconds. */
export function findTvs(): Promise<FoundTv[]> {
  return invokeBridge<FoundTv[]>("cast_bridge_discover")
}

/** Saves a TV from the latest scan and returns it as a TV the picker can cast to. */
export function addFoundTv(foundId: string): Promise<TvDevice> {
  return enqueue(async () => rememberBridgedDevice(await invokeBridge<BridgedDevice>("cast_bridge_add", { foundId })))
}

/** Saves a Chromecast by its address, for networks that hide it from the scan. */
export function addChromecastByAddress(host: string): Promise<TvDevice> {
  return enqueue(async () => rememberBridgedDevice(await invokeBridge<BridgedDevice>("cast_bridge_add_manual", { host })))
}

/** Turns a bridge error ("CODE:detail") into a sentence for the person adding a TV. */
export function describeBridgeError(error: unknown): string {
  const text = typeof error === "string" ? error : error instanceof Error ? error.message : ""
  const code = text.split(":")[0]
  switch (code) {
    case "UNREACHABLE":
      return "No Chromecast answered at that address. Check the address and that the TV is on."
    case "INVALID":
      return "Enter the TV's address on your home network, like 192.168.1.20."
    case "STALE":
      return "Search again, then pick the TV."
    case "LIMIT":
      return "Too many TVs are saved. Remove one in Settings first."
    default:
      return "Could not add this TV. Check that it is on and on the same Wi-Fi."
  }
}

let syncWaiting = false

/**
 * Keeps the saved TVs and the bridge agreeing: tells the bridge which TVs were removed here, and
 * picks up a changed bridge port. Cheap, and skipped outright on installs with no bridged TV.
 */
export function syncCastBridge(): Promise<void> {
  if (!isTauriRuntime() || syncWaiting) return Promise.resolve()
  syncWaiting = true
  return enqueue(async () => {
    syncWaiting = false
    const bridgedIds = listTvDevices()
      .filter((device) => isBridgedDeviceId(device.id))
      .map((device) => device.id)
    if (bridgedIds.length === 0 && Object.keys(readLabels()).length === 0) return

    const status = await invokeBridge<BridgeStatus>("cast_bridge_sync", { knownIds: bridgedIds })
    const plan = planBridgeSync(listTvDevices(), status)
    for (const device of plan.saves) saveTvDevice(device)
    for (const id of plan.removals) removeTvDevice(id)
    if (status.port === null) return

    writeLabels(Object.fromEntries(status.devices.map((device) => [device.id, { kind: device.kind, address: device.address }])))
    const session = getCastSession()
    const sessionDevice = session ? status.devices.find((device) => device.id === session.deviceId) : undefined
    if (session && sessionDevice && (session.port !== sessionDevice.port || session.key !== sessionDevice.key)) {
      updateCastSession({ host: sessionDevice.host, port: sessionDevice.port, key: sessionDevice.key })
    }
  }).catch((err) => {
    syncWaiting = false
    log.warn("[kd:cast-bridge] sync failed:", err)
  })
}

let initialised = false

/** Call once per page: reconciles now and again whenever the saved TVs change. */
export function initCastBridge(): void {
  if (initialised || !isTauriRuntime() || typeof document === "undefined") return
  initialised = true
  document.addEventListener(TV_DEVICES_EVENT, () => {
    void syncCastBridge()
  })
  void syncCastBridge()
}
