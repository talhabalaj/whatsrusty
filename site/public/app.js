import { releaseDetails } from './release.mjs';

async function refreshRelease() {
  try {
    const response = await fetch('/downloads/manifest.json', { cache: 'no-store', signal: AbortSignal.timeout(5000) });
    if (!response.ok) return;
    const release = releaseDetails(await response.json());
    if (!release) return;
    document.querySelectorAll('[data-release-link]').forEach(link => {
      link.href = release.href;
      link.setAttribute('download', '');
      link.querySelector('[data-release-label]').textContent = 'Download for Mac';
    });
    document.querySelectorAll('[data-release-detail]').forEach(node => { node.textContent = release.detail; });
  } catch {
    // The latest-download links work even if this optional metadata refresh fails.
  }
}
refreshRelease();
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') refreshRelease();
});
