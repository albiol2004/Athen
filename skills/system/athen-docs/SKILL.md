---
name: athen-docs
description: Use when the user needs help setting up, configuring, or troubleshooting Athen.
applies_to: all
---

# Athen Self-Help Guides

You have access to Athen's built-in documentation via the `athen_docs` tool.

## How to use

1. Call `athen_docs` with action `list` to see all available guides with descriptions.
2. Call `athen_docs` with action `get` and `topic` set to a guide slug to read the full walkthrough.

## Available categories

### Setup guides
- **setup-calendar-source** -- Connect iCloud, Google, Fastmail, or other calendar via CalDAV
- **setup-email** -- Connect email with IMAP/SMTP autodetect and app-specific passwords
- **setup-mcp-server** -- Add external tool servers (Slack, Notion, GitHub, etc.)
- **setup-cloud-api-endpoint** -- Register HTTP APIs (weather, translation, search, etc.)
- **setup-github-identity** -- Connect GitHub for commits, PRs, and code pushes
- **setup-skill** -- Create reusable playbooks for recurring tasks
- **setup-wakeup** -- Set up scheduled and recurring tasks

### Concept guides
- **understand-risk-system** -- How Athen decides when to act vs. ask permission
- **understand-profiles** -- Create specialized agent personalities for different tasks
- **pick-local-model** -- Hardware requirements and recommendations for running models locally

### Troubleshooting
- **troubleshoot-no-llm-response** -- Diagnostic checklist when Athen isn't responding
