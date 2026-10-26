import { createHash, createPublicKey, verify } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { isDeepStrictEqual } from 'node:util';
import { releaseOrigin, releaseFiles } from './release-files.mjs';
import { githubReleaseStore, githubDownloads } from './github-release.mjs';

export const repoRoot = fileURLToPath(new URL('../..', import.meta.url));
const buildPattern = /^\d+(?:\.\d+){0,2}$/;
const freshRequest = { cache: 'no-store', headers: { 'Cache-Control': 'no-cache' } };

function compareBuilds(left, right) {
  const a = left.split('.').map(BigInt), b = right.split('.').map(BigInt);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0n) !== (b[i] ?? 0n)) return (a[i] ?? 0n) > (b[i] ?? 0n) ? 1 : -1;
  }
  return 0;
}

function requireForwardRelease(manifest, current) {
  if (current === null) return;
  if (!buildPattern.test(current?.build ?? '') || !/^[a-f0-9]{64}$/.test(current?.sha256 ?? '')) throw new Error('Current release manifest is invalid.');
  const comparison = compareBuilds(manifest.build, current.build);
  if (comparison < 0) throw new Error('Refusing to replace a newer release with an older build.');
  if (comparison === 0 && (current.sha256 !== manifest.sha256 || current.file !== manifest.file)) throw new Error('A different archive already uses this build number.');
}

function publicSigningKey(value) {
  if (typeof value !== 'string' || !/^[A-Za-z0-9+/]{43}=$/.test(value) || Buffer.from(value, 'base64').length !== 32) throw new Error('SPARKLE_PUBLIC_KEY must contain the release public Ed25519 key.');
  return createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), Buffer.from(value, 'base64')]), type: 'spki', format: 'der' });
}

// Sparkle 2.10 appends this exact comment block, and signs the bytes BEFORE it.
// Validate its real signature rather than mistaking an enclosure for a signed feed.
export function verifySignedFeed(feed, key) {
  const text = feed.toString('utf8');
  const block = /<!-- sparkle-signatures:\nedSignature: ([A-Za-z0-9+/]{86}==)\nlength: ([0-9]+)\n-->\n?$/.exec(text);
  if (!block) throw new Error('The appcast must be signed by Sparkle before publishing.');
  const content = Buffer.from(text.slice(0, block.index), 'utf8');
  if (String(content.length) !== block[2] || !verify(null, content, key, Buffer.from(block[1], 'base64'))) throw new Error('The signed appcast failed Ed25519 verification.');
  return content.toString('utf8');
}

function requireMatchingFeed(content, manifest, metadata) {
  const enclosures = [...content.matchAll(/<enclosure\s+([^>]+)>/g)];
  const attribute = name => new RegExp(`(?:^|\\s)${name}="([^"]+)"`).exec(enclosures[0]?.[1] ?? '')?.[1];
  if (enclosures.length !== 1 || attribute('url') !== `${releaseOrigin}/downloads/${manifest.file}` ||
      attribute('sparkle:edSignature') !== metadata.signature || attribute('length') !== String(manifest.bytes) ||
      !content.includes(`<sparkle:version>${manifest.build}</sparkle:version>`) ||
      !content.includes(`<sparkle:shortVersionString>${manifest.version}</sparkle:shortVersionString>`)) {
    throw new Error('Signed feed does not describe the verified archive.');
  }
}

async function verifyDownload(response, manifest) {
  if (!response.ok || !response.body) throw new Error(`Public download failed: HTTP ${response.status}`);
  const hash = createHash('sha256');
  let bytes = 0;
  for await (const chunk of response.body) { hash.update(chunk); bytes += chunk.length; }
  if (hash.digest('hex') !== manifest.sha256 || bytes !== manifest.bytes) throw new Error('Public archive hash or size does not match the verified release.');
}

