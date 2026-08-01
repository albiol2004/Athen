#!/usr/bin/env python3
"""Fail CI when frontend code reintroduces CSP-hostile patterns.

Athen's Tauri WebView uses a CSP with `script-src 'self'`. Inline event
handlers and `javascript:` URLs are incompatible with that policy and can
silently break UI controls. They also make it harder to reason about injected
HTML rendered inside the app shell.

This checker intentionally focuses on high-signal patterns instead of trying
to be a full JavaScript/HTML linter.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / "frontend"

TEXT_EXTENSIONS = {".html", ".js", ".css"}

CHECKS: tuple[tuple[str, re.Pattern[str], str], ...] = (
    (
        "inline event handler",
        re.compile(r"\son[a-zA-Z]+\s*=", re.IGNORECASE),
        "Move event wiring to addEventListener or delegated listeners.",
    ),
    (
        "javascript: URL",
        re.compile(r"javascript\s*:", re.IGNORECASE),
        "Use a real event listener instead of javascript: URLs.",
    ),
    (
        "eval call",
        re.compile(r"\beval\s*\(", re.IGNORECASE),
        "Avoid eval; use explicit parsing or dispatch tables.",
    ),
    (
        "Function constructor",
        re.compile(r"\bnew\s+Function\s*\(", re.IGNORECASE),
        "Avoid dynamic code generation in the WebView.",
    ),
    (
        "string timer callback",
        re.compile(r"\bset(?:Timeout|Interval)\s*\(\s*['\"]", re.IGNORECASE),
        "Pass a function to timers, not a string to evaluate.",
    ),
)

INLINE_SCRIPT_RE = re.compile(
    r"<script\b(?![^>]*\bsrc\s*=)[^>]*>(?P<body>.*?)</script\s*>",
    re.IGNORECASE | re.DOTALL,
)


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    kind: str
    snippet: str
    guidance: str


def iter_frontend_files() -> list[Path]:
    return sorted(
        path
        for path in FRONTEND.rglob("*")
        if path.is_file() and path.suffix.lower() in TEXT_EXTENSIONS
    )


def line_for_offset(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def scan_file(path: Path) -> list[Finding]:
    text = path.read_text(encoding="utf-8")
    findings: list[Finding] = []

    for kind, pattern, guidance in CHECKS:
        for match in pattern.finditer(text):
            findings.append(
                Finding(
                    path=path,
                    line=line_for_offset(text, match.start()),
                    kind=kind,
                    snippet=text[match.start() : match.end()].strip(),
                    guidance=guidance,
                )
            )

    if path.suffix.lower() == ".html":
        for match in INLINE_SCRIPT_RE.finditer(text):
            body = match.group("body").strip()
            if not body:
                continue
            findings.append(
                Finding(
                    path=path,
                    line=line_for_offset(text, match.start()),
                    kind="inline script block",
                    snippet="<script>…</script>",
                    guidance="Move inline scripts into frontend/app.js or another self-hosted JS file.",
                )
            )

    return findings


def main() -> int:
    if not FRONTEND.exists():
        print(f"frontend directory not found: {FRONTEND}", file=sys.stderr)
        return 2

    findings = [finding for path in iter_frontend_files() for finding in scan_file(path)]

    if not findings:
        print("No CSP-hostile frontend patterns found.")
        return 0

    print("CSP regression check failed. Found CSP-hostile frontend patterns:\n")
    for finding in findings:
        rel = finding.path.relative_to(ROOT)
        print(f"- {rel}:{finding.line}: {finding.kind}: {finding.snippet!r}")
        print(f"  {finding.guidance}")

    print(
        "\nAthen's Tauri CSP uses script-src 'self', so inline handlers, "
        "javascript: URLs, and dynamic code execution should stay out of "
        "the WebView surface."
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
