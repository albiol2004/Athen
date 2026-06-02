#!/usr/bin/env python3
"""Audit likely user-facing Rust error surfaces.

Athen exposes errors through several external surfaces: the desktop UI,
Telegram replies, notifications, and emitted frontend events. Raw `{e}` /
`to_string()` formatting is sometimes fine for internal logs, but user-facing
paths should prefer `AthenError::user_safe_message()` where the value may come
from providers, tools, shells, MCP servers, or third-party APIs.

This script is intentionally advisory. It prints candidates for review instead
of failing CI, because not every raw error formatting site is user-facing and
not every error type is `AthenError`.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"

RUST_EXTENSIONS = {".rs"}

SURFACE_HINTS = (
    "send_telegram_reply",
    "emit(",
    "emit_all",
    "Notification",
    "toast",
    "frontend",
    "user-facing",
    "user facing",
    "format_user_error",
    "simplify_error",
    "to_frontend",
    "reply",
)

RAW_ERROR_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("format interpolation of error", re.compile(r'format!\([^\n;]*\{e(?::[^}]*)?\}')),
    ("direct error string conversion", re.compile(r'\b[a-zA-Z_][a-zA-Z0-9_]*\.to_string\(\)')),
    ("error passed through anyhow/debug string", re.compile(r'format!\([^\n;]*(error|err|failed)', re.IGNORECASE)),
)

LOG_ONLY_HINTS = (
    "tracing::",
    "log::",
    "debug!",
    "warn!",
    "error!",
    "info!",
)


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    kind: str
    text: str
    context: str


def iter_rust_files() -> list[Path]:
    return sorted(
        path
        for path in CRATES.rglob("*.rs")
        if path.is_file() and path.suffix.lower() in RUST_EXTENSIONS
    )


def has_surface_hint(window: str) -> bool:
    lowered = window.lower()
    return any(hint.lower() in lowered for hint in SURFACE_HINTS)


def is_log_only(line: str) -> bool:
    return any(hint in line for hint in LOG_ONLY_HINTS)


def scan_file(path: Path) -> list[Finding]:
    lines = path.read_text(encoding="utf-8").splitlines()
    findings: list[Finding] = []

    for idx, line in enumerate(lines):
        if is_log_only(line):
            continue

        start = max(0, idx - 4)
        end = min(len(lines), idx + 5)
        window = "\n".join(lines[start:end])

        if not has_surface_hint(window):
            continue

        for kind, pattern in RAW_ERROR_PATTERNS:
            if pattern.search(line):
                findings.append(
                    Finding(
                        path=path,
                        line=idx + 1,
                        kind=kind,
                        text=line.strip(),
                        context=" | ".join(s.strip() for s in lines[start:end] if s.strip()),
                    )
                )

    return findings


def main() -> int:
    if not CRATES.exists():
        print(f"crates directory not found: {CRATES}", file=sys.stderr)
        return 2

    findings = [finding for path in iter_rust_files() for finding in scan_file(path)]

    if not findings:
        print("No likely user-facing raw error surfaces found.")
        return 0

    print("Likely user-facing raw error surfaces found for review:\n")
    for finding in findings:
        rel = finding.path.relative_to(ROOT)
        print(f"- {rel}:{finding.line}: {finding.kind}")
        print(f"  {finding.text}")
        print("  Consider using `AthenError::user_safe_message()` or redacting before display.")

    print(
        "\nThis audit is advisory and exits successfully. Review these call sites "
        "before making the check blocking."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
