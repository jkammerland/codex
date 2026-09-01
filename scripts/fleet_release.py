#!/usr/bin/env python3
"""Synchronize the maintained Codex fork across native offload hosts."""

import argparse
import base64
import json
import ntpath
import posixpath
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "fleet" / "release.json"
COMMIT = re.compile(r"[0-9a-f]{40}")
SEMVER = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
SSH_OPTIONS = (
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=8",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ClearAllForwardings=yes",
)


class FleetError(RuntimeError):
    """The requested fleet state cannot be safely verified or applied."""


@dataclass(frozen=True)
class Release:
    name: str
    fork_commit: str
    fork_ref: str
    fork_remote: str
    cli_version: str
    server_commit: str
    rusty_v8_version: str

    @property
    def marker(self) -> str:
        return f"+{self.name}"

    @property
    def upstream_version(self) -> str:
        prefix = "codex-cli "
        if not self.cli_version.startswith(prefix):
            raise FleetError("expectedCliVersion must start with 'codex-cli '")
        return self.cli_version.removeprefix(prefix).partition("+")[0]


@dataclass(frozen=True)
class Host:
    name: str
    platform: str
    ssh_target: str
    source_root: str
    build_root: str
    server_root: str
    cli_binary: str
    code_mode_host_binary: str
    rust_toolchain: str
    rust_target: str
    rustup: str | None

    @property
    def windows(self) -> bool:
        return self.platform == "windows"

    def build_binary(self, name: str) -> str:
        separator = "\\" if self.windows else "/"
        extension = ".exe" if self.windows else ""
        return separator.join(
            [
                self.build_root,
                "codex-rs",
                "target",
                "release",
                f"{name}{extension}",
            ]
        )

    def bundle_path(self, release: Release) -> str:
        dirname = ntpath.dirname if self.windows else posixpath.dirname
        separator = "\\" if self.windows else "/"
        return f"{dirname(self.source_root)}{separator}codex-{release.name}.bundle"

    def build_receipt(self) -> str:
        separator = "\\" if self.windows else "/"
        return f"{self.build_root}.fleet{separator}build-receipt-v1"


@dataclass(frozen=True)
class Status:
    host: Host
    values: dict[str, str]
    error: str | None = None

    def drift(self, release: Release) -> tuple[str, ...]:
        if self.error:
            return ("transport",)
        expected = {
            "cliVersion": release.cli_version,
            "serverCommit": release.server_commit,
            "sourceCommit": release.fork_commit,
            "receiptForkCommit": release.fork_commit,
            "receiptCliVersion": release.cli_version,
            "codeModeHostExecutable": "true",
        }
        drift = [
            key for key, value in expected.items() if self.values.get(key) != value
        ]
        drift.extend(
            key for key in ("serverStatus", "sourceStatus") if self.values.get(key, "")
        )
        drift.extend(
            active_key
            for active_key, receipt_key in (
                ("cliSha256", "receiptCliSha256"),
                ("codeModeHostSha256", "receiptCodeModeHostSha256"),
            )
            if self.values.get(active_key) != self.values.get(receipt_key)
        )
        return tuple(drift)


def required(value: dict[str, Any], key: str) -> str:
    result = value.get(key)
    if not isinstance(result, str) or not result:
        raise FleetError(f"{key} must be a non-empty string")
    return result


def absolute(platform: str, name: str, value: str) -> None:
    is_absolute = (
        bool(re.match(r"^[A-Za-z]:[\\/]", value))
        if platform == "windows"
        else value.startswith("/")
    )
    if not is_absolute:
        raise FleetError(f"{name} must be an absolute {platform} path")


def host_from_raw(raw: Any) -> Host:
    if not isinstance(raw, dict):
        raise FleetError("every host must be an object")
    platform = required(raw, "platform")
    if platform not in {"darwin", "windows"}:
        raise FleetError("host platform must be darwin or windows")
    paths = {
        key: required(raw, key)
        for key in (
            "sourceRoot",
            "buildRoot",
            "serverRoot",
            "cliBinary",
            "codeModeHostBinary",
        )
    }
    for name, path in paths.items():
        absolute(platform, name, path)
    rustup = raw.get("rustup")
    if rustup is not None:
        if not isinstance(rustup, str) or not rustup:
            raise FleetError("rustup must be a non-empty string when provided")
        absolute(platform, "rustup", rustup)
    if platform == "darwin" and rustup is None:
        raise FleetError("darwin hosts require rustup")
    rust_target = required(raw, "rustTarget")
    expected_target_suffix = (
        "-pc-windows-msvc" if platform == "windows" else "-apple-darwin"
    )
    if not rust_target.endswith(expected_target_suffix):
        raise FleetError(
            f"rustTarget for {platform} must end with {expected_target_suffix}"
        )
    return Host(
        name=required(raw, "name"),
        platform=platform,
        ssh_target=required(raw, "sshTarget"),
        source_root=paths["sourceRoot"],
        build_root=paths["buildRoot"],
        server_root=paths["serverRoot"],
        cli_binary=paths["cliBinary"],
        code_mode_host_binary=paths["codeModeHostBinary"],
        rust_toolchain=required(raw, "rustToolchain"),
        rust_target=rust_target,
        rustup=rustup,
    )


