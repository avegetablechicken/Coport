const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const scripts = require('./panel_harness.cjs');


// Run the real renderPage and content observer: refreshing replaces the
// hovered/focused node, as innerHTML does in the panel's WebView.
function panel(page = 'main', newline) {
  // Every listener of a type runs, in the order the panel added them.
  const handlers = {}, windowHandlers = {};
  const dispatch = (to) => new Proxy({}, { get: (_, type) => (event) => { for (const fn of to[type] || []) fn(event); } });
  const events = dispatch(handlers), windowEvents = dispatch(windowHandlers);
  const timers = new Map(), microtasks = [];
  let timerId = 0, owners = [], hit = 0;
  let rows = [{ key: 'account-a', tip: '50.0 tok/s' }];
  const body = { matches: () => false };
  const tip = { hidden: true, style: {}, contains: node => node === tip, getBoundingClientRect: () => ({ width: 180, height: 40 }) };
  const doc = {
    body, activeElement: body, hidden: false,
    documentElement: { clientWidth: 480, clientHeight: 640, classList: { add() {} } },
    addEventListener(type, fn) { (handlers[type] ||= []).push(fn); },
    elementFromPoint: () => hit === -2 ? tip : owners[hit] ?? null,
    getElementById: id => id === 'route-tooltip' ? tip : content,
  };
  const owner = row => {
    const attrs = row.kind === 'title' || row.title ? { title: row.title ?? row.tip } : {};
    const el = {
    tagName: row.kind === 'svg' || row.kind === 'bar' ? 'g' : 'SPAN',
    dataset: { ...(row.kind !== 'title' && row.kind !== 'svg' ? { tip: row.tip } : {}),
      ...(row.generic ? {} : { tipKey: row.key }), ...(row.slot ? { tipSlot: row.slot } : {}) },
    isConnected: true, keyboard: false, children: [],
    matches(selector) { return selector === ':focus-visible' ? this.keyboard : selector === ':hover' ? owners[hit] === this : selector.includes('.chart-bar') && row.kind === 'bar'; },
    contains(target) { return target === this; },
    closest() { return null; },
    getBoundingClientRect: () => ({ left: 100, top: 120, bottom: 132, width: 12, height: 12 }),
    getAttribute: name => attrs[name] ?? null,
    hasAttribute: name => name in attrs,
    setAttribute(name, value) { attrs[name] = value; this[name] = value; },
    removeAttribute(name) { delete attrs[name]; delete this[name]; },
    querySelector(selector) { return selector === ':scope > title' ? this.children.find(c => c.tagName === 'title') ?? null : null; },
    prepend(child) { this.children.unshift(child); child.parentElement = this; },
    focus() { this.keyboard = true; doc.activeElement = this; events.focusin({ target: this }); },
    };
    if (row.generic) el.parentElement = { tagName: 'DIV', dataset: { tipScope: row.key }, children: [el] };
    if (row.kind === 'svg') el.prepend({ tagName: 'title', textContent: row.tip,
      remove() { this.parentElement.children.splice(this.parentElement.children.indexOf(this), 1); this.parentElement = null; } });
    return el;
  };
  const content = {
    firstElementChild: {},
    addEventListener() {},
    querySelectorAll: selector => selector.includes('[data-tip]') ? owners : [],
    contains: node => owners.includes(node),
    set innerHTML(_) {
      for (const old of owners) {
        if (doc.activeElement === old) {
          doc.activeElement = body;
          events.focusout({ target: old, relatedTarget: body });
        }
        old.isConnected = false;
      }
      owners = rows.map(owner);
      this.firstElementChild = {};
    },
  };
  const p = scripts.panel({
    document: doc, newline,
    window: { innerWidth: 480, innerHeight: 640, addEventListener(type, fn) { (windowHandlers[type] ||= []).push(fn); } },
    globals: {
      setTimeout(fn) { timers.set(++timerId, fn); return timerId; },
      clearTimeout(id) { timers.delete(id); },
      queueMicrotask(fn) { microtasks.push(fn); },
    },
  });
  const { context } = p;
  // The panel's own ui, so a test can change the page it shows.
  context.ui = Object.assign(p.get('ui'), { page, builtPage: page });
  // The pages themselves draw nothing here; the rows below stand for them.
  Object.assign(context, { main: () => '', devicesPage: () => '', settings: () => '', renderActivityForwarding() {}, renderActivityList() {} });
  // The content observer: the one that also follows title and tip changes.
  const watcher = p.observers.find(o => o.target === content && o.options?.characterData);
  assert.ok(watcher, 'content observer is registered');
  const observer = () => watcher.fn([]);
  content.innerHTML = '';
  const redraw = () => { context.renderPage(); while (microtasks.length) microtasks.shift()(); observer(); };
  const hover = () => events.pointerover({ target: owners[hit], clientX: 106, clientY: 126 });
  const flush = () => { for (const [id, fn] of timers) { timers.delete(id); fn(); } };
  return { context, doc, tip, timers, events, windowEvents, content, redraw, hover, flush,
    owner: () => owners[hit], rows: value => { rows = value; }, hit: value => { hit = value; }, observe: () => observer(),
    flushMicrotasks: () => { while (microtasks.length) microtasks.shift()(); } };
}

