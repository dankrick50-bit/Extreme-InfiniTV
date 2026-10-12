// "Chromecast or smart TV": finds TVs that cannot run this app and saves the chosen one as a
// "Play on TV" device. Opened from the TV picker; shares its look.

import { attachDialogSpatialNav } from "@/scripts/lib/dialog-spatial-nav.js"
import { log } from "@/scripts/lib/log.js"
import { t } from "@/scripts/lib/i18n.js"
import { ICON_DEVICE_TV, ICON_X } from "@/scripts/lib/icons.js"
import type { TvDevice } from "@/scripts/lib/tv-cast.js"
import {
  addChromecastByAddress,
  addFoundTv,
  bridgedKindName,
  describeBridgeError,
  findTvs,
  type FoundTv,
} from "@/scripts/lib/cast-bridge"

const DIALOG_ID = "external-tv-picker"

// This screen exists only in this fork, so its wording lives here rather than in the shared translation files.
const TEXT = {
  title: "Chromecast or smart TV",
  subtitle: "Finds Chromecast, Samsung, LG, Sony and other TVs on your Wi-Fi.",
  noneFound: "No TVs found. Check that the TV is on and on the same Wi-Fi as this device.",
  scanFailed: "The search could not run. Try again.",
  add: "Add",
  added: "Added",
  adding: "Adding…",
  manualToggle: "Enter a Chromecast address",
  manualLabel: "Chromecast address",
  manualPlaceholder: "192.168.1.20",
  manualHelp: "Shown on the TV under Settings, About or Network.",
  note: "Films and series work best. Live channels may not play on a Chromecast.",
}

const ROW_BTN_CLASS =
  "xt-picker-row flex-1 min-w-0 flex items-center gap-3.5 min-h-11 px-3 py-2.5 rounded-xl text-left hover:bg-surface-2 focus-visible:bg-surface-2 active:scale-[0.98]"

let dlg: HTMLDialogElement | null = null

function ensureDialog(): HTMLDialogElement | null {
  if (typeof document === "undefined") return null
  if (dlg && document.body.contains(dlg)) return dlg
  const node = document.createElement("dialog")
  node.id = DIALOG_ID
  node.setAttribute("aria-labelledby", `${DIALOG_ID}-title`)
  node.className = [
    "fixed inset-0 m-auto rounded-2xl border border-line bg-surface text-fg p-0",
    "w-[min(32rem,calc(100vw-2rem))] max-h-[min(85dvh,40rem)]",
    "open:flex flex-col overflow-hidden",
    "backdrop:bg-black/60",
  ].join(" ")
  document.body.appendChild(node)
  dlg = node
  return dlg
}

function describeFound(found: FoundTv): string {
  return [bridgedKindName(found.kind), found.model, found.address].filter(Boolean).join(" · ")
}

