import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

// Build the fork first: cargo build; bun test scripts/upgrade-ui.test.ts
// Never exercise a host PATH binary or an older upstream workspace build.
const root = resolve(import.meta.dir, "..");
const cli = join(root, "target/debug/crabcode");
const current = /^version = "([^"]+)"/m.exec(
  readFileSync(join(root, "Cargo.toml"), "utf8"),
)![1];
const target = "gondescode-0123456789abcdef";
const oldBinary = `#!/bin/sh\nprintf 'crabcode ${current}\\n'\n`;
// Preview identity is the tag, not the package semver. An equal-semver payload
// must still replace the installed executable.
const newBinary = `#!/bin/sh\n# downloaded preview\nprintf 'crabcode ${current}\\n'\n`;
const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});
const quote = (s: string) => `'${s.replaceAll("'", "'\\''")}'`;
const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");

function fixture(
  platform: "macos-arm64" | "linux-x64" = "linux-x64",
  custom = true,
) {
  if (!existsSync(cli))
    throw new Error(
      `Missing compiled CLI: ${cli}; run cargo build before this suite (no PATH fallback).`,
    );
  for (const source of [
    "src/upgrade.rs",
    "src/upgrade/progress.rs",
    "install.sh",
  ])
    if (statSync(cli).mtimeMs < statSync(join(root, source)).mtimeMs)
      throw new Error(
        `Stale compiled CLI: ${cli}; rebuild the fork after editing ${source} (no PATH fallback).`,
      );
  const dir = realpathSync(
    mkdtempSync(join(tmpdir(), "crabcode-preview-ui-test-")),
  );
  dirs.push(dir);
  const home = join(dir, "home");
  const tools = join(dir, "tools");
  const temp = join(dir, "tmp");
  const prefix = custom ? join(home, "custom-bin") : join(home, ".local/bin");
  const installed = join(prefix, "crabcode");
  const config = join(home, ".config");
  for (const path of [tools, temp, prefix, config, join(dir, "payload")])
    mkdirSync(path, { recursive: true });
  writeFileSync(installed, oldBinary, { mode: 0o755 });
  writeFileSync(join(dir, "payload/crabcode"), newBinary, { mode: 0o755 });
  const archive = join(dir, "archive.tar.gz");
  const tar = spawnSync(
    "/usr/bin/tar",
    ["-czf", archive, "-C", join(dir, "payload"), "crabcode"],
    { encoding: "utf8" },
  );
  if (tar.status !== 0) throw new Error(`fixture tar failed: ${tar.stderr}`);
  writeFileSync(
    `${archive}.sha256`,
    `${createHash("sha256").update(readFileSync(archive)).digest("hex")}  archive.tar.gz\n`,
  );
  function tool(name: string, body: string) {
    writeFileSync(join(tools, name), `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  }
  tool(
    "uname",
    `case "$1" in -s) echo ${platform === "macos-arm64" ? "Darwin" : "Linux"} ;; -m) echo ${platform === "macos-arm64" ? "arm64" : "x86_64"} ;; *) exit 99 ;; esac`,
  );
  tool("cargo", 'echo "unexpected package-manager command" >&2; exit 99');
  // This is the only curl on the isolated PATH. Every unexpected URL fails
  // closed, including API/latest lookups: CLI latest uses reqwest and is NOT
  // exercised here (it cannot be safely faked by substituting curl).
  tool(
    "curl",
    `
set -eu
output= url= writeout=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|--output) output=$2; shift 2 ;;
    -w|--write-out) writeout=1; shift 2 ;;
    --proto|--proto-redir|--connect-timeout|--max-time|-H) shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
printf '%s\\n' "$url" >>"$FIXTURE/curl.log"
base="https://github.com/yan-ad/crabcode/releases/download/$EXPECTED_TAG"
asset="crabcode-$EXPECTED_SHA7-$EXPECTED_PLATFORM.tar.gz"
case "$url" in
  "$base/$asset") source="$FIXTURE/archive.tar.gz" ;;
  "$base/$asset.sha256") source="$FIXTURE/archive.tar.gz.sha256" ;;
  *) echo "unexpected or unpinned URL: $url" >&2; exit 99 ;;
