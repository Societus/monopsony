// Release-contract version comparison — TypeScript mirror of
// crates/monopsony-control/src/grpc.rs (`contract_version_precedence`,
// `ContractVersion` Ord, `PreReleaseId`). The fleet version column and skew
// badge must count releases exactly the way the server's ack funnel and
// self-apply guard compare them (semver §11 subset), so keep this file in
// sync with the Rust source.
//
// Accepted subset: `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]` — build metadata
// is ignored for precedence; the core is exactly three numeric parts; a
// prerelease is dot-separated identifiers, each either all-digits (numeric)
// or non-empty alphanumeric; a leading `v` is stripped.

type PreReleaseId = { num: number } | { alph: string };

interface ParsedVersion {
  major: number;
  minor: number;
  patch: number;
  /** null = plain release; sorts above every prerelease of the same core. */
  prerelease: PreReleaseId[] | null;
}

const NUMERIC_RE = /^[0-9]+$/;

function parsePreReleaseId(id: string): PreReleaseId | null {
  if (id.length === 0) return null;
  if (NUMERIC_RE.test(id)) return { num: parseInt(id, 10) };
  return { alph: id };
}

export function parseReleaseVersion(version: string): ParsedVersion | null {
  const clean = version.trim().replace(/^v/, '').split('+')[0];
  const dash = clean.indexOf('-');
  const coreStr = dash === -1 ? clean : clean.slice(0, dash);
  const preStr = dash === -1 ? null : clean.slice(dash + 1);
  const coreParts = coreStr.split('.');
  if (coreParts.length !== 3) return null;
  const core: number[] = [];
  for (const part of coreParts) {
    if (!NUMERIC_RE.test(part)) return null;
    core.push(parseInt(part, 10));
  }
  let prerelease: PreReleaseId[] | null = null;
  if (preStr !== null) {
    prerelease = [];
    for (const id of preStr.split('.')) {
      const parsed = parsePreReleaseId(id);
      if (parsed === null) return null;
      prerelease.push(parsed);
    }
    if (prerelease.length === 0) return null;
  }
  return { major: core[0], minor: core[1], patch: core[2], prerelease };
}

function cmpPreReleaseId(a: PreReleaseId, b: PreReleaseId): number {
  if ('num' in a && 'num' in b) return a.num - b.num;
  // Numeric identifiers always sort below alphanumeric ones (semver §11).
  if ('num' in a) return -1;
  if ('num' in b) return 1;
  return a.alph < b.alph ? -1 : a.alph > b.alph ? 1 : 0;
}

function cmpPreRelease(a: PreReleaseId[], b: PreReleaseId[]): number {
  const len = Math.min(a.length, b.length);
  for (let i = 0; i < len; i++) {
    const ord = cmpPreReleaseId(a[i], b[i]);
    if (ord !== 0) return ord;
  }
  // Identifier-wise equal: the shorter prefix sorts below (semver §11).
  return a.length - b.length;
}

/** semver §11 precedence compare; null when either side is outside the subset. */
export function compareReleaseVersions(a: string, b: string): number | null {
  const pa = parseReleaseVersion(a);
  const pb = parseReleaseVersion(b);
  if (pa === null || pb === null) return null;
  if (pa.major !== pb.major) return pa.major - pb.major;
  if (pa.minor !== pb.minor) return pa.minor - pb.minor;
  if (pa.patch !== pb.patch) return pa.patch - pb.patch;
  const paPre = pa.prerelease;
  const pbPre = pb.prerelease;
  if (paPre === null || pbPre === null) {
    // At least one plain release: a release sorts above any prerelease of
    // the same core; equal null/null was already handled above.
    if (paPre === pbPre) return 0;
    return paPre === null ? 1 : -1;
  }
  return cmpPreRelease(paPre, pbPre);
}

/** Sort releases oldest-first, dropping versions outside the subset. */
export function sortReleasesAscending<T extends { version: string }>(releases: T[]): T[] {
  return releases
    .filter((r) => parseReleaseVersion(r.version) !== null)
    .sort((a, b) => compareReleaseVersions(a.version, b.version) ?? 0);
}

/**
 * Legacy report: the client never reported a monopsony version (empty) or a
 * pre-P2 client reported the literal `0`. Rendered as `legacy`, never `0`,
 * and never skew-badged (unknown ≠ lagging — mirrors the backend guard).
 */
export function isLegacyVersion(version: string): boolean {
  return version === '' || version === '0';
}

/**
 * How many stored releases rank strictly above the client's reported
 * version — i.e. how far behind the latest release it is. Null when it
 * cannot be determined (legacy report, unparseable report, or no parseable
 * stored releases). Skew badge threshold is 2 (more than one behind).
 */
export function releasesBehind(clientVersion: string, storedVersions: string[]): number | null {
  if (isLegacyVersion(clientVersion)) return null;
  if (parseReleaseVersion(clientVersion) === null) return null;
  let behind = 0;
  for (const release of sortReleasesAscending(storedVersions.map((version) => ({ version })))) {
    const cmp = compareReleaseVersions(release.version, clientVersion);
    if (cmp !== null && cmp > 0) behind++;
  }
  return behind;
}
