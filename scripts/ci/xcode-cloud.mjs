#!/usr/bin/env node
import { createPrivateKey, sign } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, rename, rm, stat, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { pathToFileURL } from 'node:url';

const API = 'https://api.appstoreconnect.apple.com';
const NOTARIZED = 'STAPLED_NOTARIZED_ARCHIVE';
const delay = ms => new Promise(resolveDelay => setTimeout(resolveDelay, ms));
const resourcePath = (type, id) => `/v1/${type}/${encodeURIComponent(id)}`;

export function createJWT(env, nowSeconds = Math.floor(Date.now() / 1000)) {
  for (const name of ['ASC_KEY_ID', 'ASC_ISSUER_ID', 'ASC_PRIVATE_KEY']) {
    if (!env[name]) throw new Error(`Missing ${name}.`);
  }
  const header = Buffer.from(JSON.stringify({ alg: 'ES256', kid: env.ASC_KEY_ID, typ: 'JWT' })).toString('base64url');
  const payload = Buffer.from(JSON.stringify({ iss: env.ASC_ISSUER_ID, iat: nowSeconds - 30, exp: nowSeconds + 600, aud: 'appstoreconnect-v1' })).toString('base64url');
  let key;
  try {
    key = createPrivateKey(env.ASC_PRIVATE_KEY.replace(/\\n/g, '\n'));
    if (key.asymmetricKeyType !== 'ec' || key.asymmetricKeyDetails?.namedCurve !== 'prime256v1') throw new Error('Unsupported key.');
  } catch {
    throw new Error('ASC_PRIVATE_KEY must be an App Store Connect ES256 private key.');
  }
  const signature = sign('sha256', Buffer.from(`${header}.${payload}`), { key, dsaEncoding: 'ieee-p1363' }).toString('base64url');
  return `${header}.${payload}.${signature}`;
}

export function createClient({ env = process.env, fetchImpl = fetch, sleep = delay, now = Date.now, deadline = Infinity } = {}) {
  async function request(path, { method = 'GET', body } = {}) {
    const url = new URL(path, API);
    if (url.origin !== API || !url.pathname.startsWith('/v1/')) throw new Error('Refusing an unexpected App Store Connect API URL.');
    for (let attempt = 0; ; attempt += 1) {
      if (now() >= deadline) throw new Error('Xcode Cloud deadline exceeded.');
      const token = createJWT(env, Math.floor(now() / 1000));
      let response;
      try {
        response = await fetchImpl(url.href, {
          method,
          headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
          ...(body ? { body: JSON.stringify(body) } : {}),
          redirect: 'error',
          signal: AbortSignal.timeout(Math.max(1, Math.min(30_000, deadline - now()))),
        });
      } catch {
        // Do not retry POST: a lost response can still mean the build started.
        throw new Error(`App Store Connect ${method} request failed. ${method === 'POST' ? 'Check for an existing build before retrying.' : ''}`.trim());
      }
      if (!response.ok) {
        if (method === 'GET' && attempt < 3 && (response.status === 429 || response.status >= 500)) {
          const retryAfter = Number(response.headers.get('retry-after'));
          const waitMs = Math.min(60_000, Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter * 1000 : (attempt + 1) * 5000);
          await sleep(Math.min(waitMs, Math.max(0, deadline - now())));
          continue;
        }
        // Apple error bodies can include user input. Keep credentials and URLs out of logs.
        throw new Error(`App Store Connect ${method} request returned HTTP ${response.status}.`);
      }
      try { return await response.json(); } catch { throw new Error('App Store Connect returned invalid JSON.'); }
    }
  }

  async function list(path) {
    const data = [];
    const visited = new Set();
    while (path) {
      if (visited.has(path)) throw new Error('App Store Connect pagination repeated a page.');
      visited.add(path);
      const page = await request(path);
      if (!Array.isArray(page.data)) throw new Error('Expected an App Store Connect resource list.');
      data.push(...page.data);
      path = page.links?.next;
    }
    return data;
  }
  return { request, list, fetchImpl, sleep, now, deadline };
}

function includedResource(document, relationship) {
  const linkage = document.data?.relationships?.[relationship]?.data;
  return document.included?.find(item => item.id === linkage?.id && item.type === linkage?.type);
}

