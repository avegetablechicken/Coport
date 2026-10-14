const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');

function harness() {
  const parent = { id: 'activity-list', children: [] };
  const node = (name) => ({
    name, style: {}, parentElement: parent,
    get nextSibling() { return parent.children[parent.children.indexOf(this) + 1] ?? null; },
    remove() { parent.children.splice(parent.children.indexOf(this), 1); },
    replaceWith(other) { parent.children.splice(parent.children.indexOf(this), 1, other); },
  });
  const context = { document: { createElement: () => node('spacer') } };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('function motionRows('), source.indexOf('function morphCopy(')), context);
  return { context, parent, node };
}

test('motion retains viewport rows and one viewport of overscan', () => {
  const { context } = harness();
  const heights = [0, 99, 100, 200, 499, 500, 600];
  const rows = context.motionRows({ querySelectorAll: () => heights.map(top => ({
    getBoundingClientRect: () => ({ top, bottom: top + 20, height: 20 }),
  })) }, { top: 200, bottom: 400, height: 200 });
  assert.deepEqual(Array.from(rows, row => row.keep), [true, true, true, true, true, true, false]);
});

test('offscreen animation rows collapse to an exact-height spacer without removing visible rows', () => {
  const { context, parent, node } = harness();
  const rows = [node('visible'), node('off1'), node('off2')];
  parent.children.push(...rows);
  context.trimMotionRows({ querySelectorAll: () => rows }, [
    { keep: true, height: 35 }, { keep: false, height: 31.5 }, { keep: false, height: 42.25 },
  ]);
  assert.deepEqual(parent.children.map(n => n.name), ['visible', 'spacer']);
  assert.equal(parent.children[1].style.height, '73.75px');
  assert.deepEqual(rows[0].style, {});
});

test('expanded log details separate spacer runs and non-log rows remain intact', () => {
  const { context, parent, node } = harness();
  const rows = [node('off1'), node('off2'), node('recent')];
  const detail = node('detail');
  rows[2].parentElement = { id: 'recent-requests' };
  parent.children.push(rows[0], detail, rows[1]);
  context.trimMotionRows({ querySelectorAll: () => rows }, rows.map(() => ({ keep: false, height: 30 })));
  assert.deepEqual(parent.children.map(n => n.name), ['spacer', 'detail', 'spacer']);
  assert.deepEqual(rows[2].style, {});
});

test('a short old viewport retains rows exposed when the native window grows', () => {
  const { context } = harness();
  context.screen = { height: 1000 };
  const rows = context.motionRows({ querySelectorAll: () => [{
    getBoundingClientRect: () => ({ top: 800, bottom: 830, height: 30 }),
  }] }, { top: 40, bottom: 240, height: 200 });
  assert.equal(rows[0].keep, true);
});