esac
if [ "$MODE" = failure ] || { [ "$MODE" = checksum500 ] && [ "$url" = "$base/$asset.sha256" ]; }; then
  [ -z "$output" ] || printf partial >"$output"
  [ "$writeout" = 0 ] || printf 500
  echo 'curl: (22) HTTP 500 fixture failure' >&2
  exit 22
fi
if [ "$MODE" = legacy ] && [ "$url" = "$base/$asset.sha256" ]; then
  [ "$writeout" = 0 ] || printf 404
  echo 'curl: (22) HTTP 404 fixture checksum absent' >&2
  exit 22
fi
case "$url" in *.sha256) ;; *) sleep 0.15 ;; esac
if [ -n "$output" ]; then cp "$source" "$output"; else cat "$source"; fi
[ "$writeout" = 0 ] || printf 200
`,
  );
  // Deliberately don't inherit HOME, host PATH, install overrides or NO_COLOR.
  const env: Record<string, string> = {
    HOME: home,
    XDG_CONFIG_HOME: config,
    CARGO_HOME: join(home, ".cargo"),
    PATH: `${prefix}:${tools}:/usr/bin:/bin`,
    FIXTURE: dir,
    TMPDIR: temp,
    TERM: "xterm",
    MODE: "",
    EXPECTED_PLATFORM: platform,
    ...(custom ? { CRABCODE_INSTALL_DIR: prefix } : {}),
  };
  function run(
    version = target,
    overrides: Record<string, string> = {},
    columns?: number,
  ) {
    const fullEnv = {
      ...env,
      EXPECTED_TAG: version,
      EXPECTED_SHA7: version.slice(
        "gondescode-".length,
        "gondescode-".length + 7,
      ),
      ...overrides,
    };
    let executable = cli;
    let args = ["upgrade", version];
    const stdoutFile = join(dir, "cli.stdout");
    if (columns !== undefined) {
      executable = "/usr/bin/script";
      // Keep stderr on the real PTY but capture stdout independently. Disable
      // LF->CRLF translation so assertions measure rendering, not transport.
      const shell = `stty rows 24 cols ${columns} -onlcr; exec ${quote(cli)} upgrade ${quote(version)} </dev/null >${quote(stdoutFile)}`;
      args =
        process.platform === "darwin"
          ? ["-q", "/dev/null", "/bin/sh", "-c", shell]
          : ["-q", "-e", "-c", shell, "/dev/null"];
    }
    const result = spawnSync(executable, args, {
      env: fullEnv,
      encoding: "utf8",
      timeout: 5000,
    });
    if (result.error) throw result.error;
    expect(readdirSync(temp)).toEqual([]);
    return {
      ...result,
      cliStdout:
        columns === undefined
          ? result.stdout
          : readFileSync(stdoutFile, "utf8"),
      text:
        columns === undefined ? result.stderr : result.stdout + result.stderr,
    };
  }
  const log = () =>
    existsSync(join(dir, "curl.log"))
      ? readFileSync(join(dir, "curl.log"), "utf8")
      : "";
  return { dir, installed, prefix, platform, run, log };
}

function successful(
  f: ReturnType<typeof fixture>,
  result: ReturnType<ReturnType<typeof fixture>["run"]>,
  version = target,
) {
  expect(result.status).toBe(0);
  expect(result.cliStdout).toBe("");
  expect(result.text).toContain(`Installed version: ${current}`);
  expect(result.text).toContain(`Upgrading crabcode version: ${version}`);
  expect(result.text).toContain("Working");
  expect(result.text).toContain(
    `Upgraded crabcode to preview ${version}. Restart crabcode to apply.`,
  );
  expect(result.text).not.toContain("already up to date");
  expect(readFileSync(f.installed, "utf8")).toBe(newBinary);
  expect(existsSync(join(f.prefix, "bin/crabcode"))).toBe(false);
  const asset = `https://github.com/yan-ad/crabcode/releases/download/${version}/crabcode-${version.slice("gondescode-".length, "gondescode-".length + 7)}-${f.platform}.tar.gz`;
  expect(f.log().trim().split("\n")).toEqual([asset, `${asset}.sha256`]);
}