export async function preflight(client, workflowId) {
  const document = await client.request(`${resourcePath('ciWorkflows', workflowId)}?include=repository,product,xcodeVersion,macOsVersion`);
  const workflow = document.data;
  if (workflow?.id !== workflowId || workflow.type !== 'ciWorkflows') throw new Error('Unexpected workflow response.');
  const attributes = workflow.attributes ?? {};
  if (attributes.isEnabled !== true || attributes.clean !== true || attributes.isLockedForEditing !== true) {
    throw new Error('Release workflow must be enabled, clean, and locked for editing.');
  }
  if (attributes.containerFilePath !== 'WhatsRusty.xcodeproj') throw new Error('Release workflow must use WhatsRusty.xcodeproj.');
  const product = includedResource(document, 'product');
  if (!product?.id || product.type !== 'ciProducts') throw new Error('Release workflow has no verifiable Xcode Cloud product.');
  // Apple may include only a bundleIds stub; the linked app supplies its identifier.
  const productDocument = await client.request(`${resourcePath('ciProducts', product.id)}?include=bundleId,app`);
  if (productDocument.data?.id !== product.id || productDocument.data.type !== 'ciProducts') throw new Error('Unexpected Xcode Cloud product response.');
  const bundleId = includedResource(productDocument, 'bundleId');
  const app = includedResource(productDocument, 'app');
  const bundleIdentifier = bundleId?.attributes?.identifier;
  const appIdentifier = app?.type === 'apps' ? app.attributes?.bundleId : undefined;
  const expectedIdentifier = 'me.balaj.WhatsRusty';
  if (bundleId?.type !== 'bundleIds'
    || (bundleIdentifier != null && bundleIdentifier !== expectedIdentifier)
    || (productDocument.data.relationships?.app?.data && appIdentifier !== expectedIdentifier)
    || (bundleIdentifier !== expectedIdentifier && appIdentifier !== expectedIdentifier)) {
    throw new Error('Release workflow product must use me.balaj.WhatsRusty.');
  }
  // The app has no XCTest target; cargo tests gate the release in GitHub Actions instead.
  if (!attributes.actions?.some(action => action.actionType === 'ARCHIVE' && action.scheme === 'WhatsRusty' && action.platform === 'MACOS' && action.isRequiredToPass === true)) {
    throw new Error('Release workflow needs a required macOS ARCHIVE action using the WhatsRusty scheme.');
  }
  const xcode = includedResource(document, 'xcodeVersion');
  const macOS = includedResource(document, 'macOsVersion');
  // Apple's version fields contain build IDs; names carry the marketing versions.
  if (!/^Xcode 27(?:[.\s]|$)/.test(xcode?.attributes?.name ?? '')) throw new Error('Release workflow must select Xcode 27.');
  // The build host and SDK have separate versions. Xcode 27 can run on macOS 26;
  // the cloud pre-xcodebuild hook verifies the required macOS 27 SDK directly.
  if (!macOS?.attributes?.version) throw new Error('Release workflow must select a macOS build environment.');
  const repositoryId = workflow.relationships?.repository?.data?.id;
  if (!repositoryId) throw new Error('Release workflow has no connected repository.');
  return { workflowId, workflowName: attributes.name, repositoryId, xcodeName: xcode.attributes.name, xcodeBuild: xcode.attributes.version, macOSName: macOS.attributes.name, macOSBuild: macOS.attributes.version };
}

export function validateReleaseInput({ workflowId, ref, sha, output }) {
  if (!workflowId) throw new Error('Provide --workflow-id.');
  if (!/^refs\/(tags|heads)\/[^\s]+$/.test(ref ?? '')) throw new Error('Provide a canonical Git ref with --ref (refs/tags/... or refs/heads/...).');
  if (!/^[a-f0-9]{40}$/i.test(sha ?? '')) throw new Error('Provide the full expected commit SHA with --sha.');
  if (!output) throw new Error('Provide --output.');
}

