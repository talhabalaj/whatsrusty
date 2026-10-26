import assert from 'node:assert/strict';
import test from 'node:test';
import { readFile } from 'node:fs/promises';
import { releaseDetails } from '../public/release.mjs';

const manifest = { version: '0.1.0', build: '2', file: 'WhatsRusty-0.1.0-2.zip', minimumMacOS: '14.0', architecture: 'arm64', notarized: true, bytes: 5068169 };
test('published notarized release displays its size and requirements', () => {
  assert.deepEqual(releaseDetails(manifest), { href: '/downloads/latest.zip', detail: 'Version 0.1.0 · 5.1 MB · Apple silicon · macOS 14.0 or later' });
});
test('missing, unsigned, malformed, and mismatched releases do not replace release details', () => {
  for (const value of [null, {}, { ...manifest, notarized: false }, { ...manifest, notarized: 'true' },
    { ...manifest, file: 'https://example.com/other.zip' }, { ...manifest, file: '../WhatsRusty-0.1.0-2.zip' },
    { ...manifest, file: 'WhatsRusty-0.1.0-3.zip' }, { ...manifest, minimumMacOS: '<script>' },
    { ...manifest, bytes: 0 }, { ...manifest, bytes: '5068169' }, { ...manifest, bytes: Infinity },
    { ...manifest, architecture: 'x86_64' }, { ...manifest, version: 'bad' }, { ...manifest, build: 2 }]) {
    assert.equal(releaseDetails(value), null);
  }
});

test('download links work without JavaScript or a successful metadata refresh', async () => {
  const html = await readFile(new URL('../public/index.html', import.meta.url), 'utf8');
  const links = [...html.matchAll(/<a[^>]*data-release-link[^>]*>/g)];
  assert.equal(links.length, 2);
  for (const [link] of links) assert.match(link, /href="\/downloads\/latest\.zip"/);
  assert.doesNotMatch(html, /first (?:notarized |)release is being prepared|Follow the first release/);
});
