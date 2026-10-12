// Identifies a build of this fork so an installed app can tell whether a newer one exists.
//
// The original app's version number does not move between fork builds, so builds are ordered by
// the time of the commit they were made from instead. A local build and a cloud build of the same
// commit therefore count as the same build, and neither tries to replace the other.
import { execFileSync } from "node:child_process"
import { readFileSync } from "node:fs"
import { fileURLToPath } from "node:url"

// Plain address strings, so this also loads where `URL` is not Node's own (the test environment).
const here = (relative) => fileURLToPath(new URL(relative, import.meta.url).href)
const repoRoot = here("../")
const brand = JSON.parse(readFileSync(here("./brand.json"), "utf8"))

function git(args) {
  try {
    return execFileSync("git", args, { cwd: repoRoot, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim()
  } catch {
    return ""
  }
}

/** Where a release keeps its files; the tag is a rolling one, so these addresses never change. */
export function releaseUrls(repo = brand.updateRepo, tag = brand.updateTag) {
  const download = `https://github.com/${repo}/releases/download/${tag}`
  return { download, feed: `${download}/latest.json`, page: `https://github.com/${repo}/releases/tag/${tag}` }
}

/**
 * `stamp` is the commit time in Unix seconds, or 0 when it cannot be found (no git, no history);
 * an app built with 0 never offers or installs updates, since it cannot say what it is.
 */
export function buildInfo(env = process.env) {
  const stamp = Number(env.KD_BUILD_STAMP || git(["log", "-1", "--format=%ct"])) || 0
  const commit = env.KD_BUILD_COMMIT || git(["rev-parse", "--short=7", "HEAD"])
  return { stamp, commit, repo: brand.updateRepo || "", tag: brand.updateTag || "" }
}
