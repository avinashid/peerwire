// Peerwire Share's service worker: a file streamed to the browser's downloads as it arrives, for browsers without
// showSaveFilePicker (Firefox, Safari, Chrome on Android). share.js registers it with the scope dl/ beside it, so it
// serves only download URLs and never the page. A classic script (no imports), which every browser with service
// workers runs.
//
// The page posts { t: 'download', key, disposition, size } with a MessagePort, then opens dl/<key>/<name> in a
// hidden iframe. That one request (the key is 128 random bits, and is served once) gets a Response whose body is a
// stream fed from the port: the page sends { t: 'chunk', data } once for each { t: 'pull' } the stream asks for (a
// few chunks queued at most, so the file is never in memory), then { t: 'end' }, or { t: 'abort' } (the download
// fails, and the browser drops what it had). The page pings the worker while a download runs, which keeps it alive.
// `disposition` is the Content-Disposition the page made (files.js disposition()): printable ASCII only, or no
// download.

const QUEUED = 4; // chunks the stream holds before it stops asking the page for more
const WAIT_MS = 60000; // a download the page set up but never opened is forgotten after this
const downloads = new Map(); // key -> { disposition, size, stream, timer }

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

self.addEventListener('message', (e) => {
  const m = e.data;
  const port = e.ports[0];
  if (m?.t !== 'download' || !port || typeof m.key !== 'string' || !/^[0-9a-f]{32}$/.test(m.key)) return;
  if (typeof m.disposition !== 'string' || !/^[\x20-\x7e]{1,2048}$/.test(m.disposition)) return;
  let ctrl;
  const stream = new ReadableStream({
    start(c) {
      ctrl = c;
    },
    pull() {
      port.postMessage({ t: 'pull' });
    },
    cancel() {
      // The download was cancelled in the browser.
      port.postMessage({ t: 'cancelled' });
    },
  }, new CountQueuingStrategy({ highWaterMark: QUEUED }));
  port.onmessage = ({ data: d }) => {
    try {
      if (d?.t === 'chunk' && d.data instanceof ArrayBuffer) ctrl.enqueue(new Uint8Array(d.data));
      else if (d?.t === 'end') ctrl.close();
      else if (d?.t === 'abort') ctrl.error(new Error('cancelled'));
    } catch {
      // The stream is closed already.
    }
  };
  const size = Number.isSafeInteger(m.size) && m.size >= 0 ? m.size : null;
  const timer = setTimeout(() => downloads.delete(m.key), WAIT_MS);
  downloads.set(m.key, { disposition: m.disposition, size, stream, timer });
  port.postMessage({ t: 'ready' });
});

self.addEventListener('fetch', (e) => {
  const m = new URL(e.request.url).pathname.match(/\/dl\/([0-9a-f]{32})\/[^/]*$/);
  const d = m && downloads.get(m[1]);
  if (!d) return;
  downloads.delete(m[1]);
  clearTimeout(d.timer);
  const headers = {
    'content-type': 'application/octet-stream',
    'content-disposition': d.disposition,
    'x-content-type-options': 'nosniff',
    'content-security-policy': "default-src 'none'; sandbox",
  };
  if (d.size !== null) headers['content-length'] = String(d.size);
  e.respondWith(new Response(d.stream, { headers }));
});
