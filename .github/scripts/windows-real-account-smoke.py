"""Run one isolated Windows account request without copying or printing credentials.

The existing auth file remains the credential authority so normal OAuth refresh
and its sibling lock preserve rotated tokens. All other state is temporary.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import uuid


def verify_package(package, version, commit):
    actual_files = set()
    pending = [package]
    while pending:
        item = pending.pop()
        metadata = item.lstat()
        if metadata.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
            raise ValueError("package contains a reparse point")
        if stat.S_ISDIR(metadata.st_mode):
            pending.extend(item.iterdir())
        elif stat.S_ISREG(metadata.st_mode):
            actual_files.add(item.relative_to(package).as_posix().casefold())
        else:
            raise ValueError("package contains a non-regular entry")
    manifest = package / "SHA256SUMS.txt"
    if not manifest.is_file() or manifest.stat().st_size > 1024 * 1024:
        raise ValueError("invalid package manifest file")
    files = set()
    for line in manifest.read_text(encoding="utf-8-sig").splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64})  (.+)", line)
        if not match:
            raise ValueError("invalid package manifest")
        expected, relative = match.groups()
        candidate = package / relative
        if candidate.resolve().parent == package.parent or not candidate.resolve().is_relative_to(package):
            raise ValueError("manifest path escapes package")
        if relative.casefold() in files:
            raise ValueError("duplicate manifest path")
        files.add(relative.casefold())
        for item in [candidate, *candidate.parents]:
            if item == package.parent:
                break
            if item.lstat().st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
                raise ValueError("package contains a reparse point")
        with candidate.open("rb") as stream:
            actual = hashlib.file_digest(stream, "sha256").hexdigest()
        if actual != expected.lower():
            raise ValueError("package hash mismatch")
    if actual_files != files | {"sha256sums.txt"}:
        raise ValueError("unexpected package file set")
    metadata = (package / "BUILD-INFO.txt").read_text(encoding="utf-8-sig")
    for label, expected in [("Version", version), ("Commit", commit)]:
        if re.findall(rf"^{label}:\s*(\S+)\s*$", metadata, re.MULTILINE) != [expected]:
            raise ValueError("package build identity mismatch")
    return package / "grok-zh.exe"


def run(args):
    if os.name != "nt":
        raise ValueError("this smoke test requires Windows")
    package = args.package_dir.resolve(strict=True)
    auth_file = args.auth_file.resolve(strict=True)
    report_file = args.report_file.resolve()
    for protected in [auth_file, auth_file.with_name(auth_file.name + ".lock")]:
        if report_file == protected or (report_file.exists() and protected.exists()
                                       and report_file.samefile(protected)):
            raise ValueError("report path overlaps the credential file or lock")
    if report_file.is_relative_to(package):
        raise ValueError("report path must be outside the package")
    executable = verify_package(package, args.expected_version, args.expected_commit)
    report = {"version": args.expected_version, "commit": args.expected_commit,
              "platform": "x86_64-pc-windows-gnu", "passed": False}
    parent = Path(tempfile.gettempdir()).resolve()
    with tempfile.TemporaryDirectory(prefix="grok-zh-account-smoke-", dir=parent) as directory:
        root = Path(directory).resolve()
        if root.parent != parent or not root.name.startswith("grok-zh-account-smoke-"):
            raise ValueError("temporary directory boundary mismatch")
        home, workspace, temp = root / "home", root / "workspace", root / "tmp"
        for path in [home, workspace, temp, home / "appdata", home / "localappdata"]:
            path.mkdir()
        (home / "config.toml").write_text("", encoding="utf-8")
        system_root = os.environ["SystemRoot"]
        environment = {key: os.environ[key] for key in
                       ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT", "NUMBER_OF_PROCESSORS"]
                       if key in os.environ}
        environment.update({
            "PATH": str(Path(system_root) / "System32") + ";" + system_root,
            "HOME": str(home), "USERPROFILE": str(home), "GROK_HOME": str(home),
            "APPDATA": str(home / "appdata"), "LOCALAPPDATA": str(home / "localappdata"),
            "TEMP": str(temp), "TMP": str(temp), "TMPDIR": str(temp), "CI": "true",
            "GROK_AUTH_PATH": str(auth_file), "GROK_DISABLE_AUTOUPDATER": "1",
            "GROK_MANAGED_CONFIG": "0", "GROK_TELEMETRY_ENABLED": "0",
            "GROK_TELEMETRY_TRACE_UPLOAD": "0", "GROK_EXTERNAL_OTEL": "0",
            "GROK_CRASH_HANDLER": "0", "DISABLE_TELEMETRY": "1",
            "DISABLE_ERROR_REPORTING": "1", "RUST_LOG": "off", "RUST_BACKTRACE": "0",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": str(home / "empty-gitconfig"),
        })
        git = shutil.which("git")
        if not git:
            raise ValueError("git is required for the isolated test workspace")
        environment["PATH"] += ";" + str(Path(git).parent)
        environment["GIT_BIN_PATH"] = git
        (home / "empty-gitconfig").write_text("", encoding="utf-8")
        kwargs = dict(env=environment, cwd=workspace, stdin=subprocess.DEVNULL,
                      capture_output=True, creationflags=subprocess.CREATE_NO_WINDOW)
        initialized = subprocess.run([git, "-c", "init.templateDir=", "init", "--quiet", str(workspace)],
                                     timeout=20, **kwargs)
        if initialized.returncode:
            raise ValueError("could not initialize isolated workspace")
        identity = subprocess.run([str(executable), "--version"], timeout=20, **kwargs)
        version_line = identity.stdout.decode("utf-8", errors="replace").strip()
        if identity.returncode or not re.fullmatch(
                rf"grok-zh {re.escape(args.expected_version)} \({args.expected_commit[:12]}\)", version_line):
            raise ValueError("executable identity mismatch")
        marker = "中文冒烟通过：" + uuid.uuid4().hex
        command = [str(executable), "--single", "请只回复下面这一行，不要添加解释或标点：" + marker,
                   "--output-format", "json", "--locale", "zh-CN", "--no-auto-update",
                   "--no-wait-for-background", "--no-subagents", "--no-memory",
                   "--disable-web-search", "--max-turns", "1", "--tools", "todo_write",
                   "--disallowed-tools", "Agent,search_tool,use_tool", "--cwd", str(workspace)]
        try:
            result = subprocess.run(command, timeout=300, **kwargs)
            report["exit_code"] = result.returncode
            report["stdout_bytes"] = len(result.stdout)
            report["stderr_bytes"] = len(result.stderr)
            try:
                response = json.loads(result.stdout)
            except (ValueError, UnicodeError):
                response = {}
            if not isinstance(response, dict):
                response = {}
            report.update({
                "completed_turn": response.get("stopReason") == "end_turn",
                "chinese_marker_matched": marker in str(response.get("text", "")),
                "session_id_present": bool(response.get("sessionId")),
                "request_id_present": bool(response.get("requestId")),
            })
            report["passed"] = result.returncode == 0 and all(report[key] for key in
                ["completed_turn", "chinese_marker_matched", "session_id_present", "request_id_present"])
            if not report["passed"]:
                diagnostics = (result.stdout + result.stderr).decode("utf-8", errors="replace").lower()
                report["authentication_error_hint"] = any(
                    word in diagnostics for word in ["invalid_grant", "unauthorized", "authentication failed", "401"])
                report["rate_limit_hint"] = any(word in diagnostics for word in ["rate limit", "429", "usage limit"])
        except subprocess.TimeoutExpired:
            report["timed_out"] = True
    report_file.parent.mkdir(parents=True, exist_ok=True)
    report_file.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", required=True, type=Path)
    parser.add_argument("--expected-version", required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--auth-file", required=True, type=Path)
    parser.add_argument("--report-file", required=True, type=Path)
    raise SystemExit(run(parser.parse_args()))
