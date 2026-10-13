// Fixes a freeze in the app framework (Tauri) on Android, by editing its downloaded source
// before the Android build compiles it.
//
// The freeze: on Android a page starting or finishing to load is reported on the UI thread,
// and Tauri then waits for its plugin store. A plugin command that is running holds that store,
// and several of them (the file plugin's "exists", "stat", "mkdir", "remove") must ask the UI
// thread to resolve an app folder before they can finish. Each waits for the other and the app
// stops responding for good. Opening Settings did it: the page asks whether a backup exists
// while it is still loading.
//
// The fix: on the UI thread, never wait for the plugin store. When it is busy, the page-load
// notice is passed to another thread to deliver, and a navigation is simply allowed.
//
// This edits the copy of Tauri in Cargo's download folder, not the repository, so nothing here
// conflicts when the author's code is merged. If Tauri's code has changed and the edit no
// longer fits, it says so and leaves the source alone: the build still succeeds, without the fix.
//
// Cargo assumes downloaded crates never change and reuses what it compiled before, so this must
// run before the first compile of a build folder. The workflow's cache key ("kdpatch") keeps
// build folders from before the fix from being restored.
//
//   node build-tools/patch-tauri-android.mjs [--check] [--source <tauri crate folder>]
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"
import { fileURLToPath, pathToFileURL } from "node:url"

export const MARKER = "kd-patch: android ui-thread plugin store"

const PAGE_LOAD = {
  from: `          app_manager_
            .plugins
            .lock()
            .unwrap()
            .on_page_load(&w, &payload);
`,
  to: `          // ${MARKER}
          // This runs on the UI thread. A plugin command holding the store may be waiting for
          // this very thread, so never block here: another thread delivers it when it is busy.
          match app_manager_.plugins.try_lock() {
            Ok(mut plugins) => plugins.on_page_load(&w, &payload),
            Err(std::sync::TryLockError::WouldBlock) => {
              let manager = app_manager_.clone();
              let webview = w.clone();
              let url = url.clone();
              std::thread::spawn(move || {
                let payload = PageLoadPayload { url: &url, event };
                if let Ok(mut plugins) = manager.plugins.lock() {
                  plugins.on_page_load(&webview, &payload);
                }
              });
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {}
          }
`,
}

const NAVIGATION = {
  from: `        app_manager
          .plugins
          .lock()
          .expect("poisoned plugin store")
          .on_navigation(&w, url)
`,
  to: `        // ${MARKER}
        // Same UI thread, same store: when it is busy the navigation goes ahead unasked.
        match app_manager.plugins.try_lock() {
          Ok(mut plugins) => plugins.on_navigation(&w, url),
          Err(_) => true,
        }
`,
}

/** Pure: the patched text, or why it could not be patched. */
export function patchSource(original) {
  const text = original.replace(/\r\n/g, "\n")
  if (text.includes(MARKER)) return { status: "already", text: original }
  let patched = text
  for (const edit of [PAGE_LOAD, NAVIGATION]) {
    if (patched.split(edit.from).length !== 2) return { status: "mismatch", text: original }
    patched = patched.replace(edit.from, () => edit.to)
  }
  return { status: "patched", text: patched }
}

function tauriVersion(repoRoot) {
  const lock = readFileSync(join(repoRoot, "src-tauri", "Cargo.lock"), "utf8").replace(/\r\n/g, "\n")
  const match = /\[\[package\]\]\nname = "tauri"\nversion = "([^"]+)"/.exec(lock)
  return match ? match[1] : null
}

function findCrate(version) {
  const registry = join(process.env.CARGO_HOME || join(homedir(), ".cargo"), "registry", "src")
  if (!existsSync(registry)) return null
  for (const index of readdirSync(registry)) {
    const candidate = join(registry, index, `tauri-${version}`)
    if (existsSync(join(candidate, "src", "manager", "webview.rs"))) return candidate
  }
  return null
}

function main() {
  const args = process.argv.slice(2)
  const valueOf = (flag) => (args.includes(flag) ? args[args.indexOf(flag) + 1] : undefined)
  const repoRoot = fileURLToPath(new URL("..", import.meta.url))
  const version = tauriVersion(repoRoot)
  const crate = valueOf("--source") || (version ? findCrate(version) : null)
  const warn = (message) => console.log(`::warning::${message}`)
  if (!crate) {
    warn(`Tauri ${version || "(version unknown)"} is not in Cargo's download folder yet (run "cargo fetch" first); the Android freeze fix was NOT applied.`)
    return
  }
  const file = join(crate, "src", "manager", "webview.rs")
  const result = patchSource(readFileSync(file, "utf8"))
  if (result.status === "mismatch") {
    warn(`Tauri ${version} no longer contains the code this fix edits; the Android freeze fix was NOT applied. build-tools/patch-tauri-android.mjs needs updating.`)
    return
  }
  if (args.includes("--check")) {
    console.log(`Tauri ${version}: ${result.status === "already" ? "fix already in place" : "fix fits and would be applied"}`)
    return
  }
  if (result.status === "patched") writeFileSync(file, result.text)
  console.log(`Tauri ${version || ""}: Android freeze fix ${result.status === "already" ? "already in place" : "applied"} (${file})`)
}

const invokedAs = process.argv[1] ? pathToFileURL(process.argv[1]).href.toLowerCase() : ""
if (invokedAs && invokedAs === import.meta.url.toLowerCase()) main()