export async function startBuild(client, { workflowId, repositoryId, ref }) {
  // GitHub can trigger Actions before Apple's repository mirror indexes the tag.
  // Retry only discovery GETs; the build-creation POST below remains one-shot.
  const refDeadline = Math.min(client.deadline, client.now() + 300_000);
  let matches;
  for (;;) {
    if (client.now() >= refDeadline) throw new Error('Release ref did not appear in Xcode Cloud before the discovery deadline. Confirm repository access.');
    const references = await client.list(`${resourcePath('scmRepositories', repositoryId)}/gitReferences?limit=200`);
    matches = references.filter(reference => reference.attributes?.canonicalName === ref && reference.attributes?.isDeleted !== true);
    if (matches.length > 1) throw new Error('Expected exactly one matching Git ref in Xcode Cloud.');
    if (matches.length === 1) break;
    await client.sleep(Math.max(0, Math.min(10_000, refDeadline - client.now())));
  }
  const document = await client.request('/v1/ciBuildRuns', {
    method: 'POST',
    body: { data: { type: 'ciBuildRuns', attributes: { clean: true }, relationships: {
      workflow: { data: { type: 'ciWorkflows', id: workflowId } },
      sourceBranchOrTag: { data: { type: 'scmGitReferences', id: matches[0].id } },
    } } },
  });
  if (!document.data?.id || document.data.type !== 'ciBuildRuns') throw new Error('Apple did not return a build ID. Check for an existing build before retrying.');
  return document.data.id;
}

export async function waitForBuild(client, { buildId, workflowId, ref, sha, pollMs = 30_000, log = () => {} }) {
  let previous;
  while (client.now() < client.deadline) {
    const document = await client.request(`${resourcePath('ciBuildRuns', buildId)}?include=workflow,sourceBranchOrTag`);
    const build = document.data;
    if (build?.id !== buildId || build.type !== 'ciBuildRuns') throw new Error('Unexpected build response.');
    if (build.relationships?.workflow?.data?.id !== workflowId) throw new Error('Build belongs to a different Xcode Cloud workflow.');
    const attributes = build.attributes ?? {};
    const commit = attributes.sourceCommit?.commitSha;
    if (commit && commit.toLowerCase() !== sha.toLowerCase()) throw new Error('Xcode Cloud built a different commit than the requested release.');
    const reference = includedResource(document, 'sourceBranchOrTag');
    if (reference?.attributes?.canonicalName && reference.attributes.canonicalName !== ref) throw new Error('Xcode Cloud built a different Git ref.');
    const status = `${attributes.executionProgress}/${attributes.completionStatus ?? ''}`;
    if (status !== previous) { log(`Xcode Cloud build ${buildId}: ${status}`); previous = status; }
    if (attributes.executionProgress === 'COMPLETE') {
      if (attributes.completionStatus !== 'SUCCEEDED') throw new Error(`Xcode Cloud build did not succeed (${attributes.completionStatus ?? 'unknown'}).`);
      if (!commit) throw new Error('Completed Xcode Cloud build has no verifiable source commit.');
      if (reference?.attributes?.canonicalName !== ref) throw new Error('Completed Xcode Cloud build has no matching source Git ref.');
      return build;
    }
    if (attributes.completionStatus && attributes.completionStatus !== 'SUCCEEDED') throw new Error(`Xcode Cloud build did not succeed (${attributes.completionStatus}).`);
    await client.sleep(Math.min(pollMs, Math.max(0, client.deadline - client.now())));
  }
  throw new Error(`Timed out waiting for Xcode Cloud build ${buildId}. Resume using download --build-id; do not start a duplicate build.`);
}

export async function notarizedArtifact(client, buildId) {
  const actions = await client.list(`${resourcePath('ciBuildRuns', buildId)}/actions?limit=200`);
  if (!actions.length || actions.some(action => action.attributes?.executionProgress !== 'COMPLETE' || action.attributes?.completionStatus !== 'SUCCEEDED')) {
    throw new Error('Every Xcode Cloud action must complete successfully before publishing.');
  }
  if (!actions.some(action => action.attributes?.actionType === 'ARCHIVE')) throw new Error('Completed build is missing its ARCHIVE action.');
  const artifacts = [];
  for (const action of actions.filter(action => action.attributes.actionType === 'ARCHIVE')) {
    artifacts.push(...await client.list(`${resourcePath('ciBuildActions', action.id)}/artifacts?limit=200`));
  }
  const matches = artifacts.filter(artifact => artifact.attributes?.fileType === NOTARIZED);
  if (matches.length !== 1) throw new Error('Expected exactly one STAPLED_NOTARIZED_ARCHIVE artifact. Configure the Notarize post-action; unsigned archive exports cannot be published.');
  const artifact = matches[0];
  if (!artifact.attributes.downloadUrl || !Number.isSafeInteger(artifact.attributes.fileSize) || artifact.attributes.fileSize <= 0) throw new Error('Notarized artifact is missing its download URL or size.');
  let url;
  try { url = new URL(artifact.attributes.downloadUrl); } catch { throw new Error('Notarized artifact has an invalid download URL.'); }
  if (url.protocol !== 'https:' || url.username || url.password) throw new Error('Notarized artifact download must use HTTPS.');
  return artifact;
}

