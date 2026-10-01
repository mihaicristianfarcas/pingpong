// Where the page's facts come from: the repository on GitHub, and files in
// this checkout that change with each release.
import pingCask from '../../../Casks/ping.rb?raw';

export const REPO = 'https://github.com/mihaicristianfarcas/pingpong';

/** A file in the repository, on GitHub. */
export const onGitHub = (path: string) => `${REPO}/blob/main/${path}`;

/**
 * The release the Homebrew casks install. tools/update-casks rewrites the
 * cask at each release, so the page shows the version people get.
 */
export const VERSION = /^\s*version "([^"]+)"/m.exec(pingCask)?.[1] ?? null;

export const TAP = `brew tap mihaicristianfarcas/pingpong ${REPO}`;
