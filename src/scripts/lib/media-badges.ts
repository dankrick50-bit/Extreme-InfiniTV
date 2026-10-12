// Short "what is in this file" labels for a film, read from the provider's own details
// (get_vod_info carries the first video and audio stream as ffprobe describes them).
// They say what the stream carries, not what a given device will manage to play.

function record(value: unknown): Record<string, unknown> | null {
  // Some panels send an empty array where the stream description should be.
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  return value as Record<string, unknown>
}

function text(value: unknown): string {
  return typeof value === "string" ? value.trim() : ""
}

function channelLabel(channels: number, layout: string): string {
  if (/^7\.1/.test(layout) || channels === 8) return "7.1"
  if (/^5\.1/.test(layout) || channels === 6) return "5.1"
  if (channels === 2) return "Stereo"
  if (channels === 1) return "Mono"
  return channels > 0 ? `${channels} ch` : ""
}

const AUDIO_CODECS: Record<string, string> = {
  ac3: "Dolby Digital",
  eac3: "Dolby Digital+",
  truehd: "Dolby TrueHD",
  dts: "DTS",
  aac: "AAC",
  mp3: "MP3",
  mp2: "MP2",
  opus: "Opus",
  flac: "FLAC",
  vorbis: "Vorbis",
}

/** "Dolby Atmos", "Dolby Digital+ 5.1", "AAC Stereo"; null when the provider said nothing usable. */
export function describeAudio(raw: unknown): string | null {
  const audio = record(raw)
  if (!audio) return null
  const codec = text(audio.codec_name).toLowerCase()
  const profile = text(audio.profile)
  const channels = channelLabel(Number(audio.channels) || 0, text(audio.channel_layout))
  // Atmos travels inside Dolby Digital+ or TrueHD; the profile is the only place it is named.
  if (/atmos/i.test(profile)) return "Dolby Atmos"
  const name = /^dts/i.test(profile) ? profile : AUDIO_CODECS[codec] || codec.toUpperCase()
  const label = [name, channels].filter(Boolean).join(" ")
  return label || null
}

const VIDEO_CODECS: Record<string, string> = {
  hevc: "HEVC",
  h265: "HEVC",
  h264: "H.264",
  av1: "AV1",
  vp9: "VP9",
  mpeg2video: "MPEG-2",
  mpeg4: "MPEG-4",
}

function resolutionLabel(width: number, height: number): string {
  // Widescreen films are cropped in height, so the width decides as much as the height does.
  if (width >= 3800 || height >= 2000) return "4K"
  if (width >= 2500 || height >= 1400) return "1440p"
  if (width >= 1900 || height >= 1000) return "1080p"
  if (width >= 1200 || height >= 700) return "720p"
  return height > 0 ? `${height}p` : ""
}

/** "4K HEVC HDR", "1080p H.264"; null when the provider said nothing usable. */
export function describeVideo(raw: unknown): string | null {
  const video = record(raw)
  if (!video) return null
  const resolution = resolutionLabel(Number(video.width) || 0, Number(video.height) || 0)
  const codec = VIDEO_CODECS[text(video.codec_name).toLowerCase()] || ""
  const transfer = text(video.color_transfer).toLowerCase()
  const hdr = transfer === "smpte2084" ? "HDR" : transfer === "arib-std-b67" ? "HLG" : ""
  const label = [resolution, codec, hdr].filter(Boolean).join(" ")
  return label || null
}

/** Picture first, then sound; empty when the provider gave no stream details. */
export function mediaBadges(info: unknown): string[] {
  const details = record(info)
  if (!details) return []
  return [describeVideo(details.video), describeAudio(details.audio)].filter((label): label is string => !!label)
}