// Explicit preview targets only: never allow these tests to touch native net.
describe("fork preview CLI upgrade (offline)", () => {
  for (const platform of ["macos-arm64", "linux-x64"] as const) {
    test(`${platform}: pinned SHA7 archive replaces equal-semver installation; redirected UI is stderr only`, () => {
      const f = fixture(platform);
      const r = f.run();
      successful(f, r);
      expect(r.text).toContain("Working...");
      expect(r.text).not.toMatch(/[\x1b\r%]/);
      // Successful captured installer chatter is not replayed into CLI output.
      expect(r.text).not.toContain("Downloading...");
    });
  }
  test("without an install override defaults to isolated ~/.local/bin", () => {
    const f = fixture("linux-x64", false);
    successful(f, f.run());
  });
  for (const mode of ["failure", "checksum500", "mismatch"]) {
    test(`${mode}: preserves old executable and reports failure, never success or fallback`, () => {
      const f = fixture();
      if (mode === "mismatch")
        writeFileSync(
          join(f.dir, "archive.tar.gz.sha256"),
          `${"0".repeat(64)}  archive.tar.gz\n`,
        );
      const r = f.run(target, { MODE: mode });
      expect(r.status).not.toBe(0);
      expect(r.cliStdout).toBe("");
      expect(r.text).toContain(
        mode === "mismatch"
          ? "SHA256 checksum mismatch"
          : "HTTP 500 fixture failure",
      );
      expect(r.text).not.toContain("Upgraded crabcode to");
      expect(r.text).not.toMatch(/[\x1b\r%]/);
      expect(readFileSync(f.installed, "utf8")).toBe(oldBinary);
      expect(f.log()).not.toMatch(
        /latest|api\.github|crabcode-installer|tar\.xz/,
      );
    });
  }
  test("legacy checksum 404 still installs; captured warning need not reach CLI", () => {
    const f = fixture();
    successful(f, f.run(target, { MODE: "legacy" }));
  });
  for (const length of [7, 40, 64]) {
    test(`accepts ${length}-digit hexadecimal preview identity`, () => {
      const f = fixture();
      const version = `gondescode-${"A".repeat(length)}`;
      successful(f, f.run(version), version);
    });
  }
  for (const invalid of [
    current,
    `v${current}`,
    "nightly",
    "gondescode-",
    "gondescode-abc",
    `gondescode-${"a".repeat(6)}`,
    `gondescode-${"a".repeat(41)}`,
    `gondescode-${"a".repeat(63)}`,
    `gondescode-${"a".repeat(65)}`,
    "gondescode-abcdefg",
    "gondescode-abcd123;echo",
    "gondescode-../../main",
    "gondescode-abcd123\n",
  ]) {
    test(`invalid target ${JSON.stringify(invalid)} never invokes curl`, () => {
      const f = fixture();
      const r = f.run(invalid);
      expect(r.status).not.toBe(0);
      expect(r.cliStdout).toBe("");
      expect(r.text).toContain("invalid target release");
      expect(r.text).not.toContain("Working");
      expect(f.log()).toBe("");
      expect(readFileSync(f.installed, "utf8")).toBe(oldBinary);
    });
  }
  for (const columns of [20, 40, 80]) {
    test(`PTY ${columns} columns: amber Working fits without fabricated percentages`, () => {
      const f = fixture();
      const r = f.run(target, {}, columns);
      successful(f, r);
      expect(r.text).toContain("\x1b[33m");
      expect(r.text).not.toMatch(/\d+(?:\.\d+)?%/);
      const rows = [...r.text.matchAll(/\r\x1b\[2K([^\r\n]*)/g)]
        .map((m) => stripAnsi(m[1]))
        .filter((row) => row.includes(" Working"));
      expect(rows.length).toBeGreaterThan(0);
      for (const row of rows)
        expect([...row].length).toBeLessThanOrEqual(columns - 1);
    });
  }
  test("PTY NO_COLOR suppresses SGR while retaining clear-line rendering", () => {
    const f = fixture();
    const r = f.run(target, { NO_COLOR: "" }, 40);
    successful(f, r);
    expect(r.text).not.toMatch(/\x1b\[[0-9;]*m/);
    expect(r.text).toContain("\x1b[2K");
    expect(r.text).not.toContain("%");
  });
  test("PTY TERM=dumb has neither ANSI nor carriage-return rendering", () => {
    const f = fixture();
    const r = f.run(target, { TERM: "dumb" }, 40);
    successful(f, r);
    expect(r.text).toContain("Working...");
    expect(r.text).not.toMatch(/[\x1b\r%]/);
  });
});
