// The chat demo's QR encoder (demo/qr.js) against known answers. They were taken from it once it matched the
// Arase encoder that npm vendors (qrcode-terminal) module for module: every version 1 to 40, level and mask,
// at full and half capacity, except 15-H, where that encoder's block table lacks the standard's 7 x (37, 13).
//
//   node packages/js/test/qr.mjs

import { createHash } from 'node:crypto';
import { qrCode } from '../demo/qr.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const flat = (q) => q.modules.map((r) => r.map((d) => (d ? '1' : '0')).join('')).join('');

const cases = [
  // An invite link: version 4.
  ['https://peerwire.example/demo/chat.html#3q2-7wE1fYk0pX9QZr8lAg', 'M', 33, 4, '0b9b67811ea9e9779f17331b7cf85d117f9d0f3f2ac153f6a5f537500438f052'],
  // Version 11: a 16-bit length and the version information blocks.
  [Array.from({ length: 300 }, (_, i) => String.fromCharCode(33 + ((i * 37) % 94))).join(''), 'L', 61, 3, '0682891b7140032e1d1a7c3950bc18dc3e99fb9f261f2668161555a73bc224eb'],
  // UTF-8 bytes.
  ['héllo wörld ✓', 'H', 29, 5, 'a444f4792a7b639491a5334f9f7be6be775676aef76e6e1d8256db303b5b5256'],
];
for (const [text, ecl, size, mask, sha] of cases) {
  const h = createHash('sha256');
  for (let m = 0; m < 8; m++) h.update(flat(qrCode(text, { ecl, mask: m })));
  check(h.digest('hex') === sha, `${ecl}, ${text.length} chars: all 8 masks match the known answer`);
  const auto = qrCode(text, { ecl });
  check(auto.size === size, `${ecl}, ${text.length} chars: ${auto.size} modules a side (want ${size})`);
  check(flat(auto) === flat(qrCode(text, { ecl, mask })), `${ecl}, ${text.length} chars: the lowest penalty is mask ${mask}`);
}

let threw = null;
try { qrCode('x'.repeat(2954), { ecl: 'L' }); } catch (e) { threw = e; }
check(threw instanceof RangeError, 'more than version 40-L holds (2953 bytes) throws RangeError');
check(qrCode('x'.repeat(2953), { ecl: 'L' }).size === 177, '2953 bytes fit version 40-L');
threw = null;
try { qrCode('x', { ecl: 'X' }); } catch (e) { threw = e; }
check(threw instanceof TypeError, 'an unknown level throws TypeError');

console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
