/**
 * Installs pure-Python packages from PyPI into a Python run, the way `pip install` would,
 * without pip and without network access inside the sandbox.
 *
 * Before a run, the engine looks at what the student's files import (and any
 * requirements.txt), resolves those names and their dependencies on PyPI, downloads each
 * package's pure-Python wheel (checked against PyPI's SHA-256), unpacks it, and mounts the
 * files into the interpreter's site-packages. Packages with compiled code can't run in the
 * browser; they are reported, and the program still runs.
 */
import stdlibModules from './python-stdlib-modules.json' with { type: 'json' };

export const PYTHON_VERSION = '3.15.2';
export const SITE_PACKAGES = '/usr/local/lib/python3.15/site-packages';

const PYPI_JSON = 'https://pypi.org/pypi';
const WHEEL_HOST = 'files.pythonhosted.org';
const LIMITS = {
  packages: 50,
  downloadBytes: 60 * 1024 * 1024,
  files: 5000,
  unpackedBytes: 200 * 1024 * 1024
};

/** Environment markers for this runtime (PEP 508). */
const ENVIRONMENT: Record<string, string> = {
  python_version: PYTHON_VERSION.split('.').slice(0, 2).join('.'),
  python_full_version: PYTHON_VERSION,
  implementation_name: 'cpython',
  implementation_version: PYTHON_VERSION,
  platform_python_implementation: 'CPython',
  os_name: 'posix',
  sys_platform: 'wasi',
  platform_system: '',
  platform_machine: 'wasm32',
  platform_release: '0.0.0',
  platform_version: '0.0.0'
};

/** Import names whose PyPI project is named differently. */
const PROJECT_FOR_IMPORT: Record<string, string> = {
  yaml: 'PyYAML',
  bs4: 'beautifulsoup4',
  dateutil: 'python-dateutil',
  attr: 'attrs',
  dotenv: 'python-dotenv',
  jwt: 'PyJWT',
  serial: 'pyserial',
  docx: 'python-docx',
  pptx: 'python-pptx',
  slugify: 'python-slugify',
  PIL: 'pillow',
  sklearn: 'scikit-learn',
  cv2: 'opencv-python',
  Crypto: 'pycryptodome',
  skimage: 'scikit-image',
  google: 'protobuf',
  _pytest: 'pytest'
};

const STDLIB = new Set<string>(stdlibModules as string[]);
const NAME = /^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$/;

export type PackageStatus = { kind: 'installing' | 'installed' | 'skipped'; message: string };
export type Fetch = (url: string, init?: RequestInit) => Promise<Response>;

/** PEP 503 normalized project name. */
export function normalize(name: string): string {
  return name.toLowerCase().replace(/[-_.]+/g, '-');
}

// ── what a program needs ─────────────────────────────────────────────────────

type DirNode = { [name: string]: string | DirNode };

