import { build } from 'esbuild';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import test from 'node:test';
import { deflateRawSync } from 'node:zlib';

const bundle = await build({
  entryPoints: ['src/ts/python-packages.ts'],
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm'
});
const P = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`);

test('finds third-party imports, not the stdlib or the workspace, plus requirements.txt', () => {
  const fs = {
    'main.py': 'import json, os\nimport pytest\nfrom yaml import safe_load\nfrom helpers.greet import greet\nimport numpy as np  # data\n# import ignored\nfrom . import sibling\n',
    'helpers': { 'greet.py': 'import attr\n' },
    'local_mod.py': 'x = 1\n',
    'other.py': 'import local_mod\n',
    'requirements.txt': 'requests>=2  # comment\n-r other.txt\n\nrich\n'
  };
  assert.deepEqual(P.requirementsFor(fs), ['PyYAML', 'attrs', 'numpy', 'pytest', 'requests>=2', 'rich']);
});

test('orders and matches versions like PEP 440', () => {
  const v = (s: string) => P.parseVersion(s);
  const order = ['1.0.dev1', '1.0a1', '1.0b2', '1.0rc1', '1.0', '1.0.post1', '1.1'];
  for (let i = 1; i < order.length; i++) assert.equal(P.compareVersions(v(order[i - 1]), v(order[i])), -1, `${order[i - 1]} < ${order[i]}`);
  assert.equal(P.compareVersions(v('1.0'), v('1.0.0')), 0);
  assert.ok(P.satisfies(v('1.6.2'), '>=1.6,<2'));
  assert.ok(!P.satisfies(v('2.0'), '>=1.6,<2'));
  assert.ok(P.satisfies(v('1.4.5'), '~=1.4.2') && !P.satisfies(v('1.5'), '~=1.4.2'));
  assert.ok(P.satisfies(v('3.15.2'), '==3.*') && !P.satisfies(v('3.15.2'), '!=3.15.*'));
  assert.ok(P.satisfies(v('3.15.2'), '') && !P.satisfies(v('1.0'), '!=1.0'));
});

test('evaluates environment markers for this runtime', () => {
  assert.ok(P.markerMatches('python_version >= "3.8"'));
  assert.ok(!P.markerMatches('python_version < "3.11"'));
  assert.ok(P.markerMatches('"3.8" <= python_version'));
  assert.ok(!P.markerMatches('sys_platform == "win32"'));
  assert.ok(P.markerMatches('sys_platform != "win32" and (os_name == "posix" or os_name == "nt")'));
  assert.ok(!P.markerMatches('extra == "testing"'));
  assert.ok(P.markerMatches('extra == "testing"', ['testing']));
  assert.ok(P.markerMatches('platform_python_implementation == "CPython" and python_full_version >= "3.10.1"'));
});

function zip(entries: Record<string, string | Buffer>, method = 8): Buffer {
  const locals: Buffer[] = [], central: Buffer[] = [];
  let offset = 0;
  for (const [name, content] of Object.entries(entries)) {
    const data = Buffer.from(content);
    const packed = method === 8 ? deflateRawSync(data) : data;
    const nameBytes = Buffer.from(name);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0); local.writeUInt16LE(method, 8);
    local.writeUInt32LE(packed.length, 18); local.writeUInt32LE(data.length, 22); local.writeUInt16LE(nameBytes.length, 26);
    const dir = Buffer.alloc(46);
    dir.writeUInt32LE(0x02014b50, 0); dir.writeUInt16LE(method, 10);
    dir.writeUInt32LE(packed.length, 20); dir.writeUInt32LE(data.length, 24); dir.writeUInt16LE(nameBytes.length, 28); dir.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, packed);
    central.push(dir, nameBytes);
    offset += 30 + nameBytes.length + packed.length;
  }
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0); end.writeUInt16LE(Object.keys(entries).length, 8); end.writeUInt16LE(Object.keys(entries).length, 10);
  end.writeUInt32LE(directory.length, 12); end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

test('unzips stored and deflated entries and refuses unsafe paths', async () => {
  for (const method of [0, 8]) {
    const files = await P.unzip(new Uint8Array(zip({ 'pkg/__init__.py': 'x = 1\n', 'pkg/data.bin': Buffer.from([0, 1, 2]) }, method)), 1 << 20);
    assert.equal(new TextDecoder().decode(files.get('pkg/__init__.py')), 'x = 1\n');
    assert.deepEqual([...files.get('pkg/data.bin')], [0, 1, 2]);
  }
  for (const evil of ['../evil.py', '/abs.py', 'a/../../b.py', 'win\\path.py'])
    await assert.rejects(P.unzip(new Uint8Array(zip({ [evil]: 'x' })), 1 << 20), /unsafe path/);
  await assert.rejects(P.unzip(new Uint8Array(zip({ 'big.py': 'x'.repeat(5000) })), 1000), /too large/);
});

/** A tiny PyPI: project JSON plus wheel files, served by a fake fetch. */
function fakePypi(projects: Record<string, { versions: Record<string, { files: Array<{ tag?: string; sdistOnly?: boolean; requiresPython?: string; body?: Buffer }>; requires?: string[] }> }>) {
  const blobs = new Map<string, Buffer>();
  const json = new Map<string, unknown>();
  const requests: string[] = [];
  for (const [name, project] of Object.entries(projects)) {
    const versions = Object.keys(project.versions);
    const latest = versions[versions.length - 1];
    const releases: Record<string, unknown[]> = {};
    for (const [version, release] of Object.entries(project.versions)) {
      releases[version] = release.files.map((file, i) => {
        const filename = file.sdistOnly ? `${name}-${version}.tar.gz` : `${name}-${version}-${file.tag ?? 'py3-none-any'}.whl`;
        const body = file.body ?? zip({ [`${name}/__init__.py`]: `VERSION = "${version}"\n`, [`${name}-${version}.dist-info/METADATA`]: `Name: ${name}\n` });
        const url = `https://files.pythonhosted.org/packages/${name}/${i}/${filename}`;
        blobs.set(url, body);
        return { filename, url, packagetype: file.sdistOnly ? 'sdist' : 'bdist_wheel', requires_python: file.requiresPython ?? null, digests: { sha256: createHash('sha256').update(body).digest('hex') }, size: body.length };
      });
      json.set(`https://pypi.org/pypi/${name}/${version}/json`, { info: { name, version, requires_dist: release.requires ?? [] }, releases: {} });
    }
    json.set(`https://pypi.org/pypi/${name}/json`, { info: { name, version: latest, requires_dist: project.versions[latest].requires ?? [] }, releases });
  }
  const fetch = async (url: string) => {
    requests.push(url);
    if (json.has(url)) return new Response(JSON.stringify(json.get(url)));
    if (blobs.has(url)) return new Response(blobs.get(url));
    return new Response('not found', { status: 404 });
  };
  return { fetch, blobs, requests };
}

