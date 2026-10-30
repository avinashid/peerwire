// Raw deflate (RFC 1951) over this platform's CompressionStream and DecompressionStream, for doc frames between
// members (room.js) and a doc's pieces on the relays (relays.js). Inflating takes its input INFLATE_SLICE bytes at
// a time and stops past the limit it is given, so input that would inflate further (raw deflate inflates 1032 times
// at most) holds at most one slice's output past it.

const INFLATE_SLICE = 1024;

// Whether this platform deflates and inflates (CompressionStream and DecompressionStream with 'deflate-raw').
export const deflates = (() => {
  try {
    new CompressionStream('deflate-raw');
    new DecompressionStream('deflate-raw');
    return true;
  } catch {
    return false;
  }
})();

// A stream's chunks as one array, or null if they come to more than `max` bytes or the stream fails.
async function readAll(readable, max) {
  const reader = readable.getReader();
  const parts = [];
  let n = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      n += value.length;
      if (n > max) {
        reader.cancel().catch(() => {});
        return null;
      }
      parts.push(value);
    }
  } catch {
    return null;
  }
  const out = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

export async function deflate(bytes) {
  const cs = new CompressionStream('deflate-raw');
  const w = cs.writable.getWriter();
  w.write(bytes).catch(() => {});
  w.close().catch(() => {});
  return readAll(cs.readable, Infinity);
}

// `bytes` inflated, or null if it isn't raw deflate or inflates past `max` bytes.
export async function inflate(bytes, max) {
  const ds = new DecompressionStream('deflate-raw');
  const w = ds.writable.getWriter();
  (async () => {
    for (let i = 0; i < bytes.length; i += INFLATE_SLICE) await w.write(bytes.subarray(i, i + INFLATE_SLICE));
    await w.close();
  })().catch(() => {});
  const out = await readAll(ds.readable, max);
  if (!out) w.abort().catch(() => {});
  return out;
}