/** Top-level import names in Python source (best effort, line based). */
export function importsIn(source: string): Set<string> {
  const names = new Set<string>();
  for (const raw of source.split('\n')) {
    const line = raw.replace(/#.*/, '').trim();
    let match = /^import\s+(.+)$/.exec(line);
    if (match) {
      for (const part of match[1].split(',')) {
        const name = part.trim().split(/\s+/)[0]?.split('.')[0];
        if (name) names.add(name);
      }
      continue;
    }
    match = /^from\s+([A-Za-z_][\w.]*)\s+import\b/.exec(line);
    if (match) names.add(match[1].split('.')[0]);
  }
  return names;
}

/** Project names a workspace needs from PyPI: third-party imports plus requirements.txt. */
export function requirementsFor(fs: DirNode): string[] {
  const local = new Set<string>();
  const sources: string[] = [];
  let requirementsTxt = '';
  const walk = (node: DirNode, path: string) => {
    for (const [name, child] of Object.entries(node)) {
      if (typeof child === 'string') {
        if (name.endsWith('.py')) {
          sources.push(child);
          if (!path) local.add(name.slice(0, -3));
        }
        if (!path && name === 'requirements.txt') requirementsTxt = child;
      } else {
        if (!path) local.add(name);
        walk(child, `${path}/${name}`);
      }
    }
  };
  walk(fs, '');
  const wanted = new Map<string, string>();
  for (const source of sources)
    for (const name of importsIn(source)) {
      if (
        STDLIB.has(name) ||
        local.has(name) ||
        (name.startsWith('_') && !(name in PROJECT_FOR_IMPORT))
      )
        continue;
      const project = PROJECT_FOR_IMPORT[name] ?? name;
      wanted.set(normalize(project), project);
    }
  for (const raw of requirementsTxt.split('\n')) {
    const line = raw.replace(/#.*/, '').trim();
    if (!line || line.startsWith('-')) continue;
    const name = /^[A-Za-z0-9][A-Za-z0-9._-]*/.exec(line)?.[0];
    if (name) wanted.set(normalize(name), line);
  }
  return [...wanted.values()].sort();
}

// ── PEP 440 versions and specifiers (the common subset) ──────────────────────

type Version = {
  release: number[];
  pre?: [number, number];
  post?: number;
  dev?: number;
  text: string;
};

export function parseVersion(text: string): Version | null {
  const match =
    /^v?(?:\d+!)?(\d+(?:\.\d+)*)(?:[-_.]?(a|b|c|rc|alpha|beta|pre|preview)[-_.]?(\d*))?(?:-(\d+)|[-_.]?(?:post|rev|r)[-_.]?(\d*))?(?:[-_.]?dev[-_.]?(\d*))?(?:\+[a-z0-9.]+)?$/i.exec(
      text.trim()
    );
  if (!match) return null;
  const preKind = match[2]?.toLowerCase();
  const preRank = preKind
    ? (
        { a: 0, alpha: 0, b: 1, beta: 1, c: 2, rc: 2, pre: 2, preview: 2 } as Record<string, number>
      )[preKind]
    : undefined;
  return {
    release: match[1].split('.').map(Number),
    pre: preRank === undefined ? undefined : [preRank, Number(match[3] || 0)],
    post:
      match[4] !== undefined
        ? Number(match[4])
        : match[5] !== undefined
          ? Number(match[5] || 0)
          : undefined,
    dev: match[6] !== undefined ? Number(match[6] || 0) : undefined,
    text
  };
}

export function compareVersions(a: Version, b: Version): number {
  const length = Math.max(a.release.length, b.release.length);
  for (let i = 0; i < length; i++) {
    const d = (a.release[i] ?? 0) - (b.release[i] ?? 0);
    if (d) return Math.sign(d);
  }
  // dev < pre < (final) < post, per PEP 440.
  const key = (v: Version): number[] => [
    v.pre ? 0 : v.dev !== undefined && v.post === undefined ? -1 : 1,
    v.pre?.[0] ?? 0,
    v.pre?.[1] ?? 0,
    v.post ?? -1,
    v.dev ?? Number.POSITIVE_INFINITY
  ];
  const ka = key(a),
    kb = key(b);
  for (let i = 0; i < ka.length; i++) if (ka[i] !== kb[i]) return Math.sign(ka[i] - kb[i]);
  return 0;
}

const isPre = (v: Version) => v.pre !== undefined || v.dev !== undefined;

/** Whether a version satisfies a specifier set like ">=1.6,<2" (empty = anything). */
export function satisfies(version: Version, specifiers: string): boolean {
  for (const raw of specifiers.split(',')) {
    const spec = raw.trim();
    if (!spec) continue;
    const match = /^(~=|===|==|!=|<=|>=|<|>)\s*(.+)$/.exec(spec);
    if (!match) return false;
    const [, op, target] = match;
    if ((op === '==' || op === '!=') && target.endsWith('.*')) {
      const prefix = target.slice(0, -2).split('.').map(Number);
      const same = prefix.every((n, i) => (version.release[i] ?? 0) === n);
      if (same !== (op === '==')) return false;
      continue;
    }
    const other = parseVersion(target);
    if (!other) return false;
    const c = compareVersions(version, other);
    if (op === '~=') {
      const upper = other.release.slice(0, Math.max(1, other.release.length - 1));
      if (c < 0 || !upper.every((n, i) => (version.release[i] ?? 0) === n)) return false;
    } else if (
      !(op === '==' || op === '==='
        ? c === 0
        : op === '!='
          ? c !== 0
          : op === '<='
            ? c <= 0
            : op === '>='
              ? c >= 0
              : op === '<'
                ? c < 0
                : c > 0)
    )
      return false;
  }
  return true;
}

// ── PEP 508 requirements and markers ─────────────────────────────────────────

export type Requirement = { name: string; extras: string[]; specifiers: string; marker?: string };

export function parseRequirement(text: string): Requirement | null {
  const [spec, marker] = text.split(';', 2).map((s) => s.trim());
  const match = /^([A-Za-z0-9][A-Za-z0-9._-]*)\s*(?:\[([^\]]*)\])?\s*\(?\s*([^)]*?)\s*\)?$/.exec(
    spec
  );
  if (!match || match[3].includes('@')) return null; // direct URL references aren't followed
  return {
    name: match[1],
    extras: (match[2] ?? '')
      .split(',')
      .map((e) => e.trim())
      .filter(Boolean),
    specifiers: match[3].replace(/\s+/g, ''),
    marker: marker || undefined
  };
}

