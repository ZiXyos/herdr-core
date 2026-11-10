#!/usr/bin/env node
// Deterministic stand-in for a harness that rewrites its process title (as Pi's
// setupCli does with `process.title = APP_NAME`). No network beyond the pane's own
// native socket, no model. `--ready` reports the optional Hydra readiness handshake.
'use strict';
const net = require('net');
process.title = 'harness-title';
console.log('TITLE_SET pid=' + process.pid);
if (process.argv.includes('--ready')) {
  const socket = net.createConnection(process.env.HERDR_SOCKET_PATH, () => {
    socket.write(JSON.stringify({
      id: 'ready',
      method: 'pane.report_metadata',
      params: {
        pane_id: process.env.HERDR_PANE_ID,
        source: 'hydra.launch.ready',
        tokens: {
          hydra_ready_launch_id: process.env.HYDRA_LAUNCH_ID,
          hydra_ready_configuration_hash: process.env.HYDRA_CONFIGURATION_HASH,
          hydra_ready_pid: String(process.pid),
        },
      },
    }) + '\n');
  });
  let reply = '';
  socket.on('data', (chunk) => {
    reply += chunk;
    if (reply.includes('\n')) {
      console.log('READY_REPORTED ' + JSON.parse(reply.split('\n')[0]).result.type);
      socket.end();
    }
  });
}
process.stdin.on('data', (data) => console.log('TITLE_REPLY ' + data.toString().trim()));
