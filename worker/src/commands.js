// Executes one command of a browser.page op. Closed set. No JS from the request is ever run.
// The only page.evaluate is the fixed built-in function for `overflow-x`.
const MAX_TEXT_BYTES = 8 * 1024

export class CommandError extends Error {
  constructor(code) {
    super(code)
    this.code = code
  }
}

export function locate(page, t) {
  const exact = t.exact === true ? true : undefined
  let loc
  if (t.role !== undefined) loc = page.getByRole(t.role, { name: t.name, exact })
  else if (t.label !== undefined) loc = page.getByLabel(t.label, { exact })
  else if (t.text !== undefined) loc = page.getByText(t.text, { exact })
  else if (t['test-id'] !== undefined) loc = page.getByTestId(t['test-id'])
  else loc = page.locator('css=' + t.css) // forced css engine: no text=/xpath=/js= prefixes
  return loc
}

// One element: nth when given, else the first match.
const one = (page, t) => locate(page, t).nth(t.nth ?? 0)

export const capText = (s) => {
  const b = Buffer.from(s, 'utf8')
  return b.length <= MAX_TEXT_BYTES ? s : b.subarray(0, MAX_TEXT_BYTES).toString('utf8').replace(/�+$/, '')
}

export const pathOf = (u) => {
  try {
    const x = new URL(u)
    return x.protocol === 'http:' || x.protocol === 'https:' ? x.pathname + x.search : ''
  } catch {
    return ''
  }
}

// Turn a Playwright failure into one of the fixed codes.
export async function classify(e, cmd, page) {
  if (e instanceof CommandError) return e.code
  const msg = String(e?.message ?? '')
  if (/detached|has been closed|Target closed|Frame was detached/i.test(msg)) return 'detached'
  if (cmd.do === 'goto') return 'navigation-failed'
  if (e?.name === 'TimeoutError' && cmd.target) {
    try {
      const loc = one(page, cmd.target)
      if ((await locate(page, cmd.target).count()) === 0) return 'not-found'
      if (cmd.do !== 'wait-for' && !(await loc.isVisible())) return 'not-visible'
    } catch { /* fall through */ }
    return 'timeout'
  }
  if (e?.name === 'TimeoutError') return msg.includes('net::') ? 'navigation-failed' : 'timeout'
  if (/net::/.test(msg)) return 'navigation-failed'
  return cmd.target ? 'not-found' : 'timeout'
}

async function read(page, cmd, st) {
  const { what, target } = cmd
  let v
  if (what === 'url') v = pathOf(page.url())
  else if (what === 'title') v = await page.title()
  else if (what === 'overflow-x') {
    v = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth)
  } else {
    if (!target) throw new CommandError('not-found')
    if (what === 'count') v = await locate(page, target).count()
    else if (what === 'visible') v = await one(page, target).isVisible()
    else if (what === 'text') v = capText(await one(page, target).innerText())
    else if (what === 'value') v = capText(await one(page, target).inputValue())
    else {
      const a = await one(page, target).getAttribute(what.slice(5))
      v = a === null ? null : capText(a)
    }
  }
  st.reads[cmd.as] = v
}

export async function runCommand(page, cmd, st) {
  const t = cmd.target
  switch (cmd.do) {
    case 'goto': {
      // A path only. `//host` and backslashes would change the host: refuse.
      if (cmd.path.startsWith('//') || cmd.path.includes('\\')) throw new CommandError('host-not-allowed')
      const url = new URL(cmd.path, st.baseUrl)
      if (url.origin !== st.baseOrigin) throw new CommandError('host-not-allowed')
      const r = await page.goto(url.href, { waitUntil: 'load' })
      if (r) st.status = r.status()
      return
    }
    case 'click': return one(page, t).click()
    case 'fill': return one(page, t).fill(cmd.value)
    case 'press-seq': return one(page, t).pressSequentially(cmd.value)
    case 'select': await one(page, t).selectOption(cmd.value); return
    case 'check': return one(page, t).check()
    case 'uncheck': return one(page, t).uncheck()
    case 'press': return one(page, t).press(cmd.key)
    case 'wait-for':
      if (cmd.url !== undefined) {
        const prefix = cmd.url
        return page.waitForURL((u) => (u.pathname + u.search).startsWith(prefix))
      }
      return one(page, t).waitFor({ state: cmd.state })
    case 'read': return read(page, cmd, st)
    case 'viewport': return page.setViewportSize({ width: cmd.width, height: cmd.height })
    default: throw new CommandError('not-found')
  }
}
