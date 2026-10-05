export function bitrate(bitsPerSecond: number | null | undefined): string {
  if (bitsPerSecond == null || !Number.isFinite(bitsPerSecond)) return "—";
  if (bitsPerSecond >= 1e9) return `${(bitsPerSecond / 1e9).toFixed(2)} Gb/s`;
  if (bitsPerSecond >= 1e6) return `${(bitsPerSecond / 1e6).toFixed(bitsPerSecond >= 1e8 ? 0 : 1)} Mb/s`;
  if (bitsPerSecond >= 1e3) return `${(bitsPerSecond / 1e3).toFixed(0)} kb/s`;
  return `${bitsPerSecond.toFixed(0)} b/s`;
}

export function micros(us: number | null | undefined): string {
  if (us == null || !Number.isFinite(us)) return "—";
  if (us >= 1000) return `${(us / 1000).toFixed(us >= 10_000 ? 1 : 2)} ms`;
  return `${us.toFixed(0)} µs`;
}

export function duration(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

export function relative(ms: number | null | undefined, now = Date.now()): string {
  if (ms == null) return "never";
  const seconds = Math.round((now - ms) / 1000);
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds} s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours} h ago`;
  return new Date(ms).toLocaleDateString();
}

/// `a1b2c3d4…` grouped for reading aloud and comparison.
export function fingerprint(hex: string | null | undefined, groups = 8): string {
  if (!hex) return "unknown";
  const parts = hex.toUpperCase().match(/.{1,4}/g) ?? [];
  return parts.slice(0, groups).join(" ") + (parts.length > groups ? " …" : "");
}

export function channels(count: number): string {
  switch (count) {
    case 2:
      return "Stereo";
    case 6:
      return "5.1 surround";
    case 8:
      return "7.1 surround";
    default:
      return `${count} channels`;
  }
}
