/// Numeric URL patterns, the way a download manager has always spelled a
/// batch: `http://host/img[001-050].jpg` is fifty links, not one.
///
/// Zero padding comes from the width the user wrote — `[001-050]` pads to
/// three digits, `[1-50]` pads to none — because that is what the server's
/// filenames look like.

/// Cap on one pattern's expansion. `[1-99999999]` is a typo, not a request,
/// and queuing it would be indistinguishable from the app hanging.
export const MAX_EXPANSION = 1000;

const PATTERN = /\[(\d+)-(\d+)\]/;

/// Whether `url` carries a `[start-end]` range worth expanding.
export function hasPattern(url: string): boolean {
  return PATTERN.test(url);
}

/// Expand the first `[start-end]` range in `url`. Anything without one comes
/// back as a single-item list, so callers can run every URL through this.
///
/// A descending range (`[10-1]`) counts down, which is the only sensible
/// reading of it. Expansion past `MAX_EXPANSION` throws rather than silently
/// truncating: a batch missing most of its files is worse than an error.
export function expandPattern(url: string): string[] {
  const match = url.match(PATTERN);
  if (!match) return [url];

  const [token, rawStart, rawEnd] = match;
  const start = Number(rawStart);
  const end = Number(rawEnd);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return [url];

  const count = Math.abs(end - start) + 1;
  if (count > MAX_EXPANSION) {
    throw new Error(
      `${token} expands to ${count} links; the limit is ${MAX_EXPANSION}.`,
    );
  }

  // The literal width the user typed, so "007" stays three digits wide.
  const width = Math.max(rawStart.length, rawEnd.length);
  const step = end >= start ? 1 : -1;
  const out: string[] = [];
  for (let n = start; step > 0 ? n <= end : n >= end; n += step) {
    out.push(url.replace(token, String(n).padStart(width, "0")));
  }
  return out;
}

/// Expand every URL in a list, keeping the order the user wrote them in.
export function expandAll(urls: string[]): string[] {
  return urls.flatMap(expandPattern);
}
