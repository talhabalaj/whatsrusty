// Enhance the permanent latest-download links with verified release details.
export function releaseDetails(manifest) {
  const version = value => typeof value === 'string' && /^\d+(?:\.\d+){0,2}$/.test(value);
  if (!manifest || manifest.notarized !== true || manifest.architecture !== 'arm64' ||
      !Number.isSafeInteger(manifest.bytes) || manifest.bytes <= 0 ||
      !version(manifest.version) || !version(manifest.build) || !version(manifest.minimumMacOS) ||
      manifest.file !== `WhatsRusty-${manifest.version}-${manifest.build}.zip`) return null;
  return {
    href: '/downloads/latest.zip',
    detail: `Version ${manifest.version} · ${(manifest.bytes / 1_000_000).toFixed(1)} MB · Apple silicon · macOS ${manifest.minimumMacOS} or later`,
  };
}
