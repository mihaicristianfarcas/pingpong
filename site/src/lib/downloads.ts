// The downloads: the archives the release workflow publishes for each push
// to main (tools/publish-downloads), on Cloudflare R2. Their names stay the
// same from one release to the next, so the page links them as they are;
// latest.json says which build they are, and the page reads it as it loads.

export const DOWNLOADS = 'https://downloads.ping-pong.sh';

/** What the archives are: version, commit, and each file's size and SHA-256. */
export const MANIFEST = `${DOWNLOADS}/latest.json`;

export const CHECKSUMS = `${DOWNLOADS}/latest/SHA256SUMS`;

export type App = 'Ping' | 'Pong';
export type System = 'macos' | 'windows' | 'linux';

/**
 * One archive: `id` is its key in latest.json's files, `url` where it
 * stays.
 */
export const archive = (app: App, system: System, arch: string) => {
  const id = `${app}-${system}-${arch}`;
  const ext = system === 'linux' ? '.tar.gz' : '.zip';
  return { id, url: `${DOWNLOADS}/latest/${id}${ext}` };
};