/** Evaluates a PEP 508 marker for this runtime, with the given extras requested. */
export function markerMatches(marker: string, extras: string[] = []): boolean {
  const tokens =
    marker.match(
      /\(|\)|'[^']*'|"[^"]*"|===|==|!=|<=|>=|~=|<|>|\bnot\s+in\b|\bin\b|\band\b|\bor\b|[A-Za-z_][A-Za-z0-9_.]*/g
    ) ?? [];
  let i = 0;
  const value = (token: string): { text: string; variable?: string } => {
    if (/^['"]/.test(token)) return { text: token.slice(1, -1) };
    if (token === 'extra') return { text: '', variable: 'extra' };
    return { text: ENVIRONMENT[token] ?? '', variable: token };
  };
  const compare = (
    left: { text: string; variable?: string },
    op: string,
    right: { text: string; variable?: string }
  ): boolean => {
    if (left.variable === 'extra' || right.variable === 'extra') {
      const wanted = normalize((left.variable === 'extra' ? right : left).text);
      const has = extras.some((e) => normalize(e) === wanted);
      return op === '!=' ? !has : has;
    }
    if (op === 'in') return right.text.includes(left.text);
    if (/^not\s+in$/.test(op)) return !right.text.includes(left.text);
    // Literal on the left ("3.8" <= python_version): put the variable first.
    if (!left.variable && right.variable) {
      const flipped: Record<string, string> = { '<': '>', '>': '<', '<=': '>=', '>=': '<=' };
      return compare(right, flipped[op] ?? op, left);
    }
    const a = parseVersion(left.text),
      b = parseVersion(right.text);
    if (a && b && /version/.test(left.variable ?? '')) return satisfies(a, `${op}${right.text}`);
    return op === '==' || op === '==='
      ? left.text === right.text
      : op === '!='
        ? left.text !== right.text
        : false;
  };
  const atom = (): boolean => {
    if (tokens[i] === '(') {
      i++;
      const result = or();
      i++; // ')'
      return result;
    }
    const left = value(tokens[i++]);
    const op = tokens[i++];
    const right = value(tokens[i++]);
    return compare(left, op, right);
  };
  const and = (): boolean => {
    let result = atom();
    while (tokens[i] === 'and') {
      i++;
      result = atom() && result;
    }
    return result;
  };
  const or = (): boolean => {
    let result = and();
    while (tokens[i] === 'or') {
      i++;
      result = and() || result;
    }
    return result;
  };
  try {
    return or();
  } catch {
    return false;
  }
}

// ── PyPI ─────────────────────────────────────────────────────────────────────

type PypiFile = {
  filename: string;
  url: string;
  packagetype: string;
  requires_python?: string | null;
  digests: { sha256: string };
  size: number;
  yanked?: boolean;
};
type PypiProject = {
  info: { name: string; version: string; requires_dist?: string[] | null };
  releases: Record<string, PypiFile[]>;
};

export type ResolvedPackage = {
  name: string;
  version: string;
  url: string;
  sha256: string;
  size: number;
};

/** A pure-Python wheel (no compiled code), usable on any platform. */
function pureWheel(file: PypiFile): boolean {
  if (file.packagetype !== 'bdist_wheel' || file.yanked) return false;
  const tags = file.filename
    .replace(/\.whl$/, '')
    .split('-')
    .slice(-3);
  return (
    tags.length === 3 &&
    tags[1] === 'none' &&
    tags[2] === 'any' &&
    tags[0].split('.').some((t) => t === 'py3' || t.startsWith('py3'))
  );
}

function pythonOk(requiresPython?: string | null): boolean {
  if (!requiresPython) return true;
  return satisfies(parseVersion(PYTHON_VERSION)!, requiresPython.replace(/\s+/g, ''));
}

export class PackageInstaller {
  private readonly projects = new Map<string, Promise<PypiProject | null>>();
  private readonly metadata = new Map<string, Promise<string[]>>();
  private readonly wheels = new Map<string, Promise<Map<string, Uint8Array>>>();

  constructor(private readonly fetchImpl: Fetch = (url, init) => fetch(url, init)) {}

  private project(name: string): Promise<PypiProject | null> {
    const key = normalize(name);
    let cached = this.projects.get(key);
    if (!cached) {
      cached = this.fetchImpl(`${PYPI_JSON}/${encodeURIComponent(key)}/json`).then(
        async (response) => (response.ok ? ((await response.json()) as PypiProject) : null),
        () => null
      );
      this.projects.set(key, cached);
    }
    return cached;
  }

  /** requires_dist of one release (the project JSON only carries the latest's). */
  private requiresDist(name: string, version: string, latest: PypiProject): Promise<string[]> {
    if (latest.info.version === version) return Promise.resolve(latest.info.requires_dist ?? []);
    const key = `${normalize(name)}@${version}`;
    let cached = this.metadata.get(key);
    if (!cached) {
      cached = this.fetchImpl(
        `${PYPI_JSON}/${encodeURIComponent(normalize(name))}/${encodeURIComponent(version)}/json`
      ).then(
        async (response) =>
          response.ok ? (((await response.json()) as PypiProject).info.requires_dist ?? []) : [],
        () => []
      );
      this.metadata.set(key, cached);
    }
    return cached;
  }

  /**
   * Resolves requirements and their dependencies to pure-Python wheels. Greedy: the newest
   * release that fits every constraint seen so far for that project.
   */
  async resolve(
    requirements: string[],
    report: (status: PackageStatus) => void = () => {}
  ): Promise<ResolvedPackage[]> {
    const chosen = new Map<string, ResolvedPackage>();
    const queue: Array<{ requirement: Requirement; top: boolean }> = [];
    for (const text of requirements) {
      const requirement = parseRequirement(text);
      if (requirement && NAME.test(requirement.name)) queue.push({ requirement, top: true });
    }
    while (queue.length) {
      const { requirement, top } = queue.shift()!;
      const key = normalize(requirement.name);
      if (chosen.has(key)) continue;
      if (chosen.size >= LIMITS.packages) break;
      const project = await this.project(requirement.name);
      if (!project) {
        if (top) report({ kind: 'skipped', message: `${requirement.name}: not found on PyPI` });
        continue;
      }
      const candidates = Object.entries(project.releases)
        .map(([text, files]) => ({ version: parseVersion(text), files }))
        .filter(
          (c): c is { version: Version; files: PypiFile[] } =>
            c.version !== null && satisfies(c.version, requirement.specifiers)
        )
        .sort((a, b) => compareVersions(b.version, a.version));
      const stable = candidates.filter((c) => !isPre(c.version));
      const pick = [...stable, ...candidates.filter((c) => isPre(c.version))]
        .map((c) => ({
          version: c.version,
          wheel: c.files.find((f) => pureWheel(f) && pythonOk(f.requires_python))
        }))
        .find((c) => c.wheel);
      if (!pick?.wheel) {
        const compiled = candidates.some((c) =>
          c.files.some((f) => f.packagetype === 'bdist_wheel')
        );
        report({
          kind: 'skipped',
          message: compiled
            ? `${project.info.name} has compiled code, which can't run in the browser`
            : `${project.info.name}: no Python 3.15 wheel on PyPI`
        });
        continue;
      }
      const url = new URL(pick.wheel.url);
      if (url.protocol !== 'https:' || url.hostname !== WHEEL_HOST) continue;
      chosen.set(key, {
        name: project.info.name,
        version: pick.version.text,
        url: pick.wheel.url,
        sha256: pick.wheel.digests.sha256,
        size: pick.wheel.size
      });
      for (const dependency of await this.requiresDist(
        requirement.name,
        pick.version.text,
        project
      )) {
        const parsed = parseRequirement(dependency);
        if (!parsed || !NAME.test(parsed.name)) continue;
        if (parsed.marker && !markerMatches(parsed.marker, requirement.extras)) continue;
        queue.push({ requirement: parsed, top: false });
      }
    }
    return [...chosen.values()];
  }

  /** Files to mount, by absolute path in the run, for these resolved packages. */
  async install(
    packages: ResolvedPackage[],
    report: (status: PackageStatus) => void = () => {}
  ): Promise<Record<string, Uint8Array>> {
    const files: Record<string, Uint8Array> = {};
    let downloaded = 0,
      count = 0;
    for (const pkg of packages) {
      downloaded += pkg.size;
      if (downloaded > LIMITS.downloadBytes) {
        report({
          kind: 'skipped',
          message: `${pkg.name}: over the ${LIMITS.downloadBytes >> 20}MB package limit`
        });
        break;
      }
      const key = `${pkg.url}#${pkg.sha256}`;
      let wheel = this.wheels.get(key);
      if (!wheel) {
        report({ kind: 'installing', message: `Installing ${pkg.name} ${pkg.version}` });
        wheel = this.download(pkg);
        this.wheels.set(key, wheel);
        wheel.catch(() => this.wheels.delete(key));
      }
      let contents: Map<string, Uint8Array>;
      try {
        contents = await wheel;
      } catch (error) {
        report({
          kind: 'skipped',
          message: `${pkg.name}: ${error instanceof Error ? error.message : String(error)}`
        });
        continue;
      }
      for (const [path, bytes] of contents) {
        if (++count > LIMITS.files) break;
        files[`${SITE_PACKAGES}/${path}`] = bytes;
      }
    }
    return files;
  }

  private async download(pkg: ResolvedPackage): Promise<Map<string, Uint8Array>> {
    const response = await this.fetchImpl(pkg.url, { cache: 'force-cache' });
    if (!response.ok) throw new Error(`download failed (${response.status})`);
    const bytes = new Uint8Array(await response.arrayBuffer());
    const digest = [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))]
      .map((b) => b.toString(16).padStart(2, '0'))
      .join('');
    if (digest !== pkg.sha256) throw new Error("download did not match PyPI's checksum");
    return wheelFiles(await unzip(bytes, LIMITS.unpackedBytes));
  }
}

