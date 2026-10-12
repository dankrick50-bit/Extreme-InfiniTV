// Writes the Tauri config override for a fork build: fork branding, and updates that come from
// the fork's own builds, signed with the fork's own key, never from the original app's releases.
import { readFileSync, writeFileSync } from "node:fs"
import { releaseUrls } from "./build-info.mjs"

const root = new URL("../", import.meta.url)
const args = process.argv.slice(2)
const outPath = args[args.indexOf("--out") + 1]
if (!args.includes("--out") || !outPath) {
  console.error("usage: node build-tools/make-build-config.mjs --out <file> [--corepack]")
  process.exit(1)
}

const config = JSON.parse(readFileSync(new URL("src-tauri/tauri.conf.json", root), "utf8"))
const brand = JSON.parse(readFileSync(new URL("build-tools/brand.json", root), "utf8"))

const override = {
  productName: brand.productName,
  mainBinaryName: brand.binaryName,
  // Config merging replaces arrays whole, so the windows are copied from the live config with only the title changed.
  app: { windows: (config.app?.windows || []).map((window) => ({ ...window, title: brand.productName })) },
  // Signed update files can only be made where the private key is (the cloud build); a build
  // without it is still a normal installer that updates itself from the signed ones.
  bundle: { createUpdaterArtifacts: !!process.env.TAURI_SIGNING_PRIVATE_KEY },
}

if (brand.updaterPubkey && brand.updateRepo) {
  // With this key in place the app cannot install the original author's builds even by accident.
  override.plugins = { updater: { pubkey: brand.updaterPubkey, endpoints: [releaseUrls().feed] } }
}

if (args.includes("--corepack")) {
  const command = String(config.build?.beforeBuildCommand || "")
  override.build = { beforeBuildCommand: command.replace(/(?<!corepack )\bpnpm\b/g, "corepack pnpm") }
}

writeFileSync(outPath, JSON.stringify(override, null, 2))
console.log(`wrote ${outPath} for ${brand.productName}`)
