# Understanding Agent Profiles

A profile is a personality for Athen. It controls how Athen presents itself, what it focuses on, which tools it highlights, and which tasks get routed to it. You can have multiple profiles for different purposes — one for coding, one for managing email, one for outreach — and switch between them per conversation.

## The default profile

When you first install Athen, it comes with a default profile. This profile has a general-purpose persona, access to all tools, and handles any kind of task. For most users, the default profile is all you need.

## Why create additional profiles?

Profiles let you tailor Athen's behavior for specific kinds of work without changing the default. Examples:

- **Coder**: Knows to use a code-specialist AI model, prioritizes file editing and shell tools, commits changes using your personal GitHub identity.
- **Outreach**: Focused on drafting and sending emails to potential contacts, highlights email and contacts tools, avoids getting pulled into coding tasks.
- **Personal assistant**: Focused on scheduling, reminders, and calendar management, highlights calendar and wakeup tools.
- **Research**: Focuses on web search, summarization, and document reading.

## What you can set on a profile

### Display name and description

The name shown in the profile selector and a short description of what this profile is for. The coordinator also uses the description when deciding which profile to route a task to.

### Persona (system prompt)

The instructions that tell Athen how to behave and what its job is. This is split into composable fragments:

- **Voice**: Tone, formality, and how the agent communicates (e.g. concise and direct, warm and detailed).
- **Mission**: What this profile is here to do — its goals and focus areas.
- **Constraints**: Hard limits — things it should never do, topics it should escalate.
- **Output style**: How long responses should be, whether to use bullet points, citation style, etc.

You can combine built-in fragments or write your own free-form addendum. If a profile has no custom persona, Athen uses its default "I am Athen, a universal AI agent" behavior.

### Primary tool groups

All tools are always available to every profile. This setting controls which tools are shown prominently in the system prompt (and therefore which ones Athen is most likely to reach for first).

Narrowing primary groups does not remove tools — it just moves less-relevant ones to a lower tier. Athen can still use any tool if the task calls for it.

Example: a profile focused on email could set primary groups to `email` and `contacts`. Calendar, shell, and web tools remain fully available but are not highlighted in the prompt.

### Expertise declaration

The domains and task types this profile is good at. The coordinator uses these when deciding which profile to route an incoming task to.

- **Domains**: Email, Calendar, Coding, Research, Outreach, Scheduling, Writing, Finance, etc.
- **Task kinds**: Drafting, Editing, Summarizing, Researching, Coding, Debugging, etc.
- **Avoid**: Task kinds that should not be routed to this profile even if no better option exists.

### GitHub identity

Which GitHub account Athen should use when running git or GitHub commands under this profile. Options:

- **None**: No credentials injected. Git commands run unauthenticated (or with whatever the system already has configured).
- **Bot**: A dedicated GitHub account set up for Athen — useful so the agent has its own commit identity and access, separate from your personal account.
- **User**: Your own GitHub credentials — agent acts as you. Use this when Athen needs access to repos the bot account does not have, or when you want commits to appear under your name.

You configure the Bot and User credentials once in Settings → Connections → GitHub. The profile just picks which one to use.

### Model profile hint

An optional hint for which AI model tier to use for this profile. Options include Cheap (fastest, lowest cost), Fast (balanced), Powerful (most capable), and Code (specialized for coding tasks). Leave this empty to let Athen choose automatically based on task complexity.

## How the coordinator picks a profile

When Athen receives a task (from you directly, from an email, from a calendar event, etc.), the coordinator classifies the task into a domain and task kind, then scores it against all active profiles. The profile with the best match gets the task.

Matching works by:

1. Comparing the task's domain and task kind against each profile's expertise declaration.
2. Checking the avoid list to penalize profiles that explicitly don't want this kind of task.
3. Using the profile's description for fuzzy matching on cases the closed categories don't cover.

The default profile is the fallback — it always accepts tasks that no specialized profile claims.

## Creating and editing profiles

1. Go to Settings → Profiles.
2. Click "+ New Profile" to create one from scratch, or click the duplicate icon on an existing profile to start from a copy.
3. Fill in the name, description, and persona fragments.
4. Set the expertise domains and task kinds that match your use case.
5. (Optional) Set primary tool groups and GitHub identity.
6. Save. The profile is immediately available in the profile selector at the top of any conversation.

Built-in profiles (marked with a lock icon) cannot be deleted but can be cloned — click duplicate to make an editable copy.

## Switching profiles mid-conversation

Use the profile selector at the top of the chat panel. Switching takes effect from the next message. The conversation history is preserved; only the active persona and tool prominence change.
