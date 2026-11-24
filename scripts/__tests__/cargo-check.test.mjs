// Plan-level tests for scripts/cargo-check.sh. A fake cargo binary records the
// invocations and can be made to fail, so these tests cover discovery-free
// execution, cache reuse, failure reporting and usage errors without
// compiling the repository.
//
//   node --test scripts/__tests__/cargo-check.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';

const here = import.meta.dirname;
const repoRoot = resolve(here, '..', '..');
const script = join(repoRoot, 'scripts', 'cargo-check.sh');

function tempDir(prefix) {
  return mkdtempSync(join(tmpdir(), prefix));
}

function fakeCargoBin() {
  const bin = tempDir('cargo-check-bin-');
  writeFileSync(
    join(bin, 'cargo'),
    `#!/usr/bin/env bash
set -u
printf '%s\\n' "$*" >> "$FAKE_CARGO_LOG"
case "\${1:-}" in
  --version) echo 'cargo 1.100.0 (fake)' ;;
  metadata) exit 0 ;;
  check)
    if [[ "\${FAKE_CARGO_FAIL:-0}" == "1" ]]; then
      exit "\${FAKE_CARGO_CODE:-1}"
    fi
    mkdir -p "\${CARGO_TARGET_DIR:-/nonexistent}/debug/.fingerprint"
    ;;
esac
exit 0
`,
  );
  chmodSync(join(bin, 'cargo'), 0o755);
  return bin;
}

function fixtureWorkspace() {
  const root = tempDir('cargo-check-ws-');
  mkdirSync(join(root, 'src'));
  writeFileSync(
    join(root, 'Cargo.toml'),
    '[package]\nname = "fixture"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n',
  );
  writeFileSync(join(root, 'src', 'lib.rs'), 'pub fn answer() -> u8 { 42 }\n');
  return root;
}

function runScript(options = {}) {
  const log = join(tempDir('cargo-check-log-'), 'cargo.log');
  writeFileSync(log, '');
  const env = {
    ...process.env,
    PATH: `${options.bin ?? fakeCargoBin()}${delimiter}${process.env.PATH ?? ''}`,
    FAKE_CARGO_LOG: log,
    CARGO_CHECK_CACHE_DIR: options.cacheDir ?? tempDir('cargo-check-cache-'),
  };
  if (options.cargo) env.CARGO = options.cargo;
  if (options.fail) {
    env.FAKE_CARGO_FAIL = '1';
    env.FAKE_CARGO_CODE = String(options.code ?? 1);
  }
  return {
    ...spawnSync('bash', [script, ...(options.args ?? [])], {
      encoding: 'utf8',
      env,
    }),
    log: readFileSync(log, 'utf8'),
    cacheDir: env.CARGO_CHECK_CACHE_DIR,
  };
}

function summary(cacheDir) {
  return JSON.parse(readFileSync(join(cacheDir, 'last-run.json'), 'utf8'));
}

test('cargo-check.sh parses and documents its options', () => {
  assert.equal(spawnSync('bash', ['-n', script]).status, 0);
  const result = runScript({ args: ['--help'] });
  assert.equal(result.status, 0);
  for (const flag of ['--workspace', '--no-cache', '--fail-fast', '--verbose']) {
    assert.ok(result.stdout.includes(flag));
  }
});

test('checks a requested workspace with all targets and writes a summary', () => {
  const workspace = fixtureWorkspace();
  const result = runScript({
    args: ['--workspace', workspace, '--verbose'],
  });

  assert.equal(result.status, 0, result.stderr);
  const workspaceName = workspace.split(/[\\/]/).pop();
  assert.match(
    result.log,
    new RegExp(
      `check --workspace --all-targets --manifest-path .*/${workspaceName}/Cargo\\.toml`,
    ),
  );
  assert.match(result.stderr, /event=check_complete workspace=.*status=success cache=miss duration_ms=\d+/);
  assert.match(result.stderr, /event=summary workspaces=1 failed=0/);
  const report = summary(result.cacheDir);
  assert.equal(report.failed, 0);
  assert.equal(report.workspaces.length, 1);
  assert.equal(report.workspaces[0].status, 'success');
});

test('reuses the fingerprinted target directory on a second run', () => {
  const workspace = fixtureWorkspace();
  const cacheDir = tempDir('cargo-check-cache-');
  const bin = fakeCargoBin();

  const first = runScript({ args: ['--workspace', workspace], cacheDir, bin });
  assert.equal(first.status, 0, first.stderr);
  assert.match(first.stderr, /status=success cache=miss/);

  const second = runScript({ args: ['--workspace', workspace], cacheDir, bin });
  assert.equal(second.status, 0, second.stderr);
  assert.match(second.stderr, /status=success cache=hit/);
  const fingerprints = readdirSync(cacheDir).filter((name) => /^\d+$/.test(name));
  assert.equal(fingerprints.length, 1);
  assert.equal(summary(cacheDir).workspaces[0].cache, 'hit');
});

test('--no-cache uses a disposable target directory', () => {
  const workspace = fixtureWorkspace();
  const result = runScript({
    args: ['--workspace', workspace, '--no-cache'],
  });

  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /event=check_start workspace=.*cache=disabled target=.*no-cache\./);
  assert.ok(
    !readdirSync(result.cacheDir).some((name) => name.startsWith('no-cache.')),
    'temporary target directory was not cleaned up',
  );
});

test('cargo failures are reported and produce a non-zero exit code', () => {
  const workspace = fixtureWorkspace();
  const result = runScript({
    args: ['--workspace', workspace],
    fail: true,
    code: 7,
  });

  assert.equal(result.status, 1);
  assert.match(result.stderr, /event=check_complete workspace=.*status=failure code=7/);
  assert.match(result.stderr, /event=summary workspaces=1 failed=1/);
  assert.equal(summary(result.cacheDir).failed, 1);
});

test('usage errors and a missing cargo binary exit with code 2', () => {
  const unknown = runScript({ args: ['--not-a-flag'] });
  assert.equal(unknown.status, 2);
  assert.match(unknown.stderr, /error=invalid_usage detail=unknown_argument/);

  const missingWorkspace = runScript({ args: ['--workspace', '/does/not/exist'] });
  assert.equal(missingWorkspace.status, 2);
  assert.match(missingWorkspace.stderr, /error=workspace_not_found/);

  const missingCargo = runScript({ args: [], cargo: 'definitely-not-a-cargo-binary' });
  assert.equal(missingCargo.status, 2);
  assert.match(missingCargo.stderr, /error=cargo_not_found/);
});
