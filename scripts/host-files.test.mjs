import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const script = readFileSync(new URL('../shell/index.html', import.meta.url), 'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];
function host(picked) {
  const writes = [];
  const window = { localStorage: { getItem: () => null, setItem() {} },
    otools: { dialog: { open: async () => null, save: async () => picked },
      readHostFile: async () => ({}), writeHostFile: async (...args) => { writes.push(args); } } };
  vm.runInNewContext(script, { window, console });
  return { bridge: window.vectorcraftOtools, writes };
}
test('choosing a save path does not write file bytes', async () => {
  const { bridge, writes } = host('/tmp/preset.json');
  assert.equal(await bridge.pickSavePath({name: 'preset.json'}), '/tmp/preset.json');
  assert.equal(writes.length, 0);
});
test('cancelling path selection returns null without writing', async () => {
  const { bridge, writes } = host(null);
  assert.equal(await bridge.pickSavePath({name: 'preset.json'}), null);
  assert.equal(writes.length, 0);
});
