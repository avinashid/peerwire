// A shared document's copy on this device, for room.doc(name, { persist }) (doc.js loads and saves it).
//
// `persist` is true, for IndexedDB in a browser, or a store: { get(key), set(key, bytes), delete(key) }, each
// returning a promise, `bytes` a Uint8Array and get giving back what set stored (or undefined). memoryStore()
// keeps copies in this process, for Node and tests; a file store is a few lines over node:fs/promises (README).
//
// A copy is sealed under the room key (proof.js), so the storage shows neither the app, the room nor the doc,
// nor what it holds: only its size and when it was saved. It is kept under HMAC-SHA256(room key, 'pw-store-id'
// || doc name), in hex, as iv || AES-256-GCM(doc key, iv, the copy as JSON): the doc key is HMAC-SHA256(room
// key, 'pw-store-key' || doc name), so each key seals one record, and iv 12 random bytes, new for each save.
// Anyone who knows the roomId can read it, as they could join the room, so a guessable roomId keeps nothing
// secret. A copy of more than MAX_STORED bytes isn't read, and one that doesn't open or parse counts as none:
// the next save replaces it.

import { toHex } from './peerwire.js';
import { MESSAGE_BYTES, STORED_FRAMES } from './doc.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const IV_BYTES = 12;
const TAG_BYTES = 16;
// A copy is at most STORED_FRAMES frames of at most MESSAGE_BYTES each, and a few bytes around them.
export const MAX_STORED = STORED_FRAMES * MESSAGE_BYTES + 1024;

// Copies kept in this process: they go with it.
export function memoryStore() {
  const copies = new Map();
  return {
    get: async (key) => copies.get(key),
    set: async (key, bytes) => void copies.set(key, bytes.slice()),
    delete: async (key) => void copies.delete(key),
  };
}

// The page's IndexedDB store (database 'peerwire', object store 'docs'), made on first use.
let indexedDbStore = null;

export function browserStore() {
  if (!globalThis.indexedDB) throw new TypeError("persist: true needs IndexedDB; pass a store instead, e.g. memoryStore()");
  if (indexedDbStore) return indexedDbStore;
  let db = null;
  const open = () => (db ??= new Promise((resolve, reject) => {
    const r = indexedDB.open('peerwire', 1);
    r.onupgradeneeded = () => r.result.createObjectStore('docs');
    r.onsuccess = () => {
      // A newer version opened elsewhere: let it, and open again next time.
      r.result.onversionchange = () => {
        r.result.close();
        db = null;
      };
      resolve(r.result);
    };
    r.onerror = () => reject(r.error);
  }).catch((e) => {
    db = null;
    throw e;
  }));
  const run = async (mode, op) => {
    const d = await open();
    return new Promise((resolve, reject) => {
      const tx = d.transaction('docs', mode);
      const req = op(tx.objectStore('docs'));
      tx.oncomplete = () => resolve(req.result);
      tx.onabort = () => reject(tx.error ?? req.error ?? new Error('IndexedDB transaction aborted'));
    });
  };
  return (indexedDbStore = {
    get: (key) => run('readonly', (s) => s.get(key)),
    set: (key, bytes) => run('readwrite', (s) => s.put(bytes, key)),
    delete: (key) => run('readwrite', (s) => s.delete(key)),
  });
}

// A store as `persist` gives it, checked: true for the browser's.
export function storeOf(persist) {
  if (persist === true) return browserStore();
  if (['get', 'set', 'delete'].every((f) => typeof persist?.[f] === 'function')) return persist;
  throw new TypeError('persist is true or a store: { get(key), set(key, bytes), delete(key) }');
}

// What each store does for each doc name, in the order asked (a doc closed and opened again at once loads what
// the closing one saved): store -> name -> the last operation.
const queues = new WeakMap();

function queued(store, name, op) {
  let q = queues.get(store);
  if (!q) queues.set(store, (q = new Map()));
  const run = (q.get(name) ?? Promise.resolve()).then(op);
  const last = run.catch(() => {});
  q.set(name, last);
  last.then(() => q.get(name) === last && q.delete(name));
  return run;
}

// The copy of doc `name` in `store`, sealed under the room key of `proof` (a RoomProof): the store doc.js takes,
// { load, save, forget }, each run after the one called before it (see queued). save() takes the copy or a
// promise of it (null: nothing to save), so a doc that closes while its copy loads keeps its turn.
export function docCopy(store, proof, name) {
  let keys = null;
  const derive = () => (keys ??= (async () => {
    const n = enc.encode(name);
    const id = toHex(await proof.mac('pw-store-id', [n]));
    const key = await crypto.subtle.importKey('raw', await proof.mac('pw-store-key', [n]), 'AES-GCM', false, ['encrypt', 'decrypt']);
    return { id, key };
  })());
  return {
    load: () => queued(store, name, async () => {
      const { id, key } = await derive();
      const got = await store.get(id);
      const bytes = got instanceof Uint8Array ? got : got instanceof ArrayBuffer ? new Uint8Array(got) : null;
      if (!bytes || bytes.length < IV_BYTES + TAG_BYTES || bytes.length > MAX_STORED) return null;
      try {
        const text = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: bytes.subarray(0, IV_BYTES) }, key, bytes.subarray(IV_BYTES));
        return JSON.parse(dec.decode(text));
      } catch {
        return null;
      }
    }),
    save: (copy) => queued(store, name, async () => {
      const c = await copy;
      if (c == null) return;
      const { id, key } = await derive();
      const iv = crypto.getRandomValues(new Uint8Array(IV_BYTES));
      const sealed = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, key, enc.encode(JSON.stringify(c))));
      const bytes = new Uint8Array(IV_BYTES + sealed.length);
      bytes.set(iv);
      bytes.set(sealed, IV_BYTES);
      await store.set(id, bytes);
    }),
    forget: () => queued(store, name, async () => {
      const { id } = await derive();
      await store.delete(id);
    }),
  };
}
