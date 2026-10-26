import assert from 'node:assert/strict';
import test from 'node:test';
import { releaseArchivePattern, releaseFiles, releaseOrigin } from './release-files.mjs';

const valid = { version: '1.1', build: '20261004.2', minimumMacOS: '27.0', sha256: 'a'.repeat(64), commitSha: 'b'.repeat(40), signature: Buffer.alloc(64, 1).toString('base64'), bytes: 1234, architecture: 'arm64', cloudBuildId: 'cloud-build', artifactId: 'notarized-artifact' };

test('feed and manifest reference the same immutable archive and Cloud build', () => {
  const { manifest, appcast } = releaseFiles(valid);
  assert.equal(releaseOrigin, 'https://whatsrusty.apps.balaj.me');
  assert.equal(manifest.file, 'WhatsRusty-1.1-20261004.2.zip');
  assert.equal(manifest.cloudBuildId, valid.cloudBuildId);
  assert.match(appcast, /downloads\/WhatsRusty-1\.1-20261004\.2\.zip/);
  assert.match(appcast, /<title>WhatsRusty Updates<\/title>/);
  assert.match(appcast, /<title>WhatsRusty 1\.1<\/title>/);
  assert.ok(appcast.includes(`${releaseOrigin}/downloads/${manifest.file}`));
  assert.equal((appcast.match(/<link>https:\/\/whatsrusty\.apps\.balaj\.me<\/link>/g) ?? []).length, 2);
  assert.equal(appcast.includes('said.apps.balaj.me'), false);
  assert.match(appcast, /<sparkle:minimumSystemVersion>27\.0\.0</);
  assert.match(appcast, /<sparkle:version>20261004\.2</);
});

test('accepts renamed and immutable historical archive names without accepting unsafe paths', () => {
  for (const file of ['WhatsRusty-1.1-20261006.zip', 'WhatsRusty-1.0-20261004.1.zip']) {
    assert.ok(releaseArchivePattern.test(file), file);
  }
  for (const file of ['WhatsRusty Dev-1.1-20261006.zip', '../WhatsRusty-1.1-20261006.zip', 'WhatsRusty-1.0-20261004.1.zip/extra', 'OtherApp-1.1-20261006.zip']) {
    assert.equal(releaseArchivePattern.test(file), false, file);
  }
});

test('rejects unsafe versions, invalid signatures and missing notarized provenance', () => {
  for (const patch of [{ version: '../release' }, { build: '1\"><script>' }, { signature: 'unsigned' }, { sha256: '' }, { bytes: -1 }, { architecture: 'x86_64' }, { cloudBuildId: '' }, { artifactId: '' }, { commitSha: 'main' }]) {
    assert.throws(() => releaseFiles({ ...valid, ...patch }));
  }
});
