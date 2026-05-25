# How Athen Decides When to Ask Permission

Athen is designed to act autonomously — but not recklessly. Before doing anything, it runs every action through a risk system that decides whether to proceed silently, tell you what it did, or stop and ask for your approval. This guide explains how that works.

## The core idea

Not all actions are equally consequential. Reading your calendar is safe. Sending an email on your behalf is more significant. Deleting an important file or making a purchase is serious enough that Athen should never do it without your explicit OK.

The risk system encodes this by scoring every potential action on a scale. A low score means "just do it." A high score means "stop and ask first."

## The four decision levels

### 1. Silent Approve (score 0–19)

Athen acts immediately without telling you anything beyond the normal reply.

Examples: reading a file, searching the web, looking up your calendar, recalling something from memory.

These are read-only or otherwise safe actions. If something goes wrong it's easy to undo or has no lasting effect.

### 2. Notify and Proceed (score 20–49)

Athen takes the action, then sends you a notification explaining what it did.

Examples: creating a note, writing a temporary file, saving something to your task list.

These have some local effect but are generally reversible. You don't need to approve them in advance, but Athen keeps you informed so you're never surprised.

### 3. Human Confirm (score 50–89)

Athen pauses and asks for your approval before doing anything.

Examples: sending an email, posting a message on your behalf, modifying an important document, creating a calendar event on a shared calendar.

Athen will show you exactly what it plans to do. You can approve, reject, or edit the action before it happens.

### 4. Hard Block (score 90+)

Athen stops completely and requires your explicit approval. These actions are never taken automatically under any circumstances.

Examples: actions involving financial accounts, configuration changes with security implications, anything coming from an untrusted source that requests a sensitive operation.

Hard Block actions always surface to you — Athen will never silently cancel them either. If something is blocked, you'll see it and you decide what happens next.

## What goes into the score?

The score is calculated from three factors multiplied together:

### Base impact (what kind of action is it?)

| Action type | Impact value |
|---|---|
| Reading information | 1 |
| Writing a temporary file | 10 |
| Writing something permanent (database, important file) | 40 |
| System-level change (installing software, modifying config) | 90 |

### Contact trust (who does this action involve?)

Actions that affect or originate from other people are weighted by how much Athen trusts them:

| Who is involved | Risk multiplier |
|---|---|
| You (the Athen owner) | 0.5x — safest, your requests get lowest score |
| A contact you've explicitly trusted | 1.0x |
| A contact Athen knows with some history | 1.5x |
| A contact in your address book but no history | 2.0x |
| An unknown person (not in your contacts) | 5.0x — highest risk |

This matters most when Athen acts on incoming messages. An email from a stranger asking Athen to do something will always be scored much higher than the same request from you.

### Data sensitivity (what information is involved?)

| Type of data | Multiplier |
|---|---|
| Ordinary information | 1x |
| Personal information (names, addresses, health) | 2x |
| Secrets (passwords, API keys, financial data) | 5x |

### Uncertainty penalty

When Athen is not confident about what an action involves (for example, a vague or ambiguous request from an external source), it adds an extra penalty to the score. This nudges uncertain cases toward asking for confirmation rather than guessing.

## A worked example

Imagine Athen receives an email from someone not in your contacts, asking it to send a reply on your behalf.

- Base impact: sending an email = WritePersist = 40
- Trust: unknown sender = 5.0x multiplier
- Data: email content = PersonalInfo = 2x multiplier
- Score: 40 × 5.0 × 2 = 400

A score of 400 is a Hard Block. Athen will surface this to you for explicit approval before doing anything.

Now imagine you ask Athen the same thing directly in the chat:

- Base impact: 40
- Trust: you (AuthUser) = 0.5x
- Data: 2x
- Score: 40 × 0.5 × 2 = 40

A score of 40 is Notify and Proceed — Athen sends the email and tells you it did.

## How trust levels are set

Athen assigns trust levels automatically based on your contact list and interaction history. You can also set them manually:

1. Go to Settings → Contacts.
2. Find the contact you want to adjust.
3. Use the Trust Level dropdown to set it explicitly.

Contacts you interact with regularly and have never had problems with will gradually build up a history, but Athen will never automatically promote someone to "Trusted" without your action — that level requires a manual override.

## The system is conservative by default

The risk system is deliberately tuned to ask more often than it needs to rather than act without permission. Especially for new contacts, external sources, or actions involving important data, Athen will pause. This is intentional. As you use Athen and build up trusted contacts, you'll see fewer confirmation prompts for routine tasks.

If you find Athen is asking for approval too often for something you consider routine, the most effective fix is to add the relevant contact to your contacts list and set their trust level appropriately.