/** Resolves with the saved TV, or null when the dialog is dismissed. */
export function openExternalTvDialog(): Promise<TvDevice | null> {
  const dialog = ensureDialog()
  if (!dialog) return Promise.resolve(null)

  return new Promise((resolve) => {
    dialog.innerHTML = `
      <div class="flex flex-col flex-auto min-h-0 p-5 sm:p-6 gap-4">
        <header class="flex items-start gap-3.5 shrink-0 px-3">
          <span class="icon-mark icon-mark--lg" aria-hidden="true">${ICON_DEVICE_TV}</span>
          <div class="flex flex-col gap-1 min-w-0 pt-0.5">
            <h2 id="${DIALOG_ID}-title" class="text-lg font-semibold leading-tight tracking-tight"></h2>
            <div data-role="subtitle" class="text-sm text-fg-3"></div>
          </div>
          <button type="button" data-role="close" class="ms-auto shrink-0 min-h-11 min-w-11 grid place-items-center rounded-xl text-fg-3 hover:bg-surface-2 hover:text-fg focus-visible:bg-surface-2">${ICON_X}</button>
        </header>
        <ul data-role="list" class="hidden flex flex-col gap-1 overflow-y-auto min-h-0 list-none m-0 p-0"></ul>
        <div class="flex items-center gap-3 shrink-0 px-3 min-h-11">
          <span data-role="scan-status" class="flex items-center gap-2 min-w-0 text-sm text-fg-3"></span>
          <button type="button" data-role="rescan-btn" class="hidden shrink-0 ms-auto min-h-11 px-2.5 -me-2.5 rounded-xl text-sm text-fg-2 hover:bg-surface-2 hover:text-fg focus-visible:bg-surface-2"></button>
        </div>
        <div class="flex flex-col gap-3 shrink-0 px-3">
          <button type="button" data-role="toggle-manual" class="btn self-end"></button>
          <form data-role="manual-form" class="hidden flex flex-col gap-3">
            <div class="flex flex-col gap-1">
              <label for="${DIALOG_ID}-host" data-role="manual-label" class="text-xs font-medium text-fg-2"></label>
              <input id="${DIALOG_ID}-host" data-role="manual-input" type="text" inputmode="decimal" autocomplete="off" class="field-input tabular-nums" />
              <span data-role="manual-help" class="text-xs text-fg-3"></span>
              <span data-role="manual-error" class="hidden text-xs text-bad"></span>
            </div>
            <button type="submit" data-role="manual-submit" class="btn-primary self-end"></button>
          </form>
          <p data-role="note" class="text-xs text-fg-3"></p>
        </div>
      </div>
    `

    const byRole = <T extends HTMLElement>(role: string) => dialog.querySelector<T>(`[data-role="${role}"]`)!
    dialog.querySelector<HTMLElement>("h2")!.textContent = TEXT.title
    byRole("subtitle").textContent = TEXT.subtitle
    byRole("close").setAttribute("aria-label", t("common.cancel"))
    byRole("note").textContent = TEXT.note

    const listEl = byRole<HTMLUListElement>("list")
    const scanStatusEl = byRole("scan-status")
    const rescanBtn = byRole<HTMLButtonElement>("rescan-btn")
    rescanBtn.textContent = t("cast.picker.rescan")
    const toggleManualBtn = byRole<HTMLButtonElement>("toggle-manual")
    toggleManualBtn.textContent = TEXT.manualToggle
    const manualForm = byRole<HTMLFormElement>("manual-form")
    const manualInput = byRole<HTMLInputElement>("manual-input")
    manualInput.placeholder = TEXT.manualPlaceholder
    byRole("manual-label").textContent = TEXT.manualLabel
    byRole("manual-help").textContent = TEXT.manualHelp
    const manualError = byRole("manual-error")
    const manualSubmit = byRole<HTMLButtonElement>("manual-submit")
    manualSubmit.textContent = TEXT.add

    let resolved = false
    let scanToken = 0
    let busy = false

    const settle = (choice: TvDevice | null) => {
      if (resolved) return
      resolved = true
      scanToken += 1
      detach()
      try {
        if (dialog.open) dialog.close()
      } catch {}
      resolve(choice)
    }

    function setScanStatus(kind: "searching" | "none" | "failed" | "found"): void {
      scanStatusEl.replaceChildren()
      if (kind === "searching") {
        const spinner = document.createElement("span")
        spinner.className = "size-4 rounded-full border-2 border-line border-t-accent animate-spin shrink-0"
        spinner.setAttribute("aria-hidden", "true")
        const label = document.createElement("span")
        label.textContent = t("cast.picker.searching")
        scanStatusEl.append(spinner, label)
      } else if (kind === "none") {
        scanStatusEl.textContent = TEXT.noneFound
      } else if (kind === "failed") {
        scanStatusEl.textContent = TEXT.scanFailed
      }
      rescanBtn.classList.toggle("hidden", kind === "searching")
    }

    async function addFromRow(found: FoundTv, button: HTMLButtonElement, actionEl: HTMLElement, errorEl: HTMLElement): Promise<void> {
      if (busy) return
      busy = true
      button.disabled = true
      actionEl.textContent = TEXT.adding
      errorEl.classList.add("hidden")
      try {
        settle(await addFoundTv(found.foundId))
      } catch (err) {
        log.warn("[kd:cast-bridge] add failed:", err)
        button.disabled = false
        actionEl.textContent = TEXT.add
        errorEl.textContent = describeBridgeError(err)
        errorEl.classList.remove("hidden")
      } finally {
        busy = false
      }
    }

    function renderRow(found: FoundTv): HTMLLIElement {
      const row = document.createElement("li")
      row.className = "flex items-center gap-1"

      const button = document.createElement("button")
      button.type = "button"
      button.dataset.role = "found-tv"
      button.className = ROW_BTN_CLASS

      const icon = document.createElement("span")
      icon.className = "shrink-0 grid place-items-center text-fg-3"
      icon.setAttribute("aria-hidden", "true")
      icon.innerHTML = ICON_DEVICE_TV
      button.appendChild(icon)

      const textCol = document.createElement("span")
      textCol.className = "flex flex-col min-w-0 flex-1 gap-0.5"
      // Names come from devices on the network: always set as text, never as markup.
      const nameEl = document.createElement("span")
      nameEl.className = "text-sm font-medium truncate"
      nameEl.textContent = found.name
      const metaEl = document.createElement("span")
      metaEl.className = "text-xs text-fg-3 truncate tabular-nums"
      metaEl.textContent = describeFound(found)
      const errorEl = document.createElement("span")
      errorEl.className = "hidden text-xs text-bad"
      textCol.append(nameEl, metaEl, errorEl)
      button.appendChild(textCol)

      const actionEl = document.createElement("span")
      actionEl.className = "shrink-0 text-xs font-medium text-accent"
      actionEl.textContent = found.alreadyAdded ? TEXT.added : TEXT.add
      button.appendChild(actionEl)

      button.addEventListener("click", () => void addFromRow(found, button, actionEl, errorEl))
      row.appendChild(button)
      return row
    }

    async function scan(): Promise<void> {
      const token = ++scanToken
      listEl.replaceChildren()
      listEl.classList.add("hidden")
      setScanStatus("searching")
      try {
        const found = await findTvs()
        if (token !== scanToken) return
        listEl.replaceChildren(...found.map(renderRow))
        listEl.classList.toggle("hidden", found.length === 0)
        setScanStatus(found.length === 0 ? "none" : "found")
        window.SpatialNavigation?.makeFocusable?.()
        if (found.length > 0 && manualForm.classList.contains("hidden")) {
          listEl.querySelector<HTMLElement>('[data-role="found-tv"]')?.focus()
        }
      } catch (err) {
        if (token !== scanToken) return
        log.warn("[kd:cast-bridge] scan failed:", err)
        setScanStatus("failed")
      }
    }

    const onRescan = () => void scan()

    const onToggleManual = () => {
      manualForm.classList.toggle("hidden")
      if (!manualForm.classList.contains("hidden")) manualInput.focus()
    }

    const onManualSubmit = async (event: Event) => {
      event.preventDefault()
      if (busy) return
      busy = true
      manualError.classList.add("hidden")
      manualSubmit.disabled = true
      manualSubmit.dataset.loading = "true"
      try {
        settle(await addChromecastByAddress(manualInput.value.trim()))
      } catch (err) {
        log.warn("[kd:cast-bridge] manual add failed:", err)
        manualSubmit.disabled = false
        delete manualSubmit.dataset.loading
        manualError.textContent = describeBridgeError(err)
        manualError.classList.remove("hidden")
      } finally {
        busy = false
      }
    }

    const onClick = (event: Event) => {
      const target = event.target as HTMLElement | null
      if (!target) return
      if (target.closest('[data-role="close"]') || target === dialog) settle(null)
    }

    const onCancel = (event: Event) => {
      event.preventDefault()
      settle(null)
    }

    const onClose = () => settle(null)

    function detach(): void {
      rescanBtn.removeEventListener("click", onRescan)
      toggleManualBtn.removeEventListener("click", onToggleManual)
      manualForm.removeEventListener("submit", onManualSubmit)
      dialog!.removeEventListener("click", onClick)
      dialog!.removeEventListener("cancel", onCancel)
      dialog!.removeEventListener("close", onClose)
    }

    rescanBtn.addEventListener("click", onRescan)
    toggleManualBtn.addEventListener("click", onToggleManual)
    manualForm.addEventListener("submit", onManualSubmit)
    dialog.addEventListener("click", onClick)
    dialog.addEventListener("cancel", onCancel)
    dialog.addEventListener("close", onClose)

    try {
      dialog.showModal()
    } catch {
      detach()
      resolve(null)
      return
    }

    attachDialogSpatialNav(dialog, {
      defaultElement: `#${DIALOG_ID} [data-role="found-tv"], #${DIALOG_ID} [data-role="toggle-manual"]`,
    })
    toggleManualBtn.focus()
    void scan()
  })
}