test('resolves dependencies to pure-Python wheels and reports what it skips', async () => {
  const pypi = fakePypi({
    pytest: { versions: {
      '8.0.0': { files: [{}], requires: ['pluggy<2,>=1'] },
      '9.2.0': { files: [{}], requires: ['iniconfig>=2', 'pluggy<2,>=1.6', 'colorama>=0.4; sys_platform == "win32"', 'exceptiongroup>=1; python_version < "3.11"', 'tomli; extra == "toml"'] },
      '10.0.0rc1': { files: [{}] }
    } },
    pluggy: { versions: { '1.5.0': { files: [{}] }, '1.6.0': { files: [{}] }, '2.0.0': { files: [{}] } } },
    iniconfig: { versions: { '2.1.0': { files: [{}] }, '3.0.0': { files: [{ requiresPython: '>=3.16' }] } } },
    numpy: { versions: { '2.3.0': { files: [{ tag: 'cp315-cp315-manylinux_2_28_x86_64' }, { sdistOnly: true }] } } }
  });
  const installer = new P.PackageInstaller(pypi.fetch);
  const notes: string[] = [];
  const resolved = await installer.resolve(['pytest', 'numpy', 'no-such-package'], (s: { message: string }) => notes.push(s.message));
  assert.deepEqual(resolved.map((p: { name: string; version: string }) => `${p.name}==${p.version}`), ['pytest==9.2.0', 'iniconfig==2.1.0', 'pluggy==1.6.0']);
  assert.deepEqual(notes, ['numpy has compiled code, which can\'t run in the browser', 'no-such-package: not found on PyPI']);
  assert.ok(!pypi.requests.some((u) => /colorama|exceptiongroup|tomli/.test(u)), 'marker-excluded dependencies are never fetched');

  const files = await installer.install(resolved);
  assert.equal(new TextDecoder().decode(files['/usr/local/lib/python3.15/site-packages/pytest/__init__.py']), 'VERSION = "9.2.0"\n');
  assert.ok('/usr/local/lib/python3.15/site-packages/pluggy-1.6.0.dist-info/METADATA' in files);
});

test('honours extras, places .data/purelib, and rejects a wheel that fails its checksum', async () => {
  const pypi = fakePypi({
    tool: { versions: { '1.0.0': { files: [{ body: zip({ 'tool/__init__.py': '', 'tool-1.0.0.data/purelib/tool_extra.py': 'E = 1\n', 'tool-1.0.0.data/scripts/tool': '#!python\n' }) }], requires: ['helper; extra == "fast"'] } } },
    helper: { versions: { '0.1.0': { files: [{}] } } }
  });
  const installer = new P.PackageInstaller(pypi.fetch);
  const resolved = await installer.resolve(['tool[fast]']);
  assert.deepEqual(resolved.map((p: { name: string }) => p.name), ['tool', 'helper']);
  const files = await installer.install(resolved);
  assert.ok('/usr/local/lib/python3.15/site-packages/tool_extra.py' in files);
  assert.ok(!Object.keys(files).some((p) => p.includes('scripts')));

  const tampered = fakePypi({ bad: { versions: { '1.0.0': { files: [{}] } } } });
  const bad = new P.PackageInstaller(async (url: string, init?: RequestInit) => {
    const response = await tampered.fetch(url);
    return url.endsWith('.whl') ? new Response(zip({ 'bad/__init__.py': 'import os; os.system("x")\n' })) : response;
  });
  const notes: string[] = [];
  const out = await bad.install(await bad.resolve(['bad']), (s: { message: string }) => notes.push(s.message));
  assert.deepEqual(Object.keys(out), []);
  assert.match(notes.at(-1)!, /checksum/);
});
