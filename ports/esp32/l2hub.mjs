// A hub for QEMU's `-netdev dgram`: each Ethernet frame one emulated node sends
// to 127.0.0.1:<hub> goes on to every other node's port, as on a shared LAN.
// Usage: node l2hub.mjs <hub port> <node port>,<node port>,...
import dgram from 'node:dgram';

const hub = Number(process.argv[2]);
const nodes = process.argv[3].split(',').map(Number);
let frames = 0;
const sock = dgram.createSocket('udp4');
sock.on('message', (frame, from) => {
  frames++;
  for (const port of nodes) if (port !== from.port) sock.send(frame, port, '127.0.0.1');
});
sock.bind(hub, '127.0.0.1', () => console.log(`l2hub: 127.0.0.1:${hub} -> ${nodes.join(', ')}`));
process.on('SIGTERM', () => {
  console.log(`l2hub: ${frames} frames`);
  process.exit(0);
});
