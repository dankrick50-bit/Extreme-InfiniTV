// Writes latest.json, the file installed apps read to find out whether a newer build exists.
//
// It is the Tauri updater's own format (Windows and Linux AppImage install straight from it),
// with a "kd" section added for everything else: the build's identity and where each platform's
// installer is. Signature files are folded into it and then removed, so they are not published
// as loose files beside the installers.
import { readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { fileURLToPath, pathToFileURL } from "node:url"
import { buildInfo, releaseUrls } from "./build-info.mjs"

/** Installer file name ending -> the updater's platform keys it serves and the app-side asset name. */
const INSTALLERS = [
  { ending: "-windows-x64-setup.exe", asset: "windows", targets: ["windows-x86_64-nsis", "windows-x86_64"] },
  { ending: "-linux-x86_64.AppImage", asset: "appimage", targets: ["linux-x86_64-appimage"] },
  { ending: "-ubuntu-amd64.deb", asset: "deb", targets: ["linux-x86_64-deb"] },
  { ending: "-macos-universal.dmg", asset: "macos", targets: [] },
  { ending: "-android-fireos.apk", asset: "android", targets: [] },
]

/**
 * Pure: builds the feed from the files of one release. `signatures` maps a file name to the
 * contents of its .sig; a platform without a signature gets no updater entry, so an app there is
 * only told about the build instead of being handed something it could not verify.
 */
export function makeUpdateFeed({ files, signatures, version, build, urls }) {
  const platforms = {}
  const assets = {}
  for (const installer of INSTALLERS) {
    const file = files.find((name) => name.endsWith(installer.ending))
    if (!file) continue
    const url = `${urls.download}/${file}`
    assets[installer.asset] = url
    const signature = signatures[file]
    if (!signature) continue
    for (const target of installer.targets) platforms[target] = { signature, url }
  }
  return {
    version,
    notes: `Build ${build.commit || "unknown"}`,
    pub_date: new Date(build.stamp * 1000).toISOString().replace(".000Z", "Z"),
    platforms,
    kd: { build: build.stamp, commit: build.commit, page: urls.page, assets },
  }
}

function main() {
  const args = process.argv.slice(2)
  const valueOf = (flag) => (args.includes(flag) ? args[args.indexOf(flag) + 1] : undefined)
  const dir = valueOf("--dir")
  if (!dir) {
    console.error("usage: node build-tools/make-update-feed.mjs --dir <folder with the installers> [--repo owner/name] [--tag name]")
    process.exit(1)
  }

  const build = buildInfo()
  if (!build.stamp) {
    console.error("cannot tell which commit this is, so the feed would never offer an update")
    process.exit(1)
  }
  const urls = releaseUrls(valueOf("--repo") || build.repo, valueOf("--tag") || build.tag)
  const everything = readdirSync(dir)
  const signatureFiles = everything.filter((name) => name.endsWith(".sig"))
  const signatures = Object.fromEntries(
    signatureFiles.map((name) => [name.slice(0, -".sig".length), readFileSync(join(dir, name), "utf8").trim()])
  )
  const files = everything.filter((name) => !name.endsWith(".sig") && name !== "latest.json")
  const version = JSON.parse(readFileSync(fileURLToPath(new URL("../package.json", import.meta.url).href), "utf8")).version

  const feed = makeUpdateFeed({ files, signatures, version, build, urls })
  writeFileSync(join(dir, "latest.json"), JSON.stringify(feed, null, 2) + "\n")
  for (const name of signatureFiles) rmSync(join(dir, name))
  console.log(`wrote latest.json for build ${build.commit} (${feed.pub_date}); self-installing on: ${Object.keys(feed.platforms).join(", ") || "none"}`)
}

// Only when run as a script; the tests import makeUpdateFeed without running it.
// (Compared without regard to case: Windows may spell the drive letter either way.)
const invokedAs = process.argv[1] ? pathToFileURL(process.argv[1]).href.toLowerCase() : ""
if (invokedAs && invokedAs === import.meta.url.toLowerCase()) main()
