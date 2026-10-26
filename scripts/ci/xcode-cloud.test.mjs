import assert from 'node:assert/strict';
import { generateKeyPairSync, verify } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { createClient, createJWT, downloadArtifact, notarizedArtifact, preflight, startBuild, validateReleaseInput, waitForBuild } from './xcode-cloud.mjs';

const { privateKey, publicKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
const env = { ASC_KEY_ID: 'test-key', ASC_ISSUER_ID: 'test-issuer', ASC_PRIVATE_KEY: privateKey.export({ type: 'pkcs8', format: 'pem' }) };
const sha = 'a'.repeat(40);
const ref = 'refs/tags/v1.1.0';
const json = data => new Response(JSON.stringify(data), { status: 200, headers: { 'content-type': 'application/json' } });
const completeAction = (id, actionType) => ({ id, type: 'ciBuildActions', attributes: { actionType, executionProgress: 'COMPLETE', completionStatus: 'SUCCEEDED' } });
const artifact = { id: 'artifact-1', type: 'ciArtifacts', attributes: { fileType: 'STAPLED_NOTARIZED_ARCHIVE', fileName: 'WhatsRusty.zip', fileSize: 7, downloadUrl: 'https://downloads.example.test/app.zip?secret=redacted' } };
const buildDocument = (attributes = {}, workflowId = 'workflow-1') => ({
  data: { id: 'build-1', type: 'ciBuildRuns', attributes: { number: 2, sourceCommit: { commitSha: sha }, executionProgress: 'COMPLETE', completionStatus: 'SUCCEEDED', ...attributes }, relationships: { workflow: { data: { id: workflowId, type: 'ciWorkflows' } }, sourceBranchOrTag: { data: { id: 'ref-1', type: 'scmGitReferences' } } } },
  included: [{ id: 'ref-1', type: 'scmGitReferences', attributes: { canonicalName: ref } }],
});
const macOSTestDestination = () => ({ kind: 'MAC', runtimeName: 'macOS Golden Gate 27 (26A428)', runtimeIdentifier: 'macOS26A428:stable' });
const workflowDocument = () => ({
  data: { id: 'workflow-1', type: 'ciWorkflows', attributes: { name: 'Direct Release', isEnabled: true, clean: true, isLockedForEditing: true, containerFilePath: 'WhatsRusty.xcodeproj', actions: [{ actionType: 'ARCHIVE', scheme: 'WhatsRusty', platform: 'MACOS', isRequiredToPass: true }] }, relationships: { repository: { data: { id: 'repository-1', type: 'scmRepositories' } }, product: { data: { id: 'product-1', type: 'ciProducts' } }, xcodeVersion: { data: { id: 'xcode-1', type: 'ciXcodeVersions' } }, macOsVersion: { data: { id: 'macos-1', type: 'ciMacOsVersions' } } } },
  included: [{ id: 'product-1', type: 'ciProducts', attributes: { name: 'WhatsRusty', productType: 'APP' }, relationships: { bundleId: { data: { id: 'bundle-1', type: 'bundleIds' } } } }, { id: 'xcode-1', type: 'ciXcodeVersions', attributes: { name: 'Xcode 27', version: '27A266a', testDestinations: [macOSTestDestination()] } }, { id: 'macos-1', type: 'ciMacOsVersions', attributes: { name: 'macOS Golden Gate 27', version: '26A428' } }],
});
// JSON:API shape from Apple's current CiProductResponse and BundleId schemas.
const productDocument = () => ({
  data: { id: 'product-1', type: 'ciProducts', attributes: { name: 'WhatsRusty', productType: 'APP' }, relationships: { bundleId: { data: { id: 'bundle-1', type: 'bundleIds' } } } },
  included: [{ id: 'bundle-1', type: 'bundleIds', attributes: { identifier: 'me.balaj.WhatsRusty', name: 'WhatsRusty', platform: 'MAC_OS' } }],
  links: { self: 'https://api.appstoreconnect.apple.com/v1/ciProducts/product-1' },
});
// Observed Apple response: the bundle ID is a stub; the linked app has attributes.
const productWithBundleStub = () => ({
  data: { id: 'product-1', type: 'ciProducts', attributes: { name: 'WhatsRusty', productType: 'APP' }, relationships: { bundleId: { data: { id: 'me.balaj.WhatsRusty', type: 'bundleIds' } }, app: { data: { id: '6818962070', type: 'apps' } } } },
  included: [{ id: 'me.balaj.WhatsRusty', type: 'bundleIds', links: { self: 'https://api.appstoreconnect.apple.com/v1/bundleIds/me.balaj.WhatsRusty' } }, { id: '6818962070', type: 'apps', attributes: { bundleId: 'me.balaj.WhatsRusty', name: 'WhatsRusty by Talha' } }],
  links: { self: 'https://api.appstoreconnect.apple.com/v1/ciProducts/product-1' },
});
const configurationClient = (workflow, product, calls = []) => createClient({ env, fetchImpl: async url => {
  calls.push(url);
  if (url === 'https://api.appstoreconnect.apple.com/v1/ciWorkflows/workflow-1?include=repository,product,xcodeVersion,macOsVersion') return json(workflow);
  assert.equal(url, 'https://api.appstoreconnect.apple.com/v1/ciProducts/product-1?include=bundleId,app');
  return json(product);
} });

test('JWT uses ES256, short lifetime, and a JOSE-compatible signature', () => {
  const token = createJWT(env, 1000);
  const [header, payload, signature] = token.split('.');
  assert.deepEqual(JSON.parse(Buffer.from(header, 'base64url')), { alg: 'ES256', kid: 'test-key', typ: 'JWT' });
  assert.deepEqual(JSON.parse(Buffer.from(payload, 'base64url')), { iss: 'test-issuer', iat: 970, exp: 1600, aud: 'appstoreconnect-v1' });
  assert.equal(Buffer.from(signature, 'base64url').length, 64);
  assert.equal(verify('sha256', Buffer.from(`${header}.${payload}`), { key: publicKey, dsaEncoding: 'ieee-p1363' }, Buffer.from(signature, 'base64url')), true);
  assert.throws(() => createJWT({ ...env, ASC_PRIVATE_KEY: 'bad secret private key' }), /must be an App Store Connect ES256/);
  assert.throws(() => createJWT({}), /Missing ASC_KEY_ID/);
});

test('preflight checks app identity, required tests/archive, cloud version, and release protections', async () => {
  const fixture = workflowDocument();
  const product = productDocument();
  const calls = [];
  const client = configurationClient(fixture, product, calls);
  assert.equal((await preflight(client, 'workflow-1')).repositoryId, 'repository-1');
  assert.equal(calls.length, 2);
  fixture.data.attributes.clean = false;
  await assert.rejects(preflight(client, 'workflow-1'), /enabled, clean, and locked/);
  fixture.data.attributes.clean = true;
  product.included[0].attributes.identifier = 'me.balaj.WhatsRusty.dev';
  await assert.rejects(preflight(client, 'workflow-1'), /me.balaj.WhatsRusty/);
  product.included[0].attributes.identifier = 'me.balaj.WhatsRusty';
  fixture.data.attributes.actions[0].scheme = 'Other';
  await assert.rejects(preflight(client, 'workflow-1'), /ARCHIVE action/);
});

test('preflight rejects missing or mismatched product and bundle ID relationships', async () => {
  const fixture = workflowDocument();
  const product = productDocument();
  const client = configurationClient(fixture, product);
  // A misleading legacy field must not bypass the actual linked identifier.
  product.data.attributes.bundleId = 'me.balaj.WhatsRusty';
  product.included[0].attributes.identifier = 'me.balaj.WhatsRusty.dev';
  await assert.rejects(preflight(client, 'workflow-1'), /me.balaj.WhatsRusty/);
  product.included[0].attributes.identifier = 'me.balaj.WhatsRusty';
  product.included[0].id = 'unrelated-bundle';
  await assert.rejects(preflight(client, 'workflow-1'), /me.balaj.WhatsRusty/);
  product.included[0].id = 'bundle-1';
  product.data.relationships.bundleId.data = null;
  await assert.rejects(preflight(client, 'workflow-1'), /me.balaj.WhatsRusty/);
  product.data.id = 'different-product';
  await assert.rejects(preflight(client, 'workflow-1'), /Unexpected Xcode Cloud product/);
  fixture.data.relationships.product.data = null;
  await assert.rejects(preflight(client, 'workflow-1'), /no verifiable Xcode Cloud product/);
});

test('preflight accepts the live bundle ID stub using the linked production app', async () => {
  const client = configurationClient(workflowDocument(), productWithBundleStub());
  assert.equal((await preflight(client, 'workflow-1')).repositoryId, 'repository-1');
});

test('preflight rejects conflicting identifiers and unverifiable app linkage with bundle stubs', async () => {
  const cases = [
    ['development app', product => { product.included[1].attributes.bundleId = 'me.balaj.WhatsRusty.dev'; }],
    ['conflicting bundle identifier', product => { product.included[0].attributes = { identifier: 'me.balaj.WhatsRusty.dev' }; }],
    ['unlinked app', product => { product.data.relationships.app.data.id = 'different-app'; }],
    ['wrong app resource type', product => { product.included[1].type = 'bundleIds'; }],
    ['missing app identifier', product => { delete product.included[1].attributes.bundleId; }],
    ['missing app relationship', product => { product.data.relationships.app.data = null; }],
    ['unlinked bundle stub', product => { product.included[0].id = 'different-bundle'; }],
  ];
  for (const [name, change] of cases) {
    const product = productWithBundleStub();
    change(product);
    await assert.rejects(preflight(configurationClient(workflowDocument(), product), 'workflow-1'), /me.balaj.WhatsRusty/, name);
  }
  // A fully populated bundle resource does not excuse a contradictory linked app.
  const product = productWithBundleStub();
  product.included[0].attributes = { identifier: 'me.balaj.WhatsRusty' };
  product.included[1].attributes.bundleId = 'me.balaj.WhatsRusty.dev';
  await assert.rejects(preflight(configurationClient(workflowDocument(), product), 'workflow-1'), /me.balaj.WhatsRusty/);
});

test('preflight permits an Xcode 27 host on macOS 26 while requiring a selected environment', async () => {
  const fixture = workflowDocument();
  fixture.included[2].attributes.name = 'macOS 26.6';
  fixture.included[2].attributes.version = '25G100';
  const client = configurationClient(fixture, productDocument());
  const configuration = await preflight(client, 'workflow-1');
  assert.equal(configuration.macOSName, 'macOS 26.6');
  assert.equal(configuration.macOSBuild, '25G100');
  fixture.included[2].attributes.version = null;
  await assert.rejects(preflight(client, 'workflow-1'), /select a macOS build environment/);
});

test('preflight uses marketing names and accepts macOS 26 and 27 test destinations', async () => {
  const fixture = workflowDocument();
  const client = configurationClient(fixture, productWithBundleStub());
  const configuration = await preflight(client, 'workflow-1');
  assert.equal(configuration.xcodeName, 'Xcode 27');
  assert.equal(configuration.xcodeBuild, '27A266a');
  assert.equal(configuration.macOSName, 'macOS Golden Gate 27');
  assert.equal(configuration.macOSBuild, '26A428');
  fixture.included[1].attributes.name = 'Xcode 26';
  await assert.rejects(preflight(client, 'workflow-1'), /select Xcode 27/);
  fixture.included[1].attributes.name = 'Xcode 270';
  await assert.rejects(preflight(client, 'workflow-1'), /select Xcode 27/);
  fixture.included[1].attributes.name = 'Xcode 27.1';
  assert.equal((await preflight(client, 'workflow-1')).xcodeName, 'Xcode 27.1');
});

test('start selects an exact canonical ref across pages and sends supported relationships', async () => {
  const calls = [];
  const client = createClient({ env, fetchImpl: async (url, options) => {
    calls.push({ url, options });
    if (url.includes('gitReferences')) return json({ data: [{ id: 'other-ref', attributes: { canonicalName: 'refs/heads/main' } }], links: { next: 'https://api.appstoreconnect.apple.com/v1/refs?page=2' } });
    if (url.includes('page=2')) return json({ data: [{ id: 'ref-1', attributes: { canonicalName: ref, isDeleted: false } }] });
    assert.equal(options.method, 'POST');
    const body = JSON.parse(options.body);
    assert.deepEqual(body, { data: { type: 'ciBuildRuns', attributes: { clean: true }, relationships: { workflow: { data: { type: 'ciWorkflows', id: 'workflow-1' } }, sourceBranchOrTag: { data: { type: 'scmGitReferences', id: 'ref-1' } } } } });
    return json({ data: { id: 'build-1', type: 'ciBuildRuns' } });
  } });
  assert.equal(await startBuild(client, { workflowId: 'workflow-1', repositoryId: 'repository-1', ref }), 'build-1');
  assert.equal(calls.length, 3);
});

test('start waits for Apple to index a new tag and creates exactly one build', async () => {
  let now = 0, reads = 0, posts = 0;
  const client = createClient({ env, now: () => now, deadline: 100_000,
    sleep: async ms => { now += ms; }, fetchImpl: async (_, options) => {
      if (options.method === 'GET') return json({ data: ++reads < 3 ? [] : [{ id: 'ref-1', attributes: { canonicalName: ref } }] });
      posts += 1;
      return json({ data: { id: 'build-1', type: 'ciBuildRuns' } });
    } });
  assert.equal(await startBuild(client, { workflowId: 'workflow-1', repositoryId: 'repository-1', ref }), 'build-1');
  assert.equal(reads, 3); assert.equal(posts, 1); assert.equal(now, 20_000);
});

test('start never posts for an absent, deleted or ambiguous ref', async () => {
  for (const references of [[], [{ id: 'deleted', attributes: { canonicalName: ref, isDeleted: true } }],
    ['a', 'b'].map(id => ({ id, attributes: { canonicalName: ref } }))]) {
    let now = 0, posts = 0;
    const client = createClient({ env, now: () => now, deadline: 20_000,
      sleep: async ms => { now += ms; }, fetchImpl: async (_, options) => {
        if (options.method === 'POST') posts += 1;
        return json({ data: references });
      } });
    await assert.rejects(startBuild(client, { workflowId: 'workflow-1', repositoryId: 'repository-1', ref }), /discovery deadline|exactly one/);
    assert.equal(posts, 0);
  }
});

test('POST failures never retry, while GET server failures can retry without exposing bodies', async () => {
  let calls = 0;
  const client = createClient({ env, fetchImpl: async () => { calls += 1; return new Response('sensitive Apple error body', { status: 503 }); }, sleep: async () => {} });
  await assert.rejects(client.request('/v1/ciBuildRuns', { method: 'POST', body: {} }), /HTTP 503/);
  assert.equal(calls, 1);
  await assert.rejects(client.request('/v1/ciBuildRuns/build-1'), /HTTP 503/);
  assert.equal(calls, 5);
});

test('pagination cannot send credentials to another host', async () => {
  let calls = 0;
  const client = createClient({ env, fetchImpl: async () => { calls += 1; return json({ data: [], links: { next: 'https://attacker.example/v1/steal' } }); } });
  await assert.rejects(client.list('/v1/scmRepositories/repo/gitReferences'), /unexpected App Store Connect API URL/);
  assert.equal(calls, 1);
});

test('polling waits for completion and verifies commit, workflow, and ref', async () => {
  let now = 0;
  let calls = 0;
  const client = createClient({ env, now: () => now, deadline: 100, sleep: async ms => { now += ms; }, fetchImpl: async () => json(buildDocument(++calls === 1 ? { executionProgress: 'RUNNING', completionStatus: null } : {})) });
  assert.equal((await waitForBuild(client, { buildId: 'build-1', workflowId: 'workflow-1', ref, sha, pollMs: 10 })).attributes.number, 2);
  assert.equal(calls, 2);
  const wrongCommit = createClient({ env, fetchImpl: async () => json(buildDocument({ sourceCommit: { commitSha: 'b'.repeat(40) } })) });
  await assert.rejects(waitForBuild(wrongCommit, { buildId: 'build-1', workflowId: 'workflow-1', ref, sha }), /different commit/);
  const wrongWorkflow = createClient({ env, fetchImpl: async () => json(buildDocument({}, 'workflow-2')) });
  await assert.rejects(waitForBuild(wrongWorkflow, { buildId: 'build-1', workflowId: 'workflow-1', ref, sha }), /different Xcode Cloud workflow/);
  const wrongRef = createClient({ env, fetchImpl: async () => json(buildDocument()) });
  await assert.rejects(waitForBuild(wrongRef, { buildId: 'build-1', workflowId: 'workflow-1', ref: 'refs/tags/v9.0.0', sha }), /different Git ref/);
});

test('polling has a deadline and tells CI how to resume', async () => {
  let now = 0;
  const client = createClient({ env, now: () => now, deadline: 20, sleep: async ms => { now += ms; }, fetchImpl: async () => json(buildDocument({ executionProgress: 'PENDING', completionStatus: null })) });
  await assert.rejects(waitForBuild(client, { buildId: 'build-1', workflowId: 'workflow-1', ref, sha, pollMs: 10 }), /Resume using download --build-id/);
});

test('only a stapled notarized artifact from a successful build is accepted', async () => {
  const actions = [completeAction('archive-1', 'ARCHIVE'), completeAction('test-1', 'TEST')];
  let artifacts = [{ ...artifact, attributes: { ...artifact.attributes, fileType: 'ARCHIVE_EXPORT' } }];
  const client = createClient({ env, fetchImpl: async url => json({ data: url.includes('/artifacts') ? artifacts : actions }) });
  await assert.rejects(notarizedArtifact(client, 'build-1'), /STAPLED_NOTARIZED_ARCHIVE/);
  artifacts = [artifact];
  assert.equal((await notarizedArtifact(client, 'build-1')).id, artifact.id);
  actions[1].attributes.completionStatus = 'FAILED';
  await assert.rejects(notarizedArtifact(client, 'build-1'), /Every Xcode Cloud action/);
  actions[1].attributes.completionStatus = 'SUCCEEDED';
  actions.shift();
  await assert.rejects(notarizedArtifact(client, 'build-1'), /missing its ARCHIVE action/);
});

test('download keeps ASC authorization off the presigned URL and validates size before publishing', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'whatsrusty-cloud-test-'));
  try {
    const output = join(directory, 'notarized.zip');
    const client = createClient({ env, fetchImpl: async (url, options) => {
      assert.equal(url, artifact.attributes.downloadUrl);
      assert.equal(options.headers, undefined);
      return new Response('zipdata');
    } });
    await downloadArtifact(client, artifact, output);
    assert.equal(await readFile(output, 'utf8'), 'zipdata');
    await assert.rejects(downloadArtifact(client, artifact, output), /already exists/);
    const badOutput = join(directory, 'bad.zip');
    await assert.rejects(downloadArtifact(client, { ...artifact, attributes: { ...artifact.attributes, fileSize: 8 } }, badOutput), /size does not match/);
    await assert.rejects(readFile(badOutput), { code: 'ENOENT' });
    await assert.rejects(readFile(`${badOutput}.partial`), { code: 'ENOENT' });
    await writeFile(`${badOutput}.partial`, 'another run');
    await assert.rejects(downloadArtifact(client, artifact, badOutput), { code: 'EEXIST' });
    assert.equal(await readFile(`${badOutput}.partial`, 'utf8'), 'another run');
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('download errors omit presigned URLs', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'whatsrusty-cloud-error-test-'));
  try {
    const client = createClient({ env, fetchImpl: async () => { throw new Error(artifact.attributes.downloadUrl); } });
    await assert.rejects(downloadArtifact(client, artifact, join(directory, 'app.zip')), { message: 'Notarized artifact download failed.' });
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test('release input rejects unpinned commits and noncanonical refs', () => {
  validateReleaseInput({ workflowId: 'workflow-1', ref, sha, output: 'release.zip' });
  assert.throws(() => validateReleaseInput({ workflowId: 'workflow-1', ref: 'v1.1.0', sha, output: 'release.zip' }), /canonical Git ref/);
  assert.throws(() => validateReleaseInput({ workflowId: 'workflow-1', ref, sha: 'main', output: 'release.zip' }), /full expected commit SHA/);
});

test('cloud hook preserves project settings and uses increasing integer cloud builds', async () => {
 const directory=await mkdtemp(join(tmpdir(),'whatsrusty-cloud-hook-'));
 try {
  execFileSync('git',['init','--quiet',directory],{stdio:'pipe'});
  await mkdir(join(directory,'WhatsRusty.xcodeproj'),{recursive:true});
  const project=join(directory,'WhatsRusty.xcodeproj/project.pbxproj');
  await writeFile(project,'CURRENT_PROJECT_VERSION = 1; MARKETING_VERSION = 0.1.0; PRODUCT_NAME = WhatsRusty;');
  const hook=fileURLToPath(new URL('../../ci_scripts/stamp-version.sh',import.meta.url));
  const hookEnv={...process.env,CI_PRIMARY_REPOSITORY_PATH:directory,CI_BUILD_NUMBER:'2',CI_TAG:'v0.2.0'};
  execFileSync('/bin/sh',[hook],{env:hookEnv,stdio:'pipe'});
  const output=await readFile(project,'utf8');
  assert.match(output,/CURRENT_PROJECT_VERSION = 2;/); assert.match(output,/MARKETING_VERSION = 0.2.0;/); assert.match(output,/PRODUCT_NAME = WhatsRusty;/);
  for (const patch of [{CI_BUILD_NUMBER:'0'},{CI_BUILD_NUMBER:'1.2'},{CI_TAG:'unsafe'}]) assert.throws(()=>execFileSync('/bin/sh',[hook],{env:{...hookEnv,...patch},stdio:'pipe'}));
  assert.equal(await readFile(project,'utf8'),output);
 } finally { await rm(directory,{recursive:true,force:true}); }
});
