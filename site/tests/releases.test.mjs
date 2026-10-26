import assert from 'node:assert/strict';
import { test } from 'node:test';
import { releaseResponse } from '../src/releases.ts';
const request = (path, method='GET') => new Request(`https://whatsrusty.apps.balaj.me${path}`, {method});
test('immutable downloads redirect only to the owned versioned GitHub release', async () => {
  const response = await releaseResponse(request('/downloads/WhatsRusty-1.2.3-8.zip'));
  assert.equal(response.headers.get('location'), 'https://github.com/talhabalaj/whatsrusty/releases/download/v1.2.3/WhatsRusty-1.2.3-8.zip');
  for (const path of ['/downloads/evil.zip', '/downloads/WhatsRusty-1.2-8.zip/evil', '/other']) assert.equal(await releaseResponse(request(path)), null);
});
test('feed preserves signed bytes and avoids caching; HEAD has no body', async () => {
  const fetch = async url => { assert.equal(url, 'https://github.com/talhabalaj/whatsrusty/releases/latest/download/appcast.xml'); return new Response('signed bytes'); };
  const response = await releaseResponse(request('/appcast.xml'), fetch);
  assert.equal(await response.text(), 'signed bytes'); assert.equal(response.headers.get('cache-control'), 'no-store');
  assert.equal(await (await releaseResponse(request('/appcast.xml', 'HEAD'), fetch)).text(), '');
});
test('latest download rejects arbitrary and unnotarized manifest redirects', async () => {
  for (const manifest of [{file:'https://evil.example', notarized:true}, {file:'WhatsRusty-1-8.zip', notarized:false}, {file:'WhatsRusty-1-8.zip.sha256', notarized:true}]) {
    assert.equal((await releaseResponse(request('/downloads/latest.zip'), async () => Response.json(manifest))).status, 502);
  }
  const response = await releaseResponse(request('/downloads/latest.zip'), async () => Response.json({file:'WhatsRusty-1-8.zip', notarized:true}));
  assert.equal(response.headers.get('location'), '/downloads/WhatsRusty-1-8.zip');
});
test('first release remains unavailable while GitHub has no published manifest', async () => {
  assert.equal((await releaseResponse(request('/downloads/manifest.json'), async () => new Response(null,{status:404}))).status,404);
});