// Exercise both checkout formats on every platform, without normalizing the
// source passed to the VM or weakening the tooltip behavior assertions.
for (const [format, newline] of [['LF', '\n'], ['CRLF', '\r\n']]) {
  for (const page of ['main', 'devices', 'activity']) {
    test(`${format} ${page}: the content observer updates a tooltip after repeated refreshes`, () => {
      const p = panel(page, newline);
      p.hover(); p.flush();
      for (let i = 0; i < 3; i++) {
        p.rows([{ key: 'account-a', tip: `${60 + i}.0 tok/s` }]);
        // An asynchronous content replacement must be handled by the observer
        // itself, without renderPage's tooltip restoration helping it.
        p.content.innerHTML = '';
        p.observe();
        assert.equal(p.tip.hidden, false);
        assert.equal(p.tip.textContent, `${60 + i}.0 tok/s`);
        assert.equal(p.owner()['aria-describedby'], 'route-tooltip');
      }
    });
  }
}

for (const page of ['main', 'devices', 'activity']) {
  test(`${page}: repeated refreshes keep the hovered TPS tooltip visible and current`, () => {
    const p = panel(page);
    p.hover(); p.flush();
    assert.equal(p.tip.hidden, false);
    assert.equal(p.doc.activeElement.matches(':focus-visible'), false);
    for (let i = 0; i < 5; i++) {
      const previous = p.owner();
      p.rows([{ key: 'account-a', tip: `${50 + i}.0 tok/s` }]);
      p.redraw();
      if (page === 'activity') { p.content.innerHTML = ''; p.observe(); }
      assert.equal(p.tip.hidden, false);
      assert.equal(p.tip.textContent, `${50 + i}.0 tok/s`);
      assert.equal(p.owner()['aria-describedby'], 'route-tooltip');
      // A delayed leave from the detached SVG must not close its replacement.
      p.events.pointerout({ target: previous, relatedTarget: null });
      assert.equal(p.tip.hidden, false);
    }
    p.events.pointerout({ target: p.owner(), relatedTarget: null });
    assert.equal(p.tip.hidden, true);
  });
}

test('refresh while waiting keeps the original hover delay, then shows the latest text', () => {
  const p = panel();
  p.hover();
  const timer = [...p.timers.keys()][0];
  for (let i = 0; i < 3; i++) p.redraw();
  assert.deepEqual([...p.timers.keys()], [timer]);
  assert.equal(p.tip.hidden, true);
  p.rows([{ key: 'account-a', tip: '75.0 tok/s' }]); p.redraw(); p.flush();
  assert.equal(p.tip.textContent, '75.0 tok/s');
  assert.equal(p.tip.hidden, false);
});

test('keyboard tooltip follows the same account when refreshed rows change order', () => {
  const p = panel();
  p.owner().focus(); p.flush();
  p.rows([{ key: 'account-b', tip: 'other account' }, { key: 'account-a', tip: '75.0 tok/s' }]);
  p.redraw();
  assert.equal(p.tip.hidden, false);
  assert.equal(p.tip.textContent, '75.0 tok/s');
  assert.equal(p.doc.activeElement.dataset.tipKey, 'account-a');
  assert.equal(p.doc.activeElement.isConnected, true);
});

test('a different account under the pointer, a removed metric, and page changes close the tooltip', () => {
  for (const next of ['different', 'missing', 'page']) {
    const p = panel(); p.hover(); p.flush();
    if (next === 'different') p.rows([{ key: 'account-b', tip: 'other account' }]);
    if (next === 'missing') p.rows([]);
    if (next === 'page') p.context.ui.page = 'settings';
    p.redraw(); p.flush();
    assert.equal(p.tip.hidden, true, next);
  }
});

test('click, scroll and resize still dismiss the tooltip after a refresh', () => {
  for (const close of [p => p.windowEvents.pointerdown({ target: p.content }), p => p.windowEvents.resize(), p => p.events.scroll({ target: p.content })]) {
    const p = panel(); p.hover(); p.flush(); p.redraw();
    close(p);
    assert.equal(p.tip.hidden, true);
  }
});

test('moving off a replaced hovered node closes the tooltip even without pointerout', () => {
  const p = panel(); p.hover(); p.flush(); p.redraw();
  p.events.pointermove({ target: p.content, clientX: 400, clientY: 400 });
  assert.equal(p.tip.hidden, true);
});

test('a real keyboard focus departure still closes the tooltip', () => {
  const p = panel();
  const owner = p.owner();
  owner.focus(); p.flush(); p.hit(-1);
  owner.keyboard = false;
  p.events.focusout({ target: owner, relatedTarget: p.content });
  p.flushMicrotasks();
  assert.equal(p.tip.hidden, true);
});

