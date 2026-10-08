import http from 'node:http'
import { createHash } from 'node:crypto'

const page = (title, body) => `<!doctype html><html><head><title>${title}</title></head><body>${body}</body></html>`

const PAGES = {
  '/': page('Home', '<h1>Home</h1><a id="lnk" href="/next" data-x="42">Next page</a><ul><li>a</li><li>b</li><li>c</li></ul>'),
  '/next': page('Next', '<h1>Next</h1>'),
  '/form': page('Form', `
    <label for="e">Email</label><input id="e" name="email">
    <button id="save" onclick="document.getElementById('out').textContent='saved:'+document.getElementById('e').value">Save</button>
    <p id="out"></p>
    <select id="s" aria-label="Color"><option value="r">Red</option><option value="b">Blue</option></select>
    <input type="checkbox" id="c" aria-label="Agree">
    <input id="k" aria-label="Keys" onkeydown="if(event.key==='Enter')document.getElementById('keyed').textContent='enter'">
    <p id="keyed"></p>
    <input id="seq" aria-label="Seq">
    <span data-testid="tid">by test id</span>
    <span id="late" style="display:none">late</span>
    <span id="gone">gone soon</span>
    <script>
      setTimeout(()=>{document.getElementById('late').style.display='inline'},300);
      setTimeout(()=>{document.getElementById('gone').remove()},300);
    </script>`),
  '/hidden': page('Hidden', '<button id="hb" style="display:none">Hidden button</button>'),
  '/wide': page('Wide', '<div style="width:3000px;height:10px;background:red"></div>'),
  '/narrow': page('Narrow', '<p>short</p>'),
  '/big': page('Big', `<p id="big">${'x'.repeat(20000)}</p>`),
}

// Tiny site on 127.0.0.1. `/cookie-set` sets a cookie, `/cookie-get` echoes the Cookie header.
export async function startSite() {
  const server = http.createServer((req, res) => {
    const [path, qs] = req.url.split('?')
    const o = new URLSearchParams(qs ?? '').get('o') ?? ''
    const q = new URLSearchParams(qs ?? '').get('to') ?? ''
    const set = (js) => page('Probe', `<p id="r"></p><script>const r=document.getElementById('r');${js}</script>`)
    const PROBES = {
      '/x-fetch': set(`fetch(${JSON.stringify(o + '/ok')}).then(()=>r.textContent='ok',()=>r.textContent='err')`),
      '/x-iframe': set(`const f=document.createElement('iframe');f.onload=()=>r.textContent='loaded';f.src=${JSON.stringify(o + '/ok')};document.body.append(f)`),
      '/x-img': set(`const i=new Image();i.onload=()=>r.textContent='ok';i.onerror=()=>r.textContent='err';i.src=${JSON.stringify(o + '/img.gif')}`),
      '/x-imgredir': set(`const i=new Image();i.onload=()=>r.textContent='ok';i.onerror=()=>r.textContent='err';i.src='/redir?to='+encodeURIComponent(${JSON.stringify(o + '/img.gif')})`),
      '/x-link': set(`r.innerHTML='<a href="${o}/">off</a>'`),
      '/x-ws': set(`const w=new WebSocket(${JSON.stringify(o.replace(/^http/, 'ws') + '/ws')});w.onopen=()=>r.textContent='open';w.onerror=w.onclose=()=>{if(!r.textContent)r.textContent='closed'}`),
      '/x-cookie-dom': set(`r.textContent=document.cookie`),
      '/x-cookie-short': set(`r.textContent='ab cab ab, ab'`),
    }
    res.setHeader('access-control-allow-origin', '*')
    if (path === '/ok') { res.writeHead(200, { 'content-type': 'text/plain' }); return res.end('ok') }
    if (path === '/img.gif') {
      res.writeHead(200, { 'content-type': 'image/gif' })
      return res.end(Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64'))
    }
    if (path === '/redir') { res.writeHead(302, { location: q }); return res.end() }
    if (PROBES[path]) {
      const h = { 'content-type': 'text/html' }
      if (path === '/x-cookie-dom') h['set-cookie'] = 'sid=abc123; Path=/'
      if (path === '/x-cookie-short') h['set-cookie'] = 'k=ab; Path=/'
      res.writeHead(200, h)
      return res.end(PROBES[path])
    }
    if (path === '/cookie-set') {
      res.writeHead(200, { 'content-type': 'text/html', 'set-cookie': 'sid=abc123; Path=/' })
      return res.end(page('Set', '<p>set</p>'))
    }
    if (path === '/cookie-get') {
      res.writeHead(200, { 'content-type': 'text/html' })
      return res.end(page('Get', `<p id="c">cookie:${req.headers.cookie ?? 'none'}</p>`))
    }
    if (path === '/missing') {
      res.writeHead(404, { 'content-type': 'text/html' })
      return res.end(page('Missing', '<p>nope</p>'))
    }
    const body = PAGES[path]
    if (!body) {
      res.writeHead(404)
      return res.end()
    }
    res.writeHead(200, { 'content-type': 'text/html' })
    res.end(body)
  })
  // Minimal WebSocket accept: enough for the handshake to succeed (no frames).
  server.on('upgrade', (req, sock) => {
    const key = req.headers['sec-websocket-key']
    const acc = createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64')
    sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${acc}\r\n\r\n`)
  })
  await new Promise((r) => server.listen(0, '127.0.0.1', r))
  const origin = `http://127.0.0.1:${server.address().port}`
  return { origin, close: () => new Promise((r) => { server.closeAllConnections?.(); server.close(r) }) }
}
