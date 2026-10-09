// Small pure decisions of the worker. No I/O, no page data in any message.

// How long an op may wait for a place: the smaller of its deadline less 1000 ms and `maxMs`,
// never below 0. The true answer (`at-capacity`) must arrive before the runner's own timer.
export const waitBound = (deadlineMs, maxMs) => Math.min(Math.max(0, deadlineMs - 1000), maxMs)

// The ids of ended runs: a bounded set, the oldest id out first.
export function createEnded(max = 1000) {
  const set = new Set()
  return {
    add(run) {
      set.delete(run)
      set.add(run)
      if (set.size > max) set.delete(set.values().next().value)
    },
    has: (run) => set.has(run),
    size: () => set.size,
  }
}

const esc = (v) => v.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

// Returns a function that replaces every cookie value in a string with `[cookie]`.
// Values of 4+ characters go anywhere; 1-3 characters only as whole tokens, so a short value does not blank ordinary words.
export function scrubber(values) {
  const vals = values.filter((v) => v.length >= 1).sort((x, y) => y.length - x.length)
  const one = (t, c) => {
    if (c.length >= 4) return t.split(c).join('[cookie]')
    const re = new RegExp(`(^|[^A-Za-z0-9_])${esc(c)}($|[^A-Za-z0-9_])`, 'g')
    // two passes: adjacent matches share their boundary character
    return t.replace(re, '$1[cookie]$2').replace(re, '$1[cookie]$2')
  }
  return (v) => (typeof v === 'string' ? vals.reduce(one, v) : v)
}

// Fail closed. `cookies` is the list of cookie values, or null when it could not be read:
// then there is no page data at all (null). Else url, title and reads, scrubbed.
export function scrubPage({ url, title, reads }, cookies) {
  if (cookies === null) return null
  const scrub = scrubber(cookies)
  const out = {}
  for (const k of Object.keys(reads)) out[k] = scrub(reads[k])
  return { url: scrub(url), title: scrub(title), reads: out }
}
