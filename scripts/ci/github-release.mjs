import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

export const githubRepository = 'talhabalaj/whatsrusty';
export const githubDownloads = `https://github.com/${githubRepository}/releases`;

// Never log gh's captured output: authenticated API errors can contain URLs.
function gh(args) {
  try { return execFileSync('gh', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 1024 * 1024 }); }
  catch {
    throw new Error(`GitHub release command failed (${args.slice(0, 2).join(' ')}).`);
  }
}

export function githubReleaseStore({ run = gh } = {}) {
  let release, tag;
  // GitHub's by-tag endpoint omits drafts, even for an authenticated owner.
  const inspect = () => JSON.parse(run(['api', `repos/${githubRepository}/releases?per_page=100`, '--paginate',
    '--jq', `.[] | select(.tag_name == ${JSON.stringify(tag)})`])?.trim() || 'null');
  return {
    async begin(version, commitSha) {
      tag = `v${version}`;
      const remoteCommit = run(['api', `repos/${githubRepository}/commits/${tag}`, '--jq', '.sha'])?.trim();
      if (remoteCommit !== commitSha) throw new Error('Release tag does not point to the verified source commit.');
      release = inspect();
      if (!release) {
        release = JSON.parse(run(['api', `repos/${githubRepository}/releases`, '--method', 'POST',
          '-f', `tag_name=${tag}`, '-f', `target_commitish=${commitSha}`, '-F', 'draft=true',
          '-F', 'prerelease=false', '-f', 'make_latest=false', '-f', `name=WhatsRusty ${version}`,
          '-f', 'body=Developer ID signed and notarized macOS release.']));
      }
      if (!release || release.prerelease || release.target_commitish !== commitSha) throw new Error('Existing release has unexpected source or prerelease status.');
    },
    async read(name) {
      release = JSON.parse(run(['api', `repos/${githubRepository}/releases/${release.id}`]));
      if (!release.assets.some(asset => asset.name === name)) return new Response(null, { status: 404 });
      const directory = mkdtempSync(join(tmpdir(), 'whatsrusty-release-'));
      try {
        run(['release', 'download', tag, '--repo', githubRepository, '--pattern', name, '--dir', directory]);
        return new Response(readFileSync(join(directory, name)));
      } finally { rmSync(directory, { recursive: true, force: true }); }
    },
    async put(name, file) {
      if (!release.draft) throw new Error('Refusing to add assets to a published release.');
      if (basename(file) !== name) throw new Error('Release asset filename mismatch.');
      run(['release', 'upload', tag, file, '--repo', githubRepository]); // No --clobber.
    },
    async publish() { if (release.draft) run(['release', 'edit', tag, '--repo', githubRepository, '--draft=false', '--latest=false']); },
    async promote() { run(['release', 'edit', tag, '--repo', githubRepository, '--latest']); },
  };
}
