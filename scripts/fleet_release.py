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
    def runtime_version(self) -> str:
        prefix = "codex-cli "
        if not self.cli_version.startswith(prefix):
            raise FleetError("expectedCliVersion must start with 'codex-cli '")
        return self.cli_version.removeprefix(prefix)

    @property
    def upstream_version(self) -> str:
        return self.runtime_version.partition("+")[0]


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
  if ($LASTEXITCODE -ne 0) {{ throw "git failed in ${{Path}}: $($Arguments -join ' ')" }}
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
