// Serves `root` over loopback for the browser tests: `python3 -m http.server` on a port
// the OS picks, resolved once it listens. Python prints "Serving HTTP on … port N" only
// after it has bound and is listening, so no fixed wait is needed however loaded the
// host is, and tests run side by side never pick the same port (NB-059).
//
//   const { port, server } = await serve(root);   // …; server.kill() when done

import { spawn } from 'node:child_process';

export const SERVE_TIMEOUT_MS = 10000;

export function serve(root, { timeoutMs = SERVE_TIMEOUT_MS } = {}) {
  const server = spawn('python3', ['-u', '-m', 'http.server', '0', '--bind', '127.0.0.1'],
    { cwd: root, stdio: ['ignore', 'pipe', 'ignore'] });
  return new Promise((resolve, reject) => {
    let out = '';
    let done = false;
    const fail = (why) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      server.kill();
      reject(new Error(`http.server for ${root}: ${why}`));
    };
    const timer = setTimeout(() => fail(`not listening after ${timeoutMs} ms`), timeoutMs);
    server.on('error', (e) => fail(e.message));
    server.on('exit', (code, signal) => fail(`exited (${signal ?? code}) before listening`));
    server.stdout.setEncoding('utf8');
    server.stdout.on('data', function listening(chunk) {
      out += chunk;
      const m = out.match(/ port (\d+) /);
      if (!m || done) return;
      done = true;
      clearTimeout(timer);
      server.stdout.off('data', listening);
      server.stdout.resume();
      resolve({ port: Number(m[1]), server });
    });
  });
}