/** Where each wheel entry belongs under site-packages (PEP 427); scripts and headers are dropped. */
function wheelFiles(entries: Map<string, Uint8Array>): Map<string, Uint8Array> {
  const files = new Map<string, Uint8Array>();
  for (const [path, bytes] of entries) {
    const data = /^[^/]+\.data\/(purelib|platlib)\/(.+)$/.exec(path);
    if (data) files.set(data[2], bytes);
    else if (!/^[^/]+\.data\//.test(path)) files.set(path, bytes);
  }
  return files;
}

// ── zip (wheels) ─────────────────────────────────────────────────────────────

function safePath(path: string): boolean {
  return (
    path.length > 0 &&
    !path.startsWith('/') &&
    !path.includes('\\') &&
    !path.includes('\0') &&
    !path.split('/').some((part) => part === '..' || part === '.')
  );
}

/** Files in a zip archive (stored or deflated), with traversal and size guards. */
export async function unzip(
  bytes: Uint8Array,
  maxUnpacked: number
): Promise<Map<string, Uint8Array>> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let end = -1;
  for (let i = bytes.length - 22; i >= Math.max(0, bytes.length - 22 - 65535); i--)
    if (view.getUint32(i, true) === 0x06054b50) {
      end = i;
      break;
    }
  if (end < 0) throw new Error('not a zip archive');
  const entries = view.getUint16(end + 10, true);
  let offset = view.getUint32(end + 16, true);
  const files = new Map<string, Uint8Array>();
  let total = 0;
  const decoder = new TextDecoder();
  for (let n = 0; n < entries; n++) {
    if (view.getUint32(offset, true) !== 0x02014b50) throw new Error('corrupt zip directory');
    const method = view.getUint16(offset + 10, true);
    const compressed = view.getUint32(offset + 20, true);
    const size = view.getUint32(offset + 24, true);
    const nameLength = view.getUint16(offset + 28, true);
    const extraLength = view.getUint16(offset + 30, true);
    const commentLength = view.getUint16(offset + 32, true);
    const local = view.getUint32(offset + 42, true);
    const name = decoder.decode(bytes.subarray(offset + 46, offset + 46 + nameLength));
    offset += 46 + nameLength + extraLength + commentLength;
    if (name.endsWith('/')) continue;
    if (!safePath(name)) throw new Error(`unsafe path in package: ${name}`);
    total += size;
    if (total > maxUnpacked) throw new Error('package too large');
    const dataStart =
      local + 30 + view.getUint16(local + 26, true) + view.getUint16(local + 28, true);
    const data = bytes.subarray(dataStart, dataStart + compressed);
    let out: Uint8Array;
    if (method === 0) out = data.slice();
    else if (method === 8)
      out = new Uint8Array(
        await new Response(
          new Blob([data.slice()]).stream().pipeThrough(new DecompressionStream('deflate-raw'))
        ).arrayBuffer()
      );
    else throw new Error(`unsupported compression in package: ${name}`);
    if (out.length !== size) throw new Error(`corrupt entry in package: ${name}`);
    files.set(name, out);
  }
  return files;
}