def load_manifest(path: Path) -> tuple[Release, tuple[Host, ...]]:
    try:
        raw = json.loads(path.read_text())
    except OSError as error:
        raise FleetError(f"could not read manifest {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise FleetError(f"invalid JSON in {path}: {error}") from error
    if not isinstance(raw, dict) or raw.get("schemaVersion") != 1:
        raise FleetError("manifest schemaVersion must be 1")
    release_raw = raw.get("release")
    if not isinstance(release_raw, dict):
        raise FleetError("release must be an object")
    commits = {
        key: required(release_raw, key) for key in ("forkCommit", "mcpServerCommit")
    }
    if any(COMMIT.fullmatch(commit) is None for commit in commits.values()):
        raise FleetError("release commits must be 40-character lowercase Git commits")
    rusty_v8_version = required(release_raw, "rustyV8Version")
    if SEMVER.fullmatch(rusty_v8_version) is None:
        raise FleetError("rustyV8Version must contain three numeric components")
    release = Release(
        name=required(release_raw, "name"),
        fork_commit=commits["forkCommit"],
        fork_ref=required(release_raw, "forkRef"),
        fork_remote=required(release_raw, "forkRemote"),
        cli_version=required(release_raw, "expectedCliVersion"),
        server_commit=commits["mcpServerCommit"],
        rusty_v8_version=rusty_v8_version,
    )
    release.upstream_version
    hosts_raw = raw.get("hosts")
    if not isinstance(hosts_raw, list) or not hosts_raw:
        raise FleetError("hosts must be a non-empty array")
    hosts = tuple(host_from_raw(host) for host in hosts_raw)
    if len({host.name for host in hosts}) != len(hosts):
        raise FleetError("host names must be unique")
    return release, hosts


def selected_hosts(
    hosts: tuple[Host, ...], names: list[str] | None
) -> tuple[Host, ...]:
    if not names:
        return hosts
    index = {host.name: host for host in hosts}
    try:
        return tuple(index[name] for name in names)
    except KeyError as error:
        raise FleetError(f"unknown host {error.args[0]!r}") from error


def quote_ps(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def remote(host: Host, script: str) -> subprocess.CompletedProcess[str]:
    if host.windows:
        encoded = base64.b64encode(script.encode("utf-16le")).decode("ascii")
        command = [
            "ssh",
            *SSH_OPTIONS,
            host.ssh_target,
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            encoded,
        ]
        return subprocess.run(command, check=False, capture_output=True, text=True)
    return subprocess.run(
        ["ssh", *SSH_OPTIONS, host.ssh_target, "sh -s"],
        check=False,
        capture_output=True,
        input=script,
        text=True,
    )


def error_message(action: str, result: subprocess.CompletedProcess[str]) -> str:
    output = "\n".join(part for part in (result.stdout, result.stderr) if part).strip()
    return f"{action} failed with exit code {result.returncode}: {output}"


def status_script(host: Host) -> str:
    if host.windows:
        return f"""$ErrorActionPreference = 'Stop'
function Field($Name, $Value) {{ Write-Output "$Name=$Value" }}
function GitValue($Path, [string[]]$Arguments) {{
  if (-not (Test-Path -LiteralPath (Join-Path $Path '.git'))) {{ return 'missing' }}
  $value = ((& git -C $Path @Arguments 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw "git failed in $Path: $($Arguments -join ' ')" }}
  return $value
}}
function HashValue($Path) {{
  if (-not (Test-Path -LiteralPath $Path)) {{ return 'missing' }}
  return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}}
function ReceiptValue($Path, $Name) {{
  if (-not (Test-Path -LiteralPath $Path)) {{ return 'missing' }}
  $prefix = "$Name="
  $matches = @(Get-Content -LiteralPath $Path | Where-Object {{ $_.StartsWith($prefix, [StringComparison]::Ordinal) }})
  if ($matches.Count -ne 1) {{ return 'invalid' }}
  return $matches[0].Substring($prefix.Length)
}}
$cli = {quote_ps(host.cli_binary)}
$codeModeHost = {quote_ps(host.code_mode_host_binary)}
$receipt = {quote_ps(host.build_receipt())}
$server = {quote_ps(host.server_root)}
$source = {quote_ps(host.source_root)}
if (Test-Path -LiteralPath $cli) {{
  $cliVersion = ((& $cli --version 2>&1 | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw 'Codex version check failed' }}
}} else {{ $cliVersion = 'missing' }}
Field 'cliVersion' $cliVersion
Field 'cliSha256' (HashValue $cli)
Field 'codeModeHostSha256' (HashValue $codeModeHost)
Field 'codeModeHostExecutable' $(if (Test-Path -LiteralPath $codeModeHost -PathType Leaf) {{ 'true' }} else {{ 'false' }})
Field 'receiptForkCommit' (ReceiptValue $receipt 'forkCommit')
Field 'receiptCliVersion' (ReceiptValue $receipt 'cliVersion')
Field 'receiptCliSha256' (ReceiptValue $receipt 'cliSha256')
Field 'receiptCodeModeHostSha256' (ReceiptValue $receipt 'hostSha256')
Field 'serverCommit' (GitValue $server @('rev-parse', 'HEAD'))
Field 'serverStatus' (GitValue $server @('status', '--porcelain'))
Field 'sourceCommit' (GitValue $source @('rev-parse', 'HEAD'))
Field 'sourceStatus' (GitValue $source @('status', '--porcelain'))
"""
    return f"""set -u
field() {{ printf '%s=%s\\n' "$1" "$2"; }}
git_value() {{ path=$1; shift; if [ -d "$path/.git" ]; then git -C "$path" "$@" 2>/dev/null; else printf missing; fi; }}
hash_value() {{ if [ -f "$1" ]; then shasum -a 256 "$1" | awk '{{print $1}}'; else printf missing; fi; }}
executable_value() {{ if [ -x "$1" ]; then printf true; else printf false; fi; }}
receipt_value() {{ if [ ! -f "$2" ]; then printf missing; return; fi; value=$(awk -v key="$1" 'index($0, key "=") == 1 {{ count++; value=substr($0, length(key) + 2) }} END {{ if (count != 1) exit 1; print value }}' "$2") || {{ printf invalid; return; }}; printf '%s' "$value"; }}
cli={shlex.quote(host.cli_binary)}
code_mode_host={shlex.quote(host.code_mode_host_binary)}
receipt={shlex.quote(host.build_receipt())}
if [ -x "$cli" ]; then cli_version=$("$cli" --version 2>&1) || exit 1; else cli_version=missing; fi
server_commit=$(git_value {shlex.quote(host.server_root)} rev-parse HEAD) || exit 1
server_status=$(git_value {shlex.quote(host.server_root)} status --porcelain) || exit 1
source_commit=$(git_value {shlex.quote(host.source_root)} rev-parse HEAD) || exit 1
source_status=$(git_value {shlex.quote(host.source_root)} status --porcelain) || exit 1
field cliVersion "$cli_version"
field cliSha256 "$(hash_value "$cli")"
field codeModeHostSha256 "$(hash_value "$code_mode_host")"
field codeModeHostExecutable "$(executable_value "$code_mode_host")"
field receiptForkCommit "$(receipt_value forkCommit "$receipt")"
field receiptCliVersion "$(receipt_value cliVersion "$receipt")"
field receiptCliSha256 "$(receipt_value cliSha256 "$receipt")"
field receiptCodeModeHostSha256 "$(receipt_value hostSha256 "$receipt")"
field serverCommit "$server_commit"
field serverStatus "$server_status"
field sourceCommit "$source_commit"
field sourceStatus "$source_status"
"""


def parse_status(output: str) -> dict[str, str]:
    fields = {
        "cliVersion",
        "cliSha256",
        "codeModeHostSha256",
        "codeModeHostExecutable",
        "receiptForkCommit",
        "receiptCliVersion",
        "receiptCliSha256",
        "receiptCodeModeHostSha256",
        "serverCommit",
        "serverStatus",
        "sourceCommit",
        "sourceStatus",
    }
    return {
        key: value.strip()
        for line in output.splitlines()
        for key, separator, value in [line.partition("=")]
        if separator and key in fields
    }


def host_status(host: Host) -> Status:
    result = remote(host, status_script(host))
    if result.returncode:
        return Status(host, {}, error_message("status", result))
    values = parse_status(result.stdout)
    if len(values) != 12:
        return Status(host, values, "status output was incomplete")
    return Status(host, values)


def require_server(host: Host, release: Release) -> None:
    status = host_status(host)
    if (
        status.error
        or status.values.get("serverCommit") != release.server_commit
        or status.values.get("serverStatus")
    ):
        detail = status.error or json.dumps(status.values, sort_keys=True)
        raise FleetError(f"{host.name}: MCP server release gate failed: {detail}")


def copy_bundle(host: Host, release: Release, bundle: Path) -> None:
    destination = host.bundle_path(release).replace("\\", "/")
    result = subprocess.run(
        ["scp", *SSH_OPTIONS, str(bundle), f"{host.ssh_target}:{destination}"],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        raise FleetError(error_message(f"copying bundle to {host.name}", result))


def build_script(host: Host, release: Release, use_bundle: bool) -> str:
    source = host.source_root
    build = host.build_root
    staged_ref = f"refs/remotes/fleet/{release.fork_ref}"
    v8_profile = "ptrcomp_sandbox_release"
    v8_archive = (
        f"rusty_v8_{v8_profile}_{host.rust_target}.lib.gz"
        if host.windows
        else f"librusty_v8_{v8_profile}_{host.rust_target}.a.gz"
    )
    v8_binding = f"src_binding_{v8_profile}_{host.rust_target}.rs"
    v8_checksums = f"rusty_v8_{v8_profile}_{host.rust_target}.sha256"
    v8_base_url = (
        "https://github.com/openai/codex/releases/download/"
        f"rusty-v8-v{release.rusty_v8_version}"
    )
    fetch = (
        f"git -C {shlex.quote(source)} fetch {shlex.quote(host.bundle_path(release))} refs/heads/{release.fork_ref}:{staged_ref}"
        if use_bundle
        else f"git -C {shlex.quote(source)} fetch fork {shlex.quote(release.fork_ref)}:{staged_ref}"
    )
    if host.windows:
        ps_fetch = (
            f"git -C $source fetch {quote_ps(host.bundle_path(release))} refs/heads/{release.fork_ref}:{staged_ref}"
            if use_bundle
            else f"git -C $source fetch fork {quote_ps(release.fork_ref)}:{staged_ref}"
        )
        return f"""$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
function Normalize-ReleaseLock($Path) {{
  $status = @(& git -C $Path status --short --untracked-files=all)
  if ($LASTEXITCODE -ne 0) {{ throw 'Could not inspect the build root status' }}
  if ($status.Count -eq 0) {{ return }}
  if ($status.Count -ne 1 -or $status[0].Trim() -ne 'M codex-rs/Cargo.lock') {{ throw 'Build root contains changes other than the generated release lockfile' }}
  $changedLines = @(& git -C $Path diff --unified=0 -- codex-rs/Cargo.lock | Where-Object {{ ($_ -match '^[+-]') -and ($_ -notmatch '^(---|\\+\\+\\+)') }})
  $invalidLines = @($changedLines | Where-Object {{ $_ -notmatch '^[+-]version = "(0\\.0\\.0|{re.escape(release.upstream_version)})"$' }})
  if ($changedLines.Count -eq 0 -or $invalidLines.Count -ne 0) {{ throw 'Build root Cargo.lock contains unexpected changes' }}
  & git -C $Path restore --worktree -- codex-rs/Cargo.lock
  if ($LASTEXITCODE -ne 0) {{ throw 'Could not normalize the generated release lockfile' }}
}}
$source = {quote_ps(source)}; $build = {quote_ps(build)}
if (Test-Path -LiteralPath $source) {{
  if (-not (Test-Path -LiteralPath (Join-Path $source '.git'))) {{ throw 'Source root must be a Git checkout' }}
  $sourceStatus = ((& git -C $source status --porcelain 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0 -or -not [string]::IsNullOrEmpty($sourceStatus)) {{ throw 'Source root must be a clean Git checkout' }}
}} else {{ git clone --origin fork {quote_ps(release.fork_remote)} $source; if ($LASTEXITCODE -ne 0) {{ throw 'Clone failed' }} }}
{ps_fetch}; if ($LASTEXITCODE -ne 0) {{ throw 'Fetch failed' }}
git -C $source switch -C {quote_ps(release.fork_ref)} {quote_ps(staged_ref)}; if ($LASTEXITCODE -ne 0) {{ throw 'Switch failed' }}
$sourceCommit = ((& git -C $source rev-parse HEAD 2>$null | Out-String).Trim())
if ($LASTEXITCODE -ne 0 -or $sourceCommit -ne {quote_ps(release.fork_commit)}) {{ throw 'Source commit does not match manifest' }}
if (Test-Path -LiteralPath $build) {{
  Normalize-ReleaseLock $build
  if (-not (Test-Path -LiteralPath (Join-Path $build '.git'))) {{ throw 'Existing build root is not a Git worktree' }}
  $buildStatus = ((& git -C $build status --porcelain 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw 'Could not inspect the build root status' }}
  $buildCommit = ((& git -C $build rev-parse HEAD 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0 -or -not [string]::IsNullOrEmpty($buildStatus) -or $buildCommit -ne {quote_ps(release.fork_commit)}) {{ throw 'Existing build root is not the clean release worktree' }}
}} else {{ git -C $source worktree add --detach $build {quote_ps(release.fork_commit)}; if ($LASTEXITCODE -ne 0) {{ throw 'Worktree creation failed' }} }}
$vswhere = Join-Path ${"{"}env:ProgramFiles(x86){"}"} 'Microsoft Visual Studio\\Installer\\vswhere.exe'
$vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrEmpty($vs)) {{ throw 'No x64 MSVC toolchain found' }}
$v8Directory = Join-Path ($build + '.fleet') 'rusty-v8'
New-Item -ItemType Directory -Force -Path $v8Directory | Out-Null
$v8BaseUrl = {quote_ps(v8_base_url)}
$v8ArchiveName = {quote_ps(v8_archive)}
$v8BindingName = {quote_ps(v8_binding)}
$v8ChecksumsName = {quote_ps(v8_checksums)}
foreach ($name in @($v8ArchiveName, $v8BindingName, $v8ChecksumsName)) {{
  $destination = Join-Path $v8Directory $name
  $partial = "$destination.partial-$PID-$([guid]::NewGuid().ToString('N'))"
  try {{
    Invoke-WebRequest -UseBasicParsing -Uri "$v8BaseUrl/$name" -OutFile $partial
    Move-Item -Force -LiteralPath $partial -Destination $destination
  }} finally {{
    if (Test-Path -LiteralPath $partial) {{ Remove-Item -Force -LiteralPath $partial }}
  }}
}}
$checksumLines = @(Get-Content -LiteralPath (Join-Path $v8Directory $v8ChecksumsName) | Where-Object {{ -not [string]::IsNullOrWhiteSpace($_) }})
if ($checksumLines.Count -ne 2) {{ throw 'Expected exactly two rusty_v8 checksums' }}
foreach ($line in $checksumLines) {{
  if ($line -notmatch '^([0-9a-fA-F]{{64}})\\s+\\*?(.+)$') {{ throw "Invalid rusty_v8 checksum line: $line" }}
  $expectedHash = $Matches[1]
  $artifactPath = Join-Path $v8Directory $Matches[2]
  if (-not (Test-Path -LiteralPath $artifactPath)) {{ throw "Missing rusty_v8 checksum target: $artifactPath" }}
  $actualHash = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash
  if ($actualHash -ne $expectedHash) {{ throw "rusty_v8 checksum mismatch: $artifactPath" }}
}}
$env:RUSTY_V8_ARCHIVE = Join-Path $v8Directory $v8ArchiveName
$env:RUSTY_V8_SRC_BINDING_PATH = Join-Path $v8Directory $v8BindingName
$cargo = Join-Path $env:USERPROFILE '.cargo\\bin\\cargo.exe'
$command = 'call "{{0}}\\Common7\\Tools\\VsDevCmd.bat" -arch=amd64 && cd /d "{{1}}\\codex-rs" && "{{2}}" +{{3}} build --release --bin codex --bin codex-code-mode-host' -f $vs, $build, $cargo, {quote_ps(host.rust_toolchain)}
cmd.exe /d /s /c $command; if ($LASTEXITCODE -ne 0) {{ throw 'Cargo build failed' }}
Normalize-ReleaseLock $build
$version = & {quote_ps(host.build_binary("codex"))} --version
if ($version -ne {quote_ps(release.cli_version)}) {{ throw "Unexpected fork version: $version" }}
& {quote_ps(host.build_binary("codex"))} agents --help *> $null
if ($LASTEXITCODE -ne 0) {{ throw 'Built fork does not support codex agents' }}
& {quote_ps(host.build_binary("codex-code-mode-host"))} --help *> $null
if ($LASTEXITCODE -ne 0) {{ throw 'Built code-mode host is not executable' }}
$receipt = {quote_ps(host.build_receipt())}
$receiptPartial = "$receipt.partial-$PID-$([guid]::NewGuid().ToString('N'))"
$receiptLines = @(
  'format=v1',
  'forkCommit={release.fork_commit}',
  'cliVersion={release.cli_version}',
  "cliSha256=$((Get-FileHash -LiteralPath {quote_ps(host.build_binary("codex"))} -Algorithm SHA256).Hash.ToLowerInvariant())",
  "hostSha256=$((Get-FileHash -LiteralPath {quote_ps(host.build_binary("codex-code-mode-host"))} -Algorithm SHA256).Hash.ToLowerInvariant())"
)
try {{
  [System.IO.File]::WriteAllLines($receiptPartial, $receiptLines, [System.Text.UTF8Encoding]::new($false))
  Move-Item -Force -LiteralPath $receiptPartial -Destination $receipt
}} finally {{
  if (Test-Path -LiteralPath $receiptPartial) {{ Remove-Item -Force -LiteralPath $receiptPartial }}
}}
Write-Output "build-version=$version"
"""
    assert host.rustup is not None
    return f"""set -eu
normalize_release_lock() {{
  status=$(git -C "$1" status --short --untracked-files=all)
  [ -z "$status" ] && return
  [ "$status" = ' M codex-rs/Cargo.lock' ] || {{ echo 'Build root contains changes other than the generated release lockfile' >&2; exit 1; }}
  changed_lines=$(git -C "$1" diff --unified=0 -- codex-rs/Cargo.lock | sed -n -e '/^---/d' -e '/^+++/d' -e '/^[+-]/p')
  [ -n "$changed_lines" ] || {{ echo 'Build root Cargo.lock has no recoverable release-version changes' >&2; exit 1; }}
  invalid_lines=$(printf '%s\\n' "$changed_lines" | grep -Ev '^[+-]version = "(0\\.0\\.0|{re.escape(release.upstream_version)})"$' || true)
  [ -z "$invalid_lines" ] || {{ echo 'Build root Cargo.lock contains unexpected changes' >&2; exit 1; }}
  git -C "$1" restore --worktree -- codex-rs/Cargo.lock
}}
source={shlex.quote(source)}; build={shlex.quote(build)}
if [ -e "$source" ]; then
  [ -d "$source/.git" ] || exit 1
  source_status=$(git -C "$source" status --porcelain) || exit 1
  [ -z "$source_status" ] || exit 1
else
  git clone --origin fork {shlex.quote(release.fork_remote)} "$source"
fi
{fetch}
git -C "$source" switch -C {shlex.quote(release.fork_ref)} {shlex.quote(staged_ref)}
source_commit=$(git -C "$source" rev-parse HEAD) || exit 1
source_status=$(git -C "$source" status --porcelain) || exit 1
[ "$source_commit" = {release.fork_commit} ]
[ -z "$source_status" ]
if [ -e "$build" ]; then
  normalize_release_lock "$build"
  [ -e "$build/.git" ] || exit 1
  build_status=$(git -C "$build" status --porcelain) || exit 1
  build_commit=$(git -C "$build" rev-parse HEAD) || exit 1
  [ -z "$build_status" ] && [ "$build_commit" = {release.fork_commit} ]
else
  git -C "$source" worktree add --detach "$build" {release.fork_commit}
fi
cd "$build/codex-rs"
fleet_directory="$build.fleet"
v8_directory="$fleet_directory/rusty-v8"
receipt="$fleet_directory/build-receipt-v1"
v8_base_url={shlex.quote(v8_base_url)}
v8_archive_name={shlex.quote(v8_archive)}
v8_binding_name={shlex.quote(v8_binding)}
v8_checksums_name={shlex.quote(v8_checksums)}
mkdir -p "$v8_directory"
cleanup_v8_downloads() {{ rm -f "$v8_directory"/*.partial.$$ "$receipt.partial.$$"; }}
trap cleanup_v8_downloads EXIT HUP INT TERM
download_v8_artifact() {{
  destination="$v8_directory/$1"
  partial="$destination.partial.$$"
  rm -f "$partial"
  curl -fsSL "$v8_base_url/$1" -o "$partial"
  mv -f "$partial" "$destination"
}}
download_v8_artifact "$v8_archive_name"
download_v8_artifact "$v8_binding_name"
download_v8_artifact "$v8_checksums_name"
[ "$(grep -cve '^[[:space:]]*$' "$v8_directory/$v8_checksums_name")" -eq 2 ]
(cd "$v8_directory" && tr -d '\\r' < "$v8_checksums_name" | shasum -a 256 -c -)
rustc=$({shlex.quote(host.rustup)} which --toolchain {shlex.quote(host.rust_toolchain)} rustc)
toolchain_bin=$(dirname "$rustc")
RUSTY_V8_ARCHIVE="$v8_directory/$v8_archive_name" \\
RUSTY_V8_SRC_BINDING_PATH="$v8_directory/$v8_binding_name" \\
PATH="$toolchain_bin:$PATH" \\
  "$toolchain_bin/cargo" build --release --bin codex --bin codex-code-mode-host
normalize_release_lock "$build"
version=$(target/release/codex --version)
target/release/codex agents --help >/dev/null
target/release/codex-code-mode-host --help >/dev/null
[ "$version" = {shlex.quote(release.cli_version)} ] || {{ echo "Unexpected fork version: $version" >&2; exit 1; }}
cli_hash=$(shasum -a 256 target/release/codex | awk '{{print $1}}')
host_hash=$(shasum -a 256 target/release/codex-code-mode-host | awk '{{print $1}}')
printf '%s\\n' \
  'format=v1' \
  'forkCommit={release.fork_commit}' \
  {shlex.quote(f"cliVersion={release.cli_version}")} \
  "cliSha256=$cli_hash" \
  "hostSha256=$host_hash" > "$receipt.partial.$$"
mv -f "$receipt.partial.$$" "$receipt"
printf 'build-version=%s\\n' "$version"
"""


def activation_script(host: Host, release: Release) -> str:
    source_cli = host.build_binary("codex")
    source_host = host.build_binary("codex-code-mode-host")
    receipt = host.build_receipt()
    backup_cli = f"{host.cli_binary}.{release.name}-previous"
    backup_host = f"{host.code_mode_host_binary}.{release.name}-previous"
    if host.windows:
        return f"""$ErrorActionPreference = 'Stop'
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class FleetFileOps {{ [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)] [return: MarshalAs(UnmanagedType.Bool)] public static extern bool ReplaceFile(string target, string replacement, string backup, uint flags, IntPtr exclude, IntPtr reserved); }}
'@
function Hash($Path) {{ (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }}
function Assert-CleanGit($Path, $ExpectedCommit, $Label) {{
  if (-not (Test-Path -LiteralPath (Join-Path $Path '.git'))) {{ throw "$Label is not a Git checkout" }}
  $commit = ((& git -C $Path rev-parse HEAD 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw "Could not inspect $Label commit" }}
  $status = ((& git -C $Path status --porcelain 2>$null | Out-String).Trim())
  if ($LASTEXITCODE -ne 0) {{ throw "Could not inspect $Label status" }}
  if ($commit -ne $ExpectedCommit -or -not [string]::IsNullOrEmpty($status)) {{ throw "$Label is not clean at the manifest commit" }}
}}
function Receipt-Value($Path, $Name) {{
  $prefix = "$Name="
  $matches = @(Get-Content -LiteralPath $Path | Where-Object {{ $_.StartsWith($prefix, [StringComparison]::Ordinal) }})
  if ($matches.Count -ne 1) {{ throw "Invalid build receipt field: $Name" }}
  return $matches[0].Substring($prefix.Length)
}}
function Replace($Target, $Source, $Backup) {{
  $stage = "$Target.{release.name}-$([guid]::NewGuid().ToString('N'))"
  try {{
    Copy-Item -LiteralPath $Source -Destination $stage
    if ((Hash $stage) -ne (Hash $Source)) {{ throw "Staging verification failed: $Target" }}
    if (-not [FleetFileOps]::ReplaceFile($Target, $stage, $Backup, 0, [IntPtr]::Zero, [IntPtr]::Zero)) {{ throw "Atomic replacement failed: $Target" }}
  }} finally {{
    if (Test-Path -LiteralPath $stage) {{ Remove-Item -Force -LiteralPath $stage }}
  }}
}}
function Restore-One($Target, $Backup) {{
  $stage = "$Target.rollback-$([guid]::NewGuid().ToString('N'))"
  try {{
    Copy-Item -LiteralPath $Backup -Destination $stage
    if (-not [FleetFileOps]::ReplaceFile($Target, $stage, $null, 0, [IntPtr]::Zero, [IntPtr]::Zero)) {{ throw "Rollback failed: $Target" }}
  }} finally {{
    if (Test-Path -LiteralPath $stage) {{ Remove-Item -Force -LiteralPath $stage }}
  }}
}}
$canonical = {quote_ps(host.source_root)}; $build = {quote_ps(host.build_root)}; $sourceCli = {quote_ps(source_cli)}; $sourceHost = {quote_ps(source_host)}; $receipt = {quote_ps(receipt)}
$targetCli = {quote_ps(host.cli_binary)}; $targetHost = {quote_ps(host.code_mode_host_binary)}
$backupCli = {quote_ps(backup_cli)}; $backupHost = {quote_ps(backup_host)}
$server = {quote_ps(host.server_root)}
Assert-CleanGit $canonical {quote_ps(release.fork_commit)} 'Canonical checkout'
Assert-CleanGit $build {quote_ps(release.fork_commit)} 'Build worktree'
Assert-CleanGit $server {quote_ps(release.server_commit)} 'MCP server'
foreach ($path in @($sourceCli, $sourceHost, $targetCli, $targetHost)) {{ if (-not (Test-Path -LiteralPath $path)) {{ throw "Missing binary: $path" }} }}
if (-not (Test-Path -LiteralPath $receipt)) {{ throw 'Missing reviewed build receipt' }}
$sourceCliHash = Hash $sourceCli; $sourceHostHash = Hash $sourceHost
if ((Receipt-Value $receipt 'format') -ne 'v1' -or
    (Receipt-Value $receipt 'forkCommit') -ne {quote_ps(release.fork_commit)} -or
    (Receipt-Value $receipt 'cliVersion') -ne {quote_ps(release.cli_version)} -or
    (Receipt-Value $receipt 'cliSha256') -ne $sourceCliHash -or
    (Receipt-Value $receipt 'hostSha256') -ne $sourceHostHash) {{ throw 'Build receipt does not match the reviewed binary pair' }}
$needsActivation = (Hash $targetCli) -ne $sourceCliHash -or (Hash $targetHost) -ne $sourceHostHash
$backupCliExists = Test-Path -LiteralPath $backupCli
$backupHostExists = Test-Path -LiteralPath $backupHost
if ($backupCliExists -ne $backupHostExists) {{ throw 'Release rollback pair is incomplete' }}
if ($needsActivation) {{
  if ($backupCliExists) {{
    if ((Hash $targetCli) -ne (Hash $backupCli) -or (Hash $targetHost) -ne (Hash $backupHost)) {{ throw 'Release rollback pair does not match the active binaries' }}
    Remove-Item -Force -LiteralPath $backupCli, $backupHost
  }}
  $oldCliHash = Hash $targetCli; $oldHostHash = Hash $targetHost
  try {{
    Replace $targetCli $sourceCli $backupCli
    if ((Hash $targetCli) -ne $sourceCliHash -or (Hash $backupCli) -ne $oldCliHash) {{ throw 'CLI replacement verification failed' }}
    Replace $targetHost $sourceHost $backupHost
    if ((Hash $targetHost) -ne $sourceHostHash -or (Hash $backupHost) -ne $oldHostHash) {{ throw 'Code-mode host replacement verification failed' }}
    $version = & $targetCli --version
    if ($version -ne {quote_ps(release.cli_version)}) {{ throw "Unexpected fork version: $version" }}
    & $targetCli agents --help *> $null
    if ($LASTEXITCODE -ne 0) {{ throw 'Activated fork does not support codex agents' }}
    & $targetHost --help *> $null
    if ($LASTEXITCODE -ne 0) {{ throw 'Activated code-mode host is not executable' }}
  }} catch {{
    $activationError = $_.Exception.Message
    $rollbackErrors = [System.Collections.Generic.List[string]]::new()
    if (Test-Path -LiteralPath $backupHost) {{ try {{ Restore-One $targetHost $backupHost }} catch {{ $rollbackErrors.Add($_.Exception.Message) }} }}
    if (Test-Path -LiteralPath $backupCli) {{ try {{ Restore-One $targetCli $backupCli }} catch {{ $rollbackErrors.Add($_.Exception.Message) }} }}
    if ((Hash $targetCli) -ne $oldCliHash) {{ $rollbackErrors.Add('CLI rollback hash mismatch') }}
    if ((Hash $targetHost) -ne $oldHostHash) {{ $rollbackErrors.Add('Code-mode host rollback hash mismatch') }}
    if ($rollbackErrors.Count -eq 0) {{ Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $backupCli, $backupHost }}
    if ($rollbackErrors.Count -ne 0) {{ throw "Activation failed: $activationError; rollback failed: $($rollbackErrors -join '; '); recovery copies: $backupCli, $backupHost" }}
    throw "Activation failed and both binaries were restored: $activationError"
  }}
}} else {{
  $version = & $targetCli --version
  if ($version -ne {quote_ps(release.cli_version)}) {{ throw "Unexpected fork version: $version" }}
  & $targetCli agents --help *> $null
  if ($LASTEXITCODE -ne 0) {{ throw 'Activated fork does not support codex agents' }}
  & $targetHost --help *> $null
  if ($LASTEXITCODE -ne 0) {{ throw 'Activated code-mode host is not executable' }}
}}
Write-Output "activated-version=$version"
"""
    return f"""set -eu
sha() {{ shasum -a 256 "$1" | awk '{{print $1}}'; }}
file_mode() {{ stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }}
git_clean_at() {{ path=$1; expected=$2; commit=$(git -C "$path" rev-parse HEAD) || return 1; status=$(git -C "$path" status --porcelain) || return 1; [ "$commit" = "$expected" ] && [ -z "$status" ]; }}
receipt_value() {{ awk -v key="$1" 'index($0, key "=") == 1 {{ count++; value=substr($0, length(key) + 2) }} END {{ if (count != 1) exit 1; print value }}' "$2"; }}
canonical={shlex.quote(host.source_root)}; build={shlex.quote(host.build_root)}; source_cli={shlex.quote(source_cli)}; source_host={shlex.quote(source_host)}; receipt={shlex.quote(receipt)}
target_cli={shlex.quote(host.cli_binary)}; target_host={shlex.quote(host.code_mode_host_binary)}
backup_cli={shlex.quote(backup_cli)}; backup_host={shlex.quote(backup_host)}
server={shlex.quote(host.server_root)}
activated=0; repaired_modes=0; old_cli_mode=; old_host_mode=; old_cli_hash=; old_host_hash=; restore_cli=; restore_host=; stage_cli=; stage_host=
rollback_pair() {{
  [ "$activated" -eq 1 ] || return 0
  restore_cli=$(mktemp "$target_cli.rollback.XXXXXX"); restore_host=$(mktemp "$target_host.rollback.XXXXXX")
  if ! cp -p "$backup_cli" "$restore_cli" || ! cp -p "$backup_host" "$restore_host"; then
    rm -f "$restore_cli" "$restore_host"
    restore_cli=; restore_host=
    return 1
  fi
  rollback_failed=0
  if ! mv -f "$restore_host" "$target_host"; then rollback_failed=1; fi
  restore_host=
  if ! mv -f "$restore_cli" "$target_cli"; then rollback_failed=1; fi
  restore_cli=
  [ "$(sha "$target_cli")" = "$old_cli_hash" ] || rollback_failed=1
  [ "$(sha "$target_host")" = "$old_host_hash" ] || rollback_failed=1
  if [ "$rollback_failed" -eq 0 ]; then
    activated=0
    rm -f "$backup_cli" "$backup_host"
    return 0
  fi
  return 1
}}
finish_activation() {{
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$activated" -eq 1 ] && ! rollback_pair; then
    echo "Activation rollback incomplete; recovery copies: $backup_cli, $backup_host" >&2
    status=1
  fi
  if [ "$repaired_modes" -eq 1 ]; then
    chmod "$old_cli_mode" "$target_cli" || status=1
    chmod "$old_host_mode" "$target_host" || status=1
  fi
  [ -z "$restore_cli" ] || rm -f "$restore_cli"
  [ -z "$restore_host" ] || rm -f "$restore_host"
  [ -z "$stage_cli" ] || rm -f "$stage_cli"
  [ -z "$stage_host" ] || rm -f "$stage_host"
  exit "$status"
}}
trap finish_activation EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
git_clean_at "$canonical" {release.fork_commit} || {{ echo 'Canonical source is not clean at the manifest commit' >&2; exit 1; }}
git_clean_at "$build" {release.fork_commit} || {{ echo 'Build worktree is not clean at the manifest commit' >&2; exit 1; }}
git_clean_at "$server" {release.server_commit} || {{ echo 'MCP server is not clean at the compatible manifest commit' >&2; exit 1; }}
[ -x "$source_cli" ] && [ -x "$source_host" ] && [ -f "$target_cli" ] && [ -f "$target_host" ] || {{ echo 'Activation binary pair is incomplete' >&2; exit 1; }}
source_cli_hash=$(sha "$source_cli"); source_host_hash=$(sha "$source_host")
[ -f "$receipt" ] || {{ echo 'Missing reviewed build receipt' >&2; exit 1; }}
receipt_format=$(receipt_value format "$receipt") || {{ echo 'Invalid build receipt' >&2; exit 1; }}
receipt_commit=$(receipt_value forkCommit "$receipt") || {{ echo 'Invalid build receipt' >&2; exit 1; }}
receipt_version=$(receipt_value cliVersion "$receipt") || {{ echo 'Invalid build receipt' >&2; exit 1; }}
receipt_cli_hash=$(receipt_value cliSha256 "$receipt") || {{ echo 'Invalid build receipt' >&2; exit 1; }}
receipt_host_hash=$(receipt_value hostSha256 "$receipt") || {{ echo 'Invalid build receipt' >&2; exit 1; }}
[ "$receipt_format" = v1 ] && [ "$receipt_commit" = {release.fork_commit} ] && [ "$receipt_version" = {shlex.quote(release.cli_version)} ] && [ "$receipt_cli_hash" = "$source_cli_hash" ] && [ "$receipt_host_hash" = "$source_host_hash" ] || {{ echo 'Build receipt does not match the reviewed binary pair' >&2; exit 1; }}
[ ! -e "$backup_cli" ] && [ ! -e "$backup_host" ] || {{ [ -e "$backup_cli" ] && [ -e "$backup_host" ] || {{ echo 'Release rollback pair is incomplete' >&2; exit 1; }}; }}
target_cli_hash=$(sha "$target_cli"); target_host_hash=$(sha "$target_host")
if [ "$target_cli_hash" = "$source_cli_hash" ] && [ "$target_host_hash" = "$source_host_hash" ] && {{ [ ! -x "$target_cli" ] || [ ! -x "$target_host" ]; }}; then
  old_cli_mode=$(file_mode "$target_cli"); old_host_mode=$(file_mode "$target_host")
  repaired_modes=1
  chmod 0755 "$target_cli" "$target_host"
fi
if [ "$target_cli_hash" != "$source_cli_hash" ] || [ "$target_host_hash" != "$source_host_hash" ]; then
  if [ -e "$backup_cli" ] || [ -e "$backup_host" ]; then
    [ -e "$backup_cli" ] && [ -e "$backup_host" ] || {{ echo 'Release rollback pair is incomplete' >&2; exit 1; }}
    cmp -s "$target_cli" "$backup_cli" && cmp -s "$target_host" "$backup_host" || {{ echo 'Release rollback pair does not match the active binaries' >&2; exit 1; }}
    rm -f "$backup_cli" "$backup_host"
  fi
  old_cli_hash=$(sha "$target_cli"); old_host_hash=$(sha "$target_host")
  stage_cli=$(mktemp "$target_cli.{release.name}.XXXXXX"); stage_host=$(mktemp "$target_host.{release.name}.XXXXXX")
  install -m 0755 "$source_cli" "$stage_cli"; install -m 0755 "$source_host" "$stage_host"
  [ "$(sha "$stage_cli")" = "$source_cli_hash" ] && [ "$(sha "$stage_host")" = "$source_host_hash" ] || {{ echo 'Staged binary hash verification failed' >&2; exit 1; }}
  if ! cp -p "$target_cli" "$backup_cli" || ! cp -p "$target_host" "$backup_host"; then
    rm -f "$backup_cli" "$backup_host"
    echo 'Failed to create the release rollback pair' >&2
    exit 1
  fi
  [ "$(sha "$backup_cli")" = "$old_cli_hash" ] && [ "$(sha "$backup_host")" = "$old_host_hash" ] || {{ echo 'Release rollback pair hash verification failed' >&2; exit 1; }}
  activated=1
  mv -f "$stage_cli" "$target_cli"
  stage_cli=
  mv -f "$stage_host" "$target_host"
  stage_host=
fi
if [ "$(sha "$target_cli")" != "$source_cli_hash" ] || [ "$(sha "$target_host")" != "$source_host_hash" ]; then
  echo 'Activated binary hash verification failed' >&2
  exit 1
fi
if ! version=$("$target_cli" --version) || [ "$version" != {shlex.quote(release.cli_version)} ]; then
  echo "Unexpected fork version: ${{version:-missing}}" >&2
  exit 1
fi
if ! "$target_cli" agents --help >/dev/null; then
  echo 'Activated fork does not support codex agents' >&2
  exit 1
fi
if ! "$target_host" --help >/dev/null; then
  echo 'Activated code-mode host is not executable' >&2
  exit 1
fi
activated=0
repaired_modes=0
trap - EXIT HUP INT TERM
printf 'activated-version=%s\\n' "$version"
"""


def print_status(statuses: list[Status], release: Release) -> bool:
    print("host\tcli\tserver\tsource\tresult")
    clean = True
    for status in statuses:
        drift = status.drift(release)
        clean &= not drift
        values = status.values
        print(
            "\t".join(
                [
                    status.host.name,
                    values.get("cliVersion", "unavailable"),
                    values.get("serverCommit", "unavailable"),
                    values.get("sourceCommit", "unavailable"),
                    "ok" if not drift else ",".join(drift),
                ]
            )
        )
    return clean


def apply_stage(host: Host, release: Release, bundle: Path | None) -> None:
    require_server(host, release)
    if bundle:
        copy_bundle(host, release, bundle)
    result = remote(host, build_script(host, release, bundle is not None))
    if result.returncode:
        raise FleetError(f"{host.name}: {error_message('building fork', result)}")
    print(f"{host.name}: {result.stdout.strip()}")


def apply_rollout(host: Host, release: Release, bundle: Path | None) -> None:
    apply_stage(host, release, bundle)
    result = remote(host, activation_script(host, release))
    if result.returncode:
        raise FleetError(f"{host.name}: {error_message('activating fork', result)}")
    print(f"{host.name}: {result.stdout.strip()}")


def args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("status", "plan"):
        child = commands.add_parser(command)
        child.add_argument("--host", action="append")
    for command in ("activate", "stage", "rollout"):
        child = commands.add_parser(command)
        child.add_argument("--host", action="append", required=True)
        child.add_argument("--apply", action="store_true")
        if command in {"stage", "rollout"}:
            child.add_argument("--bundle", type=Path)
    return parser.parse_args()


def main() -> int:
    parsed = args()
    try:
        release, hosts = load_manifest(parsed.manifest)
        hosts = selected_hosts(hosts, parsed.host)
        if parsed.command == "plan":
            for host in hosts:
                print(
                    f"{host.name}: gate server → stage {release.fork_commit} → build {host.build_root} → atomic activate → status"
                )
            return 0
        if parsed.command == "status":
            return (
                0 if print_status([host_status(host) for host in hosts], release) else 2
            )
        if not parsed.apply:
            raise FleetError(
                f"{parsed.command} changes remote state; re-run with --apply"
            )
        if parsed.command in {"stage", "rollout"}:
            bundle = parsed.bundle
            if bundle and not bundle.is_file():
                raise FleetError(f"release bundle does not exist: {bundle}")
            for host in hosts:
                if parsed.command == "stage":
                    apply_stage(host, release, bundle)
                else:
                    apply_rollout(host, release, bundle)
            if parsed.command == "stage":
                return 0
        else:
            for host in hosts:
                result = remote(host, activation_script(host, release))
                if result.returncode:
                    raise FleetError(
                        f"{host.name}: {error_message('activating fork', result)}"
                    )
                print(f"{host.name}: {result.stdout.strip()}")
        statuses = [host_status(host) for host in hosts]
        return 0 if print_status(statuses, release) else 2
    except FleetError as error:
        print(f"fleet-release: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
