// Swaps the upstream display name for the fork's at build time, so upstream text changes keep merging cleanly.
import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

const brand = JSON.parse(readFileSync(new URL("./brand.json", import.meta.url), "utf8"))
const srcRoot = fileURLToPath(new URL("../src/", import.meta.url)).replace(/\\/g, "/")

// The logo wordmark is split across two elements ("Extreme Infini" + "TV"), so a plain search misses it.
const WORDMARK = /(?:Extreme(?:&nbsp;|\s))?Infini(<\/span>\s*<span[^>]*>|<span[^>]*>)TV(?=<\/span>)/g

export function applyBrand(code) {
  return code
    .replace(WORDMARK, (_match, joint) => `${brand.wordmark[0]}${joint}${brand.wordmark[1]}`)
    .split(brand.replaces)
    .join(brand.productName)
}

export function brandPlugin() {
  return {
    name: "fork-brand",
    enforce: "pre",
    transform(code, id) {
      const file = id.split("?")[0].replace(/\\/g, "/")
      if (!file.startsWith(srcRoot) || !/\.(astro|svelte|ts|js|mjs|json)$/.test(file)) return null
      if (!code.includes("Infini")) return null
      const next = applyBrand(code)
      return next === code ? null : { code: next, map: null }
    },
  }
}