for (const kind of ['data', 'title', 'svg', 'bar']) {
  test(`${kind}: unkeyed tooltips retain their pending delay and update over repeated refreshes`, () => {
    const p = panel();
    p.rows([{ key: 'account-a', generic: true, kind, tip: 'old value', slot: kind === 'bar' ? 'minute-10' : undefined }]);
    p.redraw(); p.hover();
    const timer = [...p.timers.keys()][0];
    p.redraw();
    assert.deepEqual([...p.timers.keys()], [timer]);
    p.flush();
    for (let i = 0; i < 4; i++) {
      p.rows([{ key: 'account-a', generic: true, kind, tip: `new value ${i}`, slot: kind === 'bar' ? 'minute-10' : undefined }]);
      p.redraw();
      assert.equal(p.tip.hidden, false);
      assert.equal(p.tip.textContent, `new value ${i}`);
      assert.equal(p.owner().getAttribute('title'), null, 'native HTML tooltip is suppressed');
      assert.equal(p.owner().querySelector(':scope > title'), null, 'native SVG tooltip is suppressed');
    }
    p.events.pointerout({ target: p.owner(), relatedTarget: null });
    assert.equal(p.tip.hidden, true);
    if (kind === 'title') assert.equal(p.owner().getAttribute('title'), 'new value 3');
    if (kind === 'svg') assert.equal(p.owner().querySelector(':scope > title').textContent, 'new value 3');
  });

  test(`${kind}: keyboard focus retains an unkeyed tooltip through row reordering`, () => {
    const p = panel();
    const row = { key: 'account-a', generic: true, kind, tip: 'wanted account' };
    p.rows([row]); p.redraw(); p.owner().focus(); p.flush();
    p.rows([{ ...row, key: 'account-b', tip: 'different account' }, row]);
    p.redraw();
    assert.equal(p.tip.hidden, false);
    assert.equal(p.tip.textContent, row.tip);
    assert.equal(p.doc.activeElement.parentElement.dataset.tipScope, 'account-a');
  });
}

test('a chart category or bucket-width change does not retain the previous tooltip', () => {
  const p = panel();
  const row = { key: 'chart-a', generic: true, kind: 'bar', slot: 'model:0:60000', tip: '10:00' };
  p.rows([row]); p.redraw(); p.hover(); p.flush();
  p.rows([{ ...row, slot: 'all:0:900000', tip: '10:00 – 10:15' }]); p.redraw();
  assert.equal(p.tip.hidden, true);
});

test('tooltip text updates without replacing its node and restores the latest native title', () => {
  const p = panel();
  p.rows([{ key: 'status', generic: true, kind: 'title', tip: 'available' }]);
  p.redraw(); p.hover(); p.flush();
  p.owner().setAttribute('title', 'offline'); p.observe();
  assert.equal(p.tip.textContent, 'offline');
  assert.equal(p.owner().getAttribute('title'), null);
  p.events.pointerout({ target: p.owner(), relatedTarget: null });
  assert.equal(p.owner().getAttribute('title'), 'offline');
});

test('suppressing a native title preserves both an attribute and an SVG title on the same node', () => {
  const p = panel();
  p.rows([{ key: 'slice', generic: true, kind: 'svg', title: 'attribute text', tip: 'SVG text' }]);
  p.redraw(); p.hover(); p.flush();
  assert.equal(p.tip.textContent, 'attribute text');
  assert.equal(p.owner().getAttribute('title'), null);
  assert.equal(p.owner().querySelector(':scope > title'), null);
  p.events.pointerout({ target: p.owner(), relatedTarget: null });
  assert.equal(p.owner().getAttribute('title'), 'attribute text');
  assert.equal(p.owner().querySelector(':scope > title').textContent, 'SVG text');
});

test('long tooltips remain usable while the pointer scrolls their text across refreshes', () => {
  const p = panel(); p.hover(); p.flush();
  p.events.pointerout({ target: p.owner(), relatedTarget: p.tip });
  p.hit(-2);
  p.events.pointermove({ target: p.tip, clientX: 106, clientY: 90 });
  p.events.scroll({ target: p.tip });
  p.windowEvents.pointerdown({ target: p.tip });
  p.redraw();
  assert.equal(p.tip.hidden, false);
  p.events.pointerout({ target: p.tip, relatedTarget: p.content });
  assert.equal(p.tip.hidden, true);
});

test('Escape and window blur dismiss every tooltip source', () => {
  for (const kind of ['data', 'title', 'svg']) {
    for (const close of [p => p.windowEvents.keydown({ key: 'Escape' }), p => p.windowEvents.blur()]) {
      const p = panel();
      p.rows([{ key: 'test', generic: true, kind, tip: 'shown' }]); p.redraw(); p.hover(); p.flush();
      close(p);
      assert.equal(p.tip.hidden, true);
    }
  }
});
