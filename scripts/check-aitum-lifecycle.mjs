#!/usr/bin/env node
// Off-air integration regression. Requires an isolated OBS profile already sending
// wide + vertical streams to loopback receivers; never starts a broadcast itself.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFile, realpath} from 'node:fs/promises';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {setTimeout as sleep} from 'node:timers/promises';
import path from 'node:path';
const exec = promisify(execFile);
const root = process.argv[2];
if (!root || process.argv.length !== 3) throw Error('Usage: node scripts/check-aitum-lifecycle.mjs /isolated/config/obs-studio');
assert.notEqual(await realpath(root), await realpath(path.join(process.env.XDG_CONFIG_HOME || path.join(process.env.HOME, '.config'), 'obs-studio')), 'Use an isolated OBS configuration');
const config = JSON.parse(await readFile(path.join(root, 'plugin_config/obs-websocket/config.json'), 'utf8'));
const profile = JSON.parse(await readFile(path.join(root, 'basic/profiles/Untitled/aitum.json'), 'utf8'));
const vertical = profile.outputs.find(o => o.name === 'Vertical Stream');
assert.equal(new URL(vertical.stream_server).hostname, '127.0.0.1');
async function offAir() {
  for (const [key, expected] of [['show.mode', 'offline'], ['twitch.stream.live', false]]) {
    const {stdout} = await exec('streamctl', ['--json', 'get', key], {timeout: 5000});
    assert.equal(JSON.parse(stdout).value, expected, `Unsafe show state: ${key}`);
  }
}
await offAir();
const ws = new WebSocket(`ws://127.0.0.1:${config.server_port}`);
const pending = new Map();
let next = 0;
const hash = s => createHash('sha256').update(s).digest('base64');
await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(Error('OBS handshake timed out')), 8000);
  ws.onerror = () => { clearTimeout(timer); reject(Error('OBS connection failed')); };
  ws.onmessage = event => {
    const {op, d} = JSON.parse(event.data);
    if (op === 0) {
      const a = d.authentication;
      ws.send(JSON.stringify({op: 1, d: {rpcVersion: 1, eventSubscriptions: 0,
        ...(a ? {authentication: hash(hash(config.server_password + a.salt) + a.challenge)} : {})}}));
    } else if (op === 2) { clearTimeout(timer); resolve(); }
    else if (op === 7) {
      const p = pending.get(d.requestId);
      if (!p) return;
      clearTimeout(p.timer); pending.delete(d.requestId);
      d.requestStatus.result ? p.resolve(d.responseData || {}) : p.reject(Error(JSON.stringify(d.requestStatus)));
    }
  };
});
function request(requestType, requestData = {}) {
  return new Promise((resolve, reject) => {
    const requestId = String(++next);
    const timer = setTimeout(() => { pending.delete(requestId); reject(Error(`OBS timeout: ${requestType}`)); }, 8000);
    pending.set(requestId, {resolve, reject, timer});
    ws.send(JSON.stringify({op: 6, d: {requestId, requestType, requestData}}));
  });
}
async function vendor(requestType, requestData = {}) {
  const {responseData} = await request('CallVendorRequest', {vendorName: 'aitum-stream-suite', requestType, requestData});
  assert.equal(responseData.success, true, requestType);
  return responseData;
}
const outputName = 'Aitum Stream Suite Output Vertical Stream';
let ownedReplay = false;
try {
  const {streamServiceSettings} = await request('GetStreamServiceSettings');
  assert.equal(new URL(streamServiceSettings.server).hostname, '127.0.0.1');
  assert.equal((await request('GetStreamStatus')).outputActive, true);
  assert.equal((await request('GetOutputStatus', {outputName})).outputActive, true);
  assert.equal((await request('GetRecordStatus')).outputActive, false);
  const {outputs} = await vendor('get_outputs');
  assert.equal(outputs.find(o => o.name === 'Vertical Backtrack')?.active, false);
  for (let cycle = 0; cycle < 3; cycle++) {
    await offAir();
    ownedReplay = true;
    await vendor('start_output', {output: 'Vertical Backtrack'});
    await sleep(2000);
    const before = await request('GetOutputStatus', {outputName});
    await offAir();
    await vendor('stop_output', {output: 'Vertical Backtrack'});
    await sleep(2500);
    assert.equal((await vendor('get_outputs')).outputs.find(o => o.name === 'Vertical Backtrack')?.active, false);
    ownedReplay = false;
    const after = await request('GetOutputStatus', {outputName});
    assert.equal(after.outputActive, true);
    assert.ok(after.outputTotalFrames > before.outputTotalFrames, 'Video froze after shared replay encoder release');
    assert.ok(after.outputBytes > before.outputBytes, 'Stream bytes stopped after replay release');
    console.log(JSON.stringify({cycle, frames: after.outputTotalFrames - before.outputTotalFrames, bytes: after.outputBytes - before.outputBytes}));
  }
} finally {
  try { if (ownedReplay) { await offAir(); await vendor('stop_output', {output: 'Vertical Backtrack'}); } }
  finally { ws.close(); }
}