export async function downloadArtifact(client, artifact, output) {
  output = resolve(output);
  await mkdir(dirname(output), { recursive: true });
  try { await stat(output); throw new Error('Output already exists; choose a new output path.'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  const partial = `${output}.partial`;
  let created = false;
  try {
    const response = await client.fetchImpl(artifact.attributes.downloadUrl, {
      // The download URL is already authorized. Never forward the ASC bearer token.
      redirect: 'follow', signal: AbortSignal.timeout(Math.max(1, Math.min(600_000, client.deadline - client.now()))),
    });
    if (!response.ok || !response.body) throw new Error('Notarized artifact download failed.');
    const stream = createWriteStream(partial, { flags: 'wx', mode: 0o600 });
    stream.on('open', () => { created = true; });
    await pipeline(Readable.fromWeb(response.body), stream);
    if ((await stat(partial)).size !== artifact.attributes.fileSize) throw new Error('Downloaded notarized artifact size does not match Apple metadata.');
    await rename(partial, output);
    return output;
  } catch (error) {
    if (created) await rm(partial, { force: true });
    if (error.message?.startsWith('Downloaded notarized') || error.code === 'EEXIST') throw error;
    throw new Error('Notarized artifact download failed.');
  }
}

function argumentsFrom(argv) {
  const [command, ...rest] = argv;
  const allowed = new Set(['workflow-id', 'ref', 'sha', 'build-id', 'output', 'result', 'timeout-seconds', 'poll-seconds']);
  const options = {};
  for (let index = 0; index < rest.length; index += 2) {
    const name = rest[index]?.replace(/^--/, '');
    if (!rest[index]?.startsWith('--') || !allowed.has(name) || !rest[index + 1] || rest[index + 1].startsWith('--') || Object.hasOwn(options, name)) throw new Error('Invalid CLI arguments.');
    options[name] = rest[index + 1];
  }
  return { command, options };
}

export async function main(argv = process.argv.slice(2), env = process.env) {
  const { command, options } = argumentsFrom(argv);
  if (!['preflight', 'start', 'download'].includes(command)) throw new Error('Usage: xcode-cloud.mjs preflight|start|download --workflow-id ID [--ref REF --sha SHA --output PATH --result PATH --build-id ID --timeout-seconds 7200]');
  if (!options['workflow-id']) throw new Error('Provide --workflow-id.');
  const timeout = Number(options['timeout-seconds'] ?? 7200);
  const poll = Number(options['poll-seconds'] ?? 30);
  if (!Number.isFinite(timeout) || timeout <= 0 || timeout > 21_600 || !Number.isFinite(poll) || poll < 1 || poll > 60) throw new Error('Invalid timeout or polling interval.');
  const client = createClient({ env, deadline: Date.now() + timeout * 1000 });
  const configuration = await preflight(client, options['workflow-id']);
  if (command === 'preflight') { process.stdout.write(`${JSON.stringify(configuration, null, 2)}\n`); return configuration; }
  const input = { workflowId: options['workflow-id'], ref: options.ref, sha: options.sha, output: options.output };
  validateReleaseInput(input);
  let buildId = options['build-id'];
  if (command === 'start') {
    if (buildId) throw new Error('Use download to resume an existing build.');
    buildId = await startBuild(client, { ...input, repositoryId: configuration.repositoryId });
    process.stderr.write(`Started Xcode Cloud build ${buildId}.\n`);
  } else if (!buildId) throw new Error('Provide --build-id to resume an existing build.');
  const build = await waitForBuild(client, { ...input, buildId, pollMs: poll * 1000, log: message => process.stderr.write(`${message}\n`) });
  const artifact = await notarizedArtifact(client, buildId);
  const outputPath = await downloadArtifact(client, artifact, input.output);
  const result = { buildId, buildNumber: build.attributes.number, workflowId: input.workflowId, commitSha: build.attributes.sourceCommit.commitSha, ref: input.ref, artifactId: artifact.id, artifactType: NOTARIZED, artifactFileName: artifact.attributes.fileName, artifactFileSize: artifact.attributes.fileSize, outputPath };
  if (options.result) { await mkdir(dirname(resolve(options.result)), { recursive: true }); await writeFile(options.result, `${JSON.stringify(result, null, 2)}\n`, { flag: 'wx' }); }
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { process.stderr.write(`${error.message}\n`); process.exitCode = 1; });
}
