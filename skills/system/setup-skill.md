# Creating and Using Skills

A Skill is a reusable playbook — a piece of written guidance the agent loads when a task matches. Think of it as a cheat sheet: "when writing a cold email, follow these rules"; "when formatting a release note, use this structure"; "when summarizing a meeting, produce this layout." The agent sees a list of available skill names at all times and decides when to pull one in. You write the guidance once; the agent consults it whenever it is relevant.

Skills are different from Memory (which stores facts the agent picks up automatically) and from Identity (which is always-on personality and rules). A Skill is on-demand procedural knowledge — it is only loaded into the conversation when the agent judges it fits the task at hand.

## Prerequisites

- Athen installed and running.
- A clear idea of the task type you want to guide — for example, a style guide for emails you send regularly, or a template for a recurring report.

## Steps

### Step 1 — Open the Skills panel

1. Click **Settings** (gear icon in the sidebar).
2. Navigate to **Agents & Tools** → **Skills**.

You will see a two-panel layout:
- **Left panel** — list of your skills, grouped into "User" (ones you wrote), "Imported" (installed from a zip or URL), and "Bundled" (shipped with Athen).
- **Right panel** — the editor for the selected skill.

### Step 2 — Create a new skill

1. Click **+ New skill** at the bottom of the left panel.
2. A blank editor opens on the right. Fill in:
   - **Slug** — a short, lowercase, hyphen-separated identifier like `cold-email-outreach` or `release-notes`. This is the folder name on disk and must be unique.
   - **Name** — a human-readable display name like "Cold Email Outreach".
   - **Description** — one sentence that tells the agent when to use this skill. Start with "Use when…". For example: `Use when drafting a cold outreach email — covers subject lines, opening hooks, call-to-action shapes, and anti-spam rules.` The agent reads this description every turn to decide whether to load the skill.
   - **Applies to** — which agent profiles can see this skill. Options are:
     - `all` — every profile sees it (the default).
     - One or more profile IDs like `outreach`, `personal_assistant`, or `coder`. Separate multiple IDs with commas.
     - Prefix a profile ID with `!` to exclude it (for example, `!coder` means "all profiles except Coder").
   - **Body** — the full playbook, written in plain markdown. This is what the agent actually reads when it loads the skill. Write it as if explaining the process to a knowledgeable colleague: structure, rules, examples, and anything the agent would otherwise have to guess.

3. Click **Save**. Athen writes a `SKILL.md` file to disk and indexes it immediately.

### Step 3 — How the agent discovers and uses your skill

After saving, the skill's name and description appear in the agent's system prompt every turn (for profiles that match `applies_to`). When the agent encounters a task that fits, it calls `load_skill` with the slug, and the full body lands in the conversation as context. You do not need to tell the agent to use the skill — if the description is accurate, it will pick it up on its own.

You can confirm a skill was loaded by looking at the conversation: the agent's tool call card will show `load_skill("your-slug")` followed by the body in the result.

### Step 4 — Edit or delete a skill

Click any skill in the left panel to open its editor. Change any field and click **Save** to update. Click **Delete** to remove the skill and its file from disk.

If you edit a Bundled skill (one that ships with Athen), Athen creates a "User" copy with the same slug. Your version takes precedence. A **Reset to bundled** button appears to restore the original.

### Step 5 — Import a skill someone else wrote

Click **Import from zip/URL** in the left panel. Paste a URL pointing to a skill folder zip file, or browse to a local zip. Athen extracts it, validates the `SKILL.md`, and adds it to your list under the "Imported" group.

## Examples of useful skills

- **Cold email playbook** — subject-line patterns, opening hooks, call-to-action shapes, length limits, what to avoid.
- **Release note formatter** — how to turn a list of commits into human-readable prose, which sections to include (breaking changes, bug fixes, improvements), your team's tone.
- **Meeting summary structure** — TL;DR → decisions → action items → open questions, with guidance on how brief each section should be.
- **Code review checklist** — what to look for in a PR: security, performance, naming, test coverage, documentation.
- **Expense report template** — column names, category codes, how to handle foreign currencies, where to attach receipts.

## What to write in the body

A good skill body is concrete, not abstract:
- Use headings to separate sections.
- Give examples — before/after pairs, filled-in templates, real subject lines.
- State rules as short imperatives: "Keep subject lines under 40 characters."
- If the skill references a template file you saved alongside it, mention the file name. The agent can read it separately.

Avoid vague advice like "be professional." The agent already tries to be professional. Write the things it cannot know without you: your specific style, your team's terminology, the quirks of your audience.

## Common issues

**The agent is not loading the skill even though the task seems to match.**
Check the description. The agent reads it and decides whether it is relevant. If the description is too narrow or phrased unclearly, rewrite it to match the words a user would actually say. For example, instead of "Use for outreach", try "Use when drafting a cold email to someone who has not heard of your product."

**The skill does not appear in the list after saving.**
If you edited the `SKILL.md` file directly on disk (outside of Athen), click **Rescan** in the panel footer to force Athen to re-index the skills directory.

**The skill shows as "Bundled" and I cannot edit it.**
Click on it and then click **Edit** — Athen creates a User-owned copy. Your copy takes precedence over the bundled original and can be edited freely.

**I want different skills for different agent profiles.**
Set `applies_to` to the specific profile IDs. A skill with `applies_to: [outreach]` does not appear in the Coder profile's listing, so it does not consume any prompt budget there.

**The body is very long — will it slow the agent down?**
Skill bodies are only loaded when the agent calls `load_skill`. The listing (name + description) is always present but is short (about 30 tokens per skill). Load the body only when it is genuinely useful, and keep it as concise as the guidance allows.
