# Setting Up Scheduled Tasks (Wake-ups)

Wake-ups are tasks Athen runs automatically on a schedule — without you having to ask. Use them for reminders ("ping me at 5 pm to review the PR"), recurring digests ("every morning, brief me on my emails"), or periodic jobs ("every Sunday, tally last week's expenses"). The agent handles these the same way it handles anything you ask directly: it reads, reasons, and acts — just triggered by the clock instead of you.

The agent can also create wake-ups on its own during a conversation. If you tell it "follow up with Joan on Friday if she hasn't replied," it will schedule that automatically.

## Prerequisites

- Athen installed and running.
- A clear idea of what you want Athen to do and when.
- For tasks that send emails or Telegram messages: the relevant connections configured in Settings → Connections.

## Steps

### Step 1 — Open the Scheduled panel

Click the **clock icon** in the sidebar (labeled "Scheduled"). This opens the wake-ups view, which lists all your scheduled tasks with their next fire time, schedule, and status.

If there are no wake-ups yet, the list will be empty with a prompt to create one.

### Step 2 — Create a new wake-up

Click **+ New** to open the creation form. Fill in the following fields:

**Instruction** — describe the task in plain language, as if asking Athen in chat. Be specific about what to do and where to put the result. Examples:
- `Summarize any unread emails I received today and post the summary to Telegram.`
- `Check if the deployment pipeline on GitHub Actions finished without errors and notify me in-app.`
- `Remind me to take a break — just say something brief in-app.`

**Schedule type** — choose one of three options:

- **One-time** — fires once at a specific date and time. A calendar picker lets you choose the day; separate hour and minute fields set the time. Use this for reminders ("ping me tomorrow at 3 pm").

- **Recurring (cron)** — fires on a repeating pattern using a standard cron expression. Common patterns:
  - `0 8 * * *` — every day at 8:00 am
  - `0 8 * * 1` — every Monday at 8:00 am
  - `0 9 * * 1-5` — weekdays at 9:00 am
  - `0 8 * * 0` — every Sunday at 8:00 am
  Enter the expression in the **Cron expression** field and set your **Timezone** (for example, `Europe/Madrid` or `America/New_York`).

- **Interval** — fires every N seconds repeatedly (useful for frequent checks). Enter the number of seconds in the field.

**Autonomy** — how much Athen is allowed to do on its own when the task fires:

- **Auto** — Athen acts freely on everything except actions flagged as critical risks. Use this for tasks you fully trust, such as a simple read-and-summarize.
- **Safe only** — Athen acts on low-risk steps automatically but pauses and asks you before anything above your configured risk threshold. This is the default for tasks the agent creates itself.
- **Notify only** — Athen reads and researches but never sends anything or modifies anything. The result is shown to you for review. Use this when you want the output but do not want Athen acting outbound.

**Agent profile** — which agent profile runs the task. Leave this as the default unless you have a specialized profile (for example, "Coder" for code-related checks or "Outreach" for email tasks).

**Tool allowlist** (optional, advanced) — limit which tools the agent can use when this task fires. Expand the list and check only the tools you want available. For a read-only digest, check only search and read tools. This prevents the agent from acting outside the intended scope even if the instructions somehow end up ambiguous.

**Contact allowlist** (optional, advanced) — if the task involves sending messages, limit which contacts the agent can reach. Only addresses and chat IDs on this list will be reachable. Leave empty to use the profile's normal defaults.

Click **Save** to create the wake-up. It will appear in the list with its next fire time shown.

### Step 3 — Monitor and manage wake-ups

The Scheduled panel shows for each wake-up:
- The instruction (truncated).
- The next scheduled fire time.
- The schedule (cron expression, interval, or "once at…").
- Who created it: **User** (you) or **Agent** (with a link to the conversation where it was scheduled).
- The autonomy band.
- An enabled/disabled toggle.
- The result of the last fire.

Click any wake-up row to expand it and edit the instruction, schedule, or autonomy setting. Click the toggle to pause it temporarily. Click **Delete** to remove it permanently.

### Step 4 — Ask the agent to create a wake-up for you

You do not have to use the form. During any conversation, you can say things like:
- "Remind me in 2 hours to check if the build finished."
- "Every weekday morning, summarize my unread emails."
- "If I haven't replied to that contractor by Friday, remind me."

The agent will call `create_wakeup` and show you the schedule it set, including a link to the wake-up in the Scheduled panel. For anything beyond a simple reminder, you can review and adjust autonomy and allowlists in the panel.

## Examples of useful wake-ups

- **Morning email digest** — `0 8 * * 1-5` cron, instruction: `Summarize unread emails from the last 24 hours. Group by sender. Note anything that needs a reply today. Post the summary in-app.` Autonomy: Notify only.
- **Weekly expense tally** — `0 9 * * 0` cron (Sunday), instruction: `Find email receipts from the past 7 days. List merchant, amount, and currency. Write the list to ~/Documents/expenses-weekly.md and notify me in-app when done.` Autonomy: Safe only.
- **PR follow-up reminder** — One-time, instruction: `Check if the pull request I was reviewing earlier today has any new review comments. Summarize what changed.` Autonomy: Notify only.
- **Deployment health check** — interval every 3600 seconds, instruction: `Check the GitHub Actions run status for the main branch. If the latest run failed, notify me in-app with the failing step.` Autonomy: Auto.

## Common issues

**The wake-up fired but Athen did nothing and I got no notification.**
Check the autonomy band. If it is set to "Notify only", Athen will not send anything — it produces output for your in-app review. Open the Scheduled panel and look at the "last fire result" for that wake-up.

**The agent created a wake-up I did not expect.**
Any agent-authored wake-up goes through the risk gate at creation time. If you approved it in the conversation, it was created. Open the Scheduled panel, find the entry with origin "Agent", and disable or delete it if you no longer want it.

**The wake-up fires but the action is blocked and nothing happens.**
The per-action risk gate fired above the autonomy band's threshold. The pending action is sitting in the approval queue — open the in-app notifications or check the **Notifications** panel. Approve or deny it there. If this happens repeatedly, either raise the autonomy band to "Auto" or narrow the instruction so the agent does not attempt risky actions.

**I set a cron schedule but the time is off by several hours.**
The timezone field was not set or defaulted to UTC. Edit the wake-up and enter your local timezone (for example, `America/New_York` or `Europe/Paris`). Timezone names follow the IANA format — search "IANA timezone list" for the exact name for your location.

**Athen was closed when the wake-up should have fired.**
When Athen starts up again, it checks for missed fires. A one-time wake-up that missed its time will run once on the next startup. A recurring wake-up that missed several fires will run one catch-up pass covering the missed window, then resume its normal schedule. You will see a note in the result explaining that it is a catch-up run.
