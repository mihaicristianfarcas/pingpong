//! Keyboard regressions against the built page and its actual component scripts.
//! DOM checks cover control order and focus changes; they do not render a browser.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { Window } from 'happy-dom';
import ts from 'typescript';

const site = dirname(dirname(fileURLToPath(import.meta.url)));
const html = readFileSync(join(site, 'dist/index.html'), 'utf8').replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, '');
const actions = 'a[href], button, summary, input:not([type="hidden"]), select, textarea, video[controls]';

function visible(element, window) {
  for (let parent = element; parent; parent = parent.parentElement) {
    if (parent.hidden || parent.hasAttribute('inert') || parent.getAttribute('aria-hidden') === 'true') return false;
    const style = window.getComputedStyle(parent);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
    if (parent.localName === 'details' && !parent.open && !parent.querySelector('summary')?.contains(element)) return false;
  }
  return true;
}

function page(components = [], width = 1280) {
  const window = new Window({
    url: 'http://localhost/',
    settings: {
      enableJavaScriptEvaluation: true,
      suppressInsecureJavaScriptEnvironmentWarning: true,
      disableJavaScriptFileLoading: true,
      disableCSSFileLoading: true,
    },
  });
  window.happyDOM.setViewport({ width, height: 900 });
  window.fetch = async () => { throw new Error('release data is offline'); };
  window.IntersectionObserver = class { observe() {} };
  window.ResizeObserver = class { observe() {} };
  const document = window.document;
  document.write(html);
  for (const link of document.querySelectorAll('link[rel="stylesheet"]')) {
    const style = document.createElement('style');
    style.textContent = readFileSync(join(site, 'dist', new URL(link.href).pathname), 'utf8');
    document.head.append(style);
  }
  // Supply geometry only for visibility checks. Layout and native key events
  // require a real browser; the assertions below concern DOM focus order.
  window.HTMLElement.prototype.getClientRects = function () {
    return visible(this, window) ? [{ top: 0, bottom: 40, height: 40 }] : [];
  };
  const run = (component) => {
    const source = readFileSync(join(site, 'src', component), 'utf8');
    for (const match of source.matchAll(/<script\b(?![^>]*\/>)([^>]*)>([\s\S]*?)<\/script>/g)) {
      if (match[1].includes('application/ld+json')) continue;
      const js = ts.transpileModule(match[2], { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
      window.eval(`(() => { ${js} })()`);
    }
  };
  for (const component of components) run(component);
  const enabled = (element) => visible(element, window)
    && !element.matches(':disabled, [aria-disabled="true"]');
  const stops = () => [...document.querySelectorAll(`${actions}, [tabindex]`)]
    .filter((element) => enabled(element) && Number(element.getAttribute('tabindex') ?? 0) >= 0);
  return { window, document, run, enabled, stops, close: () => window.happyDOM.abort() };
}

const components = [
  'components/Nav.astro', 'components/Gallery.astro', 'components/Install.astro',
  'components/Command.astro', 'components/Video.astro', 'layouts/Base.astro',
];

for (const width of [1280, 390]) {
  test(`at ${width}px the page offers every visible action, in header-to-footer order`, async () => {
    const { window, document, enabled, stops, close } = page(components, width);
    try {
      if (width < 820) document.querySelector('[data-menu]').click();
      const order = stops();
      const expected = [...document.querySelectorAll(actions)].filter(enabled);
      assert.deepEqual(order.map((element) => element.outerHTML), expected.map((element) => element.outerHTML));
      assert.ok(order.length > 25, 'the traversal must cover the whole page');
      assert.equal(document.querySelectorAll('[tabindex]:not(a):not(button):not(input):not(summary)').length, 0);
      assert.equal(document.querySelectorAll('[tabindex]:not([tabindex="0"]):not([tabindex="-1"])').length, 0);
      const phases = order.map((element) => element.closest('header') ? 1
        : element.closest('main') ? 2 : element.closest('footer') ? 3 : 0);
      assert.ok(phases.every((phase, i) => i === 0 || phase >= phases[i - 1]));
      assert.ok(order.at(-1).closest('footer'));
      for (const element of order) {
        element.focus();
        assert.ok(document.activeElement === element, `cannot focus ${element.outerHTML}`);
        for (const shiftKey of [false, true]) {
          const key = new window.KeyboardEvent('keydown', { key: 'Tab', shiftKey, bubbles: true, cancelable: true });
          element.dispatchEvent(key);
          assert.equal(key.defaultPrevented, false, 'Tab and Shift+Tab must remain native');
        }
      }
    } finally { await close(); }
  });
}

test('every gallery choice remains a button in the Tab order and focus stops autoplay', async () => {
  const { document, stops, close } = page(components);
  try {
    const gallery = document.querySelector('[data-gallery]');
    const choices = [...gallery.querySelectorAll('[data-tab]')];
    assert.equal(choices.length, 5);
    assert.ok(choices.every((button) => stops().includes(button)));
    for (const [i, button] of choices.entries()) {
      button.focus();
      assert.equal(gallery.hasAttribute('data-auto'), false);
      button.click();
      assert.ok(document.activeElement === button);
      assert.equal(button.getAttribute('aria-pressed'), 'true');
      assert.equal(gallery.querySelector(`[data-pane="${i}"]`).getAttribute('aria-hidden'), 'false');
      assert.ok(choices.every((choice) => stops().includes(choice)));
    }
  } finally { await close(); }
});

test('all five download choices are buttons and all six combinations work without stealing focus', async () => {
  const { document, stops, close } = page(components);
  try {
    const apps = [...document.querySelectorAll('button[data-select-app]')];
    const systems = [...document.querySelectorAll('button[data-select-os]')];
    assert.equal(apps.length, 2);
    assert.equal(systems.length, 3);
    assert.equal(document.querySelectorAll('input[type="radio"]').length, 0);
    assert.ok([...apps, ...systems].every((button) => stops().includes(button)));
    for (const app of apps) {
      app.focus();
      app.click();
      for (const system of systems) {
        system.focus();
        system.click();
        const panels = [...document.querySelectorAll('[data-picker] .panel')].filter((panel) => !panel.hidden);
        assert.equal(panels.length, 1);
        assert.equal(panels[0].dataset.app, app.dataset.selectApp);
        assert.equal(panels[0].dataset.os, system.dataset.selectOs);
        assert.ok(document.activeElement === system);
        assert.equal(system.getAttribute('aria-pressed'), 'true');
        assert.ok([...apps, ...systems].every((button) => stops().includes(button)));
      }
    }
  } finally { await close(); }
});

test('section and skip links focus a visible action, never a heading or static region', async () => {
  const { document, enabled, close } = page(components);
  try {
    const links = [...document.querySelectorAll('header a[href^="#"], a.skip')];
    for (const link of links) {
      link.focus();
      link.click();
      const target = document.getElementById(link.getAttribute('href').slice(1));
      const first = [...target.querySelectorAll(actions)].find(enabled);
      assert.ok(document.activeElement === first, `expected an action inside ${target.id}`);
      assert.ok(document.activeElement.matches(actions));
    }
    assert.equal(document.querySelectorAll('h1[tabindex], h2[tabindex], main[tabindex], section[tabindex]').length, 0);
  } finally { await close(); }
});

test('closed FAQ answers and inactive download panels never enter the Tab order', async () => {
  const { window, document, stops, close } = page(components);
  try {
    const details = document.querySelector('details');
    const answerLink = details.querySelector('p a');
    assert.ok(!stops().includes(answerLink));
    details.open = true;
    assert.ok(stops().includes(answerLink));
    details.open = false;
    assert.ok(!stops().includes(answerLink));
    const hiddenPanels = document.querySelectorAll('[data-picker] .panel[hidden]');
    assert.equal(hiddenPanels.length, 5);
    for (const panel of hiddenPanels) {
      assert.equal(window.getComputedStyle(panel).display, 'none');
      assert.ok(stops().every((element) => !panel.contains(element)));
    }
  } finally { await close(); }
});

test('missing downloads leave focus on an available choice and cannot be followed', async () => {
  const { window, document, run, stops, close } = page(['layouts/Base.astro']);
  try {
    let respond;
    window.fetch = () => new Promise((resolve) => { respond = resolve; });
    run('components/Install.astro');
    const download = document.querySelector('[data-picker] .panel:not([hidden]) [data-file]');
    download.focus();
    respond({ ok: true, json: async () => ({ version: '1.0', commit: 'abcdef123456', published: '2026-10-04', files: {} }) });
    await new Promise(setImmediate);
    assert.equal(download.hasAttribute('href'), false);
    assert.ok(!stops().includes(download));
    assert.ok(document.activeElement === document.querySelector('[data-select-os][aria-pressed="true"]'));
  } finally { await close(); }
});

test('without JavaScript static content has no Tab stops and every download stays available', async () => {
  const { document, stops, close } = page();
  try {
    assert.ok(stops().every((element) => element.matches(actions)));
    assert.equal(document.querySelectorAll('[data-picker] .panel[hidden]').length, 0);
    assert.ok(stops().filter((element) => element.matches('[data-file]')).length >= 6);
  } finally { await close(); }
});
