#!/usr/bin/env node
// The @peerwire/js command line. One command so far:
//
//   npx @peerwire/js doctor [--json] [--redact] [--stun host:port]... [--stun5780 host:port] [--relay url]...
//
// prints what this network does to peer-to-peer traffic (see doctor.js); nothing is uploaded.
import { doctor, render } from './doctor.js';

const USAGE = 'usage: peerwire doctor [--json] [--redact] [--stun host:port]... [--stun5780 host:port] [--relay url]...';
const fail = (why) => {
  console.error(why ? `${why}\n${USAGE}` : USAGE);
  process.exit(why === undefined ? 0 : 1);
};

const [command, ...args] = process.argv.slice(2);
if (command === '--help' || command === '-h') fail();
if (command !== 'doctor') fail(command === undefined ? 'peerwire: no command' : `peerwire: unknown command ${command}`);
const options = { stun: [], relays: [] };
let json = false;
for (let i = 0; i < args.length; i++) {
  const value = () => (i + 1 < args.length ? args[++i] : fail(`${args[i]} needs a value`));
  switch (args[i]) {
    case '--json': json = true; break;
    case '--redact': options.redact = true; break;
    case '--stun': options.stun.push(value()); break;
    case '--stun5780': options.stun5780 = value(); break;
    case '--relay': options.relays.push(value()); break;
    case '--help': case '-h': fail(); break;
    default: fail(`unknown option ${args[i]}`);
  }
}
if (!options.stun.length) delete options.stun;
if (!options.relays.length) delete options.relays;
const report = await doctor(options, (step) => console.error(`doctor: ${step}`));
// Out, then exit: a closed relay connection may still be winding down.
process.stdout.write(json ? `${JSON.stringify(report, null, 2)}\n` : render(report), () => process.exit(0));
