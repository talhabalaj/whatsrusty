import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

export const releaseOrigin = 'https://whatsrusty.apps.balaj.me';
export const releaseArchivePattern = /^WhatsRusty-\d+(?:\.\d+){0,2}-\d+(?:\.\d+){0,2}\.zip$/;
const xml = value => String(value).replace(/[&<>"']/g, character => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' })[character]);

export function releaseFiles(input) {
  for (const key of ['version', 'build', 'minimumMacOS']) {
    if (!/^\d+(?:\.\d+){0,2}$/.test(input[key] ?? '')) throw new Error(`Invalid ${key}.`);
  }
  if (!/^[a-f0-9]{64}$/.test(input.sha256 ?? '')) throw new Error('Invalid release SHA-256.');
  if (!/^[a-f0-9]{40}$/.test(input.commitSha ?? '')) throw new Error('Invalid source commit.');
  if (!/^[A-Za-z0-9+/]{86}==$/.test(input.signature ?? '') || Buffer.from(input.signature, 'base64').length !== 64) throw new Error('Invalid update signature.');
  if (!Number.isSafeInteger(input.bytes) || input.bytes < 1) throw new Error('Invalid archive size.');
  if (input.architecture !== 'arm64') throw new Error('The release must target Apple silicon.');
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(input.notaryId ?? '')) throw new Error('The Apple notarization submission ID is required.');
  if (!input.builder) throw new Error('Build provenance is required.');
  const file = `WhatsRusty-${input.version}-${input.build}.zip`;
  const manifest = { version: input.version, build: input.build, file, sha256: input.sha256, bytes: input.bytes, minimumMacOS: input.minimumMacOS, architecture: input.architecture, notarized: true, commitSha: input.commitSha, notaryId: input.notaryId, builder: input.builder };
  const minimum = input.minimumMacOS.split('.');
  while (minimum.length < 3) minimum.push('0');
  const appcast = `<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>WhatsRusty Updates</title>
    <link>${releaseOrigin}</link>
    <description>WhatsRusty releases for macOS.</description>
    <language>en</language>
    <item>
      <title>WhatsRusty ${xml(input.version)}</title>
      <link>${releaseOrigin}</link>
      <sparkle:version>${xml(input.build)}</sparkle:version>
      <sparkle:shortVersionString>${xml(input.version)}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>${minimum.join('.')}</sparkle:minimumSystemVersion>
      <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>
      <enclosure url="${releaseOrigin}/downloads/${file}" sparkle:edSignature="${input.signature}" length="${input.bytes}" type="application/zip" />
    </item>
  </channel>
</rss>
`;
  return { manifest, appcast };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [metadataPath, outputDirectory] = process.argv.slice(2);
  if (!metadataPath || !outputDirectory) throw new Error('Usage: release-files.mjs METADATA_JSON OUTPUT_DIRECTORY');
  const { manifest, appcast } = releaseFiles(JSON.parse(readFileSync(metadataPath, 'utf8')));
  mkdirSync(outputDirectory, { recursive: true });
  writeFileSync(join(outputDirectory, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  writeFileSync(join(outputDirectory, 'appcast.xml'), appcast);
  writeFileSync(join(outputDirectory, `${manifest.file}.sha256`), `${manifest.sha256}  ${manifest.file}\n`);
}
