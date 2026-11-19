// A whole file's SHA-256, off the page's main thread (share.js): { t: 'update', data } with each piece in order (an
// ArrayBuffer), then { t: 'digest' }, answered with { t: 'digest', hex }.
import { Sha256, toHex } from './files.js';

const hash = new Sha256();
self.onmessage = ({ data: m }) => {
  if (m?.t === 'update') hash.update(new Uint8Array(m.data));
  else if (m?.t === 'digest') self.postMessage({ t: 'digest', hex: toHex(hash.digest()) });
};