export async function publishRelease(outputDirectory, { fetch: fetchRelease = globalThis.fetch, store = githubReleaseStore(), publicKey = process.env.SPARKLE_PUBLIC_KEY, settleDelayMs = 5000, settleAttempts = 12 } = {}) {
  const output = resolve(outputDirectory);
  const manifest = JSON.parse(readFileSync(join(output, 'manifest.json'), 'utf8'));
  const metadata = JSON.parse(readFileSync(join(output, 'metadata.json'), 'utf8'));
  if (!isDeepStrictEqual(releaseFiles(metadata).manifest, manifest)) throw new Error('Manifest does not match validated release metadata.');
  const archivePath = join(output, manifest.file);
  const archive = readFileSync(archivePath);
  if (archive.length !== manifest.bytes || createHash('sha256').update(archive).digest('hex') !== manifest.sha256) throw new Error('Archive hash or size changed after verification.');
  const key = publicSigningKey(publicKey);
  if (!verify(null, archive, key, Buffer.from(metadata.signature, 'base64'))) throw new Error('Archive failed Ed25519 verification.');
  const feed = readFileSync(join(output, 'appcast.xml'));
  requireMatchingFeed(verifySignedFeed(feed, key), manifest, metadata);
  const checksumPath = join(output, `${manifest.file}.sha256`);
  const checksum = `${manifest.sha256}  ${manifest.file}\n`;
  if (readFileSync(checksumPath, 'utf8') !== checksum) throw new Error('Archive checksum file is invalid.');
  const read = path => fetchRelease(`${releaseOrigin}/${path}`, freshRequest);
  const currentManifest = async () => {
    const response = await read('downloads/manifest.json');
    if (response.status === 404) return null;
    if (!response.ok) throw new Error('Could not verify the currently published build.');
    return response.json();
  };
  requireForwardRelease(manifest, await currentManifest());
  await store.begin(manifest.version, metadata.commitSha);
  const assets = [
    [manifest.file, archivePath],
    [`${manifest.file}.sha256`, checksumPath],
    ['manifest.json', join(output, 'manifest.json')],
    ['appcast.xml', join(output, 'appcast.xml')],
  ];
  for (const [name, file] of assets) {
    let existing = await store.read(name);
    if (existing.status === 404) {
      await store.put(name, file);
      existing = await store.read(name);
    }
    if (name === manifest.file) await verifyDownload(existing, manifest);
    else if (!existing.ok || !Buffer.from(await existing.arrayBuffer()).equals(readFileSync(file))) {
      throw new Error(`Immutable release asset differs: ${name}`);
    }
  }
  // Publish without making it latest. Verify anonymous downloads before the
  // single promotion that exposes both manifest and signed appcast together.
  await store.publish();
  const tagged = name => fetchRelease(`${githubDownloads}/download/v${manifest.version}/${name}`, freshRequest);
  await verifyDownload(await tagged(manifest.file), manifest);
  for (const [name, file] of assets.slice(1)) {
    const response = await tagged(name);
    if (!response.ok || !Buffer.from(await response.arrayBuffer()).equals(readFileSync(file))) throw new Error(`Public release asset differs: ${name}`);
  }
  requireForwardRelease(manifest, await currentManifest());
  await store.promote();
  // GitHub's latest-release redirect can serve the previous release for a few seconds after promotion.
  for (let attempt = 1; ; attempt++) {
    const manifestMatches = isDeepStrictEqual(await currentManifest(), manifest);
    const published = manifestMatches && await read('appcast.xml');
    const feedMatches = manifestMatches && published.ok && Buffer.from(await published.arrayBuffer()).equals(feed);
    if (feedMatches) break;
    if (attempt >= settleAttempts) throw new Error(manifestMatches ? 'Published appcast differs from the signed feed.' : 'Published manifest differs from the verified release.');
    await new Promise(done => setTimeout(done, settleDelayMs));
  }
  const downloadKey = `downloads/${manifest.file}`;
  const publication = { build: manifest.build, sha256: manifest.sha256, download: `${releaseOrigin}/${downloadKey}`, feed: `${releaseOrigin}/appcast.xml`, verifiedAt: new Date().toISOString() };
  writeFileSync(join(output, 'publication.json'), JSON.stringify(publication, null, 2) + '\n');
  return publication;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const publication = await publishRelease(process.argv[2] ?? 'build/ci/release');
  console.log(`Published and verified WhatsRusty build ${publication.build}.`);
}