// ── shared installs ──────────────────────────────────────────────────────────

const shared = {
  installer: undefined as PackageInstaller | undefined,
  sets: new Map<string, Promise<{ files: Record<string, Uint8Array>; notes: PackageStatus[] }>>()
};

/**
 * The files a workspace's PyPI packages add under site-packages, installing them if needed.
 * One installer serves the whole page, so a Run and IntelliSense asking for the same
 * packages share lookups and downloads. Notes about packages that can't be installed are
 * repeated to every caller; install progress is reported only while downloading.
 */
export async function pythonPackageFiles(
  fs: DirNode,
  report: (status: PackageStatus) => void = () => {}
): Promise<Record<string, Uint8Array>> {
  const requirements = requirementsFor(fs);
  if (!requirements.length) return {};
  const key = requirements.join('\n');
  let set = shared.sets.get(key);
  if (!set) {
    shared.installer ??= new PackageInstaller();
    const installer = shared.installer;
    const notes: PackageStatus[] = [];
    const collect = (status: PackageStatus) => {
      if (status.kind === 'skipped') notes.push(status);
      else report(status);
    };
    set = installer
      .resolve(requirements, collect)
      .then(async (resolved) => ({ files: await installer.install(resolved, collect), notes }));
    shared.sets.set(key, set);
    set.catch(() => shared.sets.delete(key));
  }
  const { files, notes } = await set;
  for (const note of notes) report(note);
  return files;
}
