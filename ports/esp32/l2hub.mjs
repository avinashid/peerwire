// A hub for QEMU's `-netdev dgram`: each Ethernet frame one emulated node sends
// to 127.0.0.1:<hub> goes on to every other node's port, as on a shared LAN.
// With NO_MULTICAST=1 it drops IPv4 multicast frames (to 01:00:5e:...), as the
// router in NB-091 did to the board.
// Usage: [NO_MULTICAST=1] node l2hub.mjs <hub port> <node port>,<node port>,...
import dgram from 'node:dgram';

const hub = Number(process.argv[2]);
const nodes = process.argv[3].split(',').map(Number);
const noMulticast = process.env.NO_MULTICAST === '1';
let frames = 0;
let dropped = 0;
const sock = dgram.createSocket('udp4');
sock.on('message', (frame, from) => {
  frames++;
  if (noMulticast && frame[0] === 0x01 && frame[1] === 0x00 && frame[2] === 0x5e) {
    dropped++;
    return;
  }
  for (const port of nodes) if (port !== from.port) sock.send(frame, port, '127.0.0.1');
});
sock.bind(hub, '127.0.0.1', () =>
  console.log(`l2hub: 127.0.0.1:${hub} -> ${nodes.join(', ')}${noMulticast ? ', no multicast' : ''}`),
);
process.on('SIGTERM', () => {
  console.log(`l2hub: ${frames} frames${noMulticast ? `, ${dropped} multicast dropped` : ''}`);
  process.exit(0);
});
