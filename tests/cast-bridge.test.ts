/**
 * @vitest-environment jsdom
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest"

const invokeMock = vi.fn()
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }))

import { listTvDevices, saveTvDevice, removeTvDevice, setCastSession, getCastSession, type TvDevice } from "@/scripts/lib/tv-cast"
import {
  addFoundTv,
  bridgedAddressLabel,
  describeBridgeError,
  isBridgedDeviceId,
  planBridgeSync,
  syncCastBridge,
  type BridgedDevice,
} from "@/scripts/lib/cast-bridge"

// Same in-memory Storage stub as tests/tv-cast-devices.test.ts: Node's own experimental
// localStorage would otherwise shadow jsdom's.
class MemoryStorage {
  private store = new Map<string, string>()
  get length(): number {
    return this.store.size
  }
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
  key(index: number): string | null {
    return Array.from(this.store.keys())[index] ?? null
  }
}

const memoryLocalStorage = new MemoryStorage()
const memorySessionStorage = new MemoryStorage()

function ownReceiver(): TvDevice {
  return { id: "abc123", name: "Fire TV", host: "192.168.1.40", port: 47815, key: "a".repeat(32), createdAt: 1, lastSeenAt: 2 }
}

function bridgedTv(overrides: Partial<TvDevice> = {}): TvDevice {
  return {
    id: "ext-dlna-0a1b2c",
    name: '75" QLED',
    host: "127.0.0.1",
    port: 47830,
    key: "b".repeat(32),
    createdAt: 10,
    lastSeenAt: 20,
    ...overrides,
  }
}

function reported(overrides: Partial<BridgedDevice> = {}): BridgedDevice {
  return {
    id: "ext-dlna-0a1b2c",
    name: '75" QLED',
    host: "127.0.0.1",
    port: 47830,
    key: "b".repeat(32),
    kind: "dlna",
    address: "192.168.1.110",
    ...overrides,
  }
}

beforeEach(() => {
  vi.stubGlobal("Storage", MemoryStorage)
  vi.stubGlobal("localStorage", memoryLocalStorage as unknown as Storage)
  vi.stubGlobal("sessionStorage", memorySessionStorage as unknown as Storage)
  memoryLocalStorage.clear()
  memorySessionStorage.clear()
  ;(window as any).__TAURI_INTERNALS__ = {}
  invokeMock.mockReset()
})

afterEach(() => {
  delete (window as any).__TAURI_INTERNALS__
  vi.unstubAllGlobals()
})

describe("planBridgeSync", () => {
  it("leaves the app's own receivers alone", () => {
    const plan = planBridgeSync([ownReceiver()], { port: 47830, devices: [] })
    expect(plan).toEqual({ saves: [], removals: [] })
  })

  it("changes nothing when the bridge and the saved TV agree", () => {
    const plan = planBridgeSync([bridgedTv()], { port: 47830, devices: [reported()] })
    expect(plan).toEqual({ saves: [], removals: [] })
  })

  it("follows the bridge to a new port and keeps the TV's history", () => {
    const plan = planBridgeSync([bridgedTv()], { port: 47833, devices: [reported({ port: 47833 })] })
    expect(plan.removals).toEqual([])
    expect(plan.saves).toEqual([bridgedTv({ port: 47833 })])
  })

  it("drops a saved TV the bridge no longer knows", () => {
    const plan = planBridgeSync([bridgedTv(), ownReceiver()], { port: 47830, devices: [] })
    expect(plan.removals).toEqual(["ext-dlna-0a1b2c"])
    expect(plan.saves).toEqual([])
  })

  it("removes nothing when the bridge could not start", () => {
    const plan = planBridgeSync([bridgedTv()], { port: null, devices: [] })
    expect(plan).toEqual({ saves: [], removals: [] })
  })
})

describe("bridgedAddressLabel", () => {
  it("is null for the app's own receivers so their real address shows", () => {
    expect(isBridgedDeviceId(ownReceiver().id)).toBe(false)
    expect(bridgedAddressLabel(ownReceiver())).toBeNull()
  })

  it("shows the TV's kind and real address once it was added", async () => {
    invokeMock.mockResolvedValueOnce(reported({ id: "ext-chromecast-ffee00", kind: "chromecast", address: "192.168.1.50", name: "Living Room TV" }))
    const device = await addFoundTv("chromecast:192.168.1.50:8009")
    expect(bridgedAddressLabel(device)).toBe("Chromecast · 192.168.1.50")
  })

  it("falls back to a plain description when the kind is not known yet", () => {
    expect(bridgedAddressLabel(bridgedTv())).toBe("Chromecast or smart TV")
  })
})

describe("addFoundTv", () => {
  it("saves the TV as a Play on TV device that points at the bridge", async () => {
    invokeMock.mockResolvedValueOnce(reported())
    const device = await addFoundTv("dlna:192.168.1.110:9197")

    expect(invokeMock).toHaveBeenCalledWith("cast_bridge_add", { foundId: "dlna:192.168.1.110:9197" })
    expect(device.host).toBe("127.0.0.1")
    expect(device.port).toBe(47830)
    expect(listTvDevices().map((saved) => saved.id)).toEqual(["ext-dlna-0a1b2c"])
    expect(bridgedAddressLabel(device)).toBe("Smart TV · 192.168.1.110")
  })

  it("keeps when a TV was first added if it is added again", async () => {
    saveTvDevice(bridgedTv({ createdAt: 111, lastSeenAt: 222 }))
    invokeMock.mockResolvedValueOnce(reported({ name: "Lounge TV" }))
    const device = await addFoundTv("dlna:192.168.1.110:9197")
    expect(device.createdAt).toBe(111)
    expect(device.lastSeenAt).toBe(222)
    expect(listTvDevices()).toHaveLength(1)
    expect(listTvDevices()[0].name).toBe("Lounge TV")
  })

  it("saves nothing when the bridge refuses", async () => {
    invokeMock.mockRejectedValueOnce("STALE:scan again and pick the tv")
    await expect(addFoundTv("dlna:192.168.1.110:9197")).rejects.toBe("STALE:scan again and pick the tv")
    expect(listTvDevices()).toEqual([])
  })
})

describe("syncCastBridge", () => {
  it("does not call the bridge on an install with no bridged TV", async () => {
    saveTvDevice(ownReceiver())
    await syncCastBridge()
    expect(invokeMock).not.toHaveBeenCalled()
  })

  it("tells the bridge which TVs are still saved and repairs a moved port", async () => {
    saveTvDevice(ownReceiver())
    saveTvDevice(bridgedTv())
    invokeMock.mockResolvedValueOnce({ port: 47833, devices: [reported({ port: 47833 })] })
    await syncCastBridge()

    expect(invokeMock).toHaveBeenCalledWith("cast_bridge_sync", { knownIds: ["ext-dlna-0a1b2c"] })
    const saved = listTvDevices().find((device) => device.id === "ext-dlna-0a1b2c")
    expect(saved?.port).toBe(47833)
    expect(listTvDevices()).toHaveLength(2)
  })

  it("moves a running cast session along with the port", async () => {
    saveTvDevice(bridgedTv())
    setCastSession({
      deviceId: "ext-dlna-0a1b2c",
      deviceName: '75" QLED',
      host: "127.0.0.1",
      port: 47830,
      key: "b".repeat(32),
      title: "A Film",
      isLive: false,
      startedAt: 1,
    })
    invokeMock.mockResolvedValueOnce({ port: 47833, devices: [reported({ port: 47833 })] })
    await syncCastBridge()
    expect(getCastSession()?.port).toBe(47833)
  })

  it("lets the bridge forget a TV once its last entry here is removed", async () => {
    invokeMock.mockResolvedValueOnce(reported())
    await addFoundTv("dlna:192.168.1.110:9197")
    removeTvDevice("ext-dlna-0a1b2c")

    invokeMock.mockReset()
    invokeMock.mockResolvedValueOnce({ port: 47830, devices: [] })
    await syncCastBridge()
    expect(invokeMock).toHaveBeenCalledWith("cast_bridge_sync", { knownIds: [] })

    // With nothing bridged and nothing remembered, later page loads skip the call entirely.
    invokeMock.mockReset()
    await syncCastBridge()
    expect(invokeMock).not.toHaveBeenCalled()
  })

  it("removes a saved TV the bridge has lost, so no dead entry is left in the picker", async () => {
    saveTvDevice(bridgedTv())
    invokeMock.mockResolvedValueOnce({ port: 47830, devices: [] })
    await syncCastBridge()
    expect(listTvDevices()).toEqual([])
  })

  it("keeps saved TVs when the bridge could not start", async () => {
    saveTvDevice(bridgedTv())
    invokeMock.mockResolvedValueOnce({ port: null, devices: [] })
    await syncCastBridge()
    expect(listTvDevices()).toHaveLength(1)
  })

  it("survives the bridge call failing", async () => {
    saveTvDevice(bridgedTv())
    invokeMock.mockRejectedValueOnce(new Error("ipc down"))
    await expect(syncCastBridge()).resolves.toBeUndefined()
    expect(listTvDevices()).toHaveLength(1)
  })
})

describe("describeBridgeError", () => {
  it("turns bridge codes into plain sentences", () => {
    expect(describeBridgeError("UNREACHABLE:no chromecast answered at that address")).toMatch(/No Chromecast answered/)
    expect(describeBridgeError("INVALID:enter the tv's address")).toMatch(/192\.168\.1\.20/)
    expect(describeBridgeError("STALE:scan again")).toMatch(/Search again/)
    expect(describeBridgeError("LIMIT:too many tvs saved")).toMatch(/Too many TVs/)
  })

  it("has a safe default for anything else", () => {
    expect(describeBridgeError(new Error("boom"))).toMatch(/Could not add this TV/)
    expect(describeBridgeError(undefined)).toMatch(/Could not add this TV/)
  })
})
