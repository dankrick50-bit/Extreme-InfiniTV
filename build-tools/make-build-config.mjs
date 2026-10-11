// Writes the Tauri config override for a fork build: fork branding, and no updater artifacts
// (those need the author's private signing key).
import { readFileSync, writeFileSync } from "node:fs"

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
  bundle: { createUpdaterArtifacts: false },
}

if (args.includes("--corepack")) {
  const command = String(config.build?.beforeBuildCommand || "")
  override.build = { beforeBuildCommand: command.replace(/(?<!corepack )\bpnpm\b/g, "corepack pnpm") }
}

writeFileSync(outPath, JSON.stringify(override, null, 2))
console.log(`wrote ${outPath} for ${brand.productName}`)
