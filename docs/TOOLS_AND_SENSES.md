# Tools, Senses & Notifications

## Tool Execution

The agent can use tools through 4 backends, choosing the best for each situation:

1. **NativeMcp**: Compiled Rust binaries, stdio JSON-RPC. Fastest, most portable.
2. **Shell**: Nushell (cross-platform default) or native shell (bash/zsh/pwsh). For CLI tools, curl, etc.
3. **Script**: Python execution for data processing, ML tasks, etc.
4. **HttpApi**: Direct HTTP calls to external services.

### Built-in Tools (AppToolRegistry)
The agent has 10 built-in tools available via `AppToolRegistry` (wraps `ShellToolRegistry` + calendar tools):
1. `shell_execute` -- runs shell commands (sandboxed when bwrap available, with pre-execution risk check)
2. `read_file` -- reads file contents via `tokio::fs`
3. `write_file` -- writes content to files via `tokio::fs`
4. `list_directory` -- lists directory entries as JSON
5. `memory_store` -- stores key-value pairs in in-session memory (HashMap)
6. `memory_recall` -- retrieves by key or lists all stored keys
7. `calendar_list` -- query calendar events by date range
8. `calendar_create` -- create calendar events (title, time, location, category, reminders, recurrence). Sets `created_by: Agent`.
9. `calendar_update` -- partial update of calendar events (loads existing, merges only provided fields)
10. `calendar_delete` -- delete calendar event by ID

### Sandbox Tiers
- **L1 actions**: No sandbox (read-only operations)
- **L2 actions**: OS-native sandbox (bwrap/landlock on Linux, sandbox-exec on macOS, Job Objects on Windows) -- zero install required. Default profile: `RestrictedWrite` (writable: `/tmp`, `$HOME`, cwd; read-only: everything else).
- **L3+ actions**: Container (Podman preferred, Docker fallback). Auto-detected. If unavailable, offer to install or fall back to manual approval.

### Cross-Platform Shell
Primary: Embedded Nushell (Rust-native, same commands everywhere).
Fallback: Native platform shell for platform-specific tools.

---

## Senses (Monitors)

Priority order:
1. **USER**: Always highest priority, never questioned
2. **Calendar**: Agent-managed deadlines take priority
3. **Telegram**: Owner messages treated as direct user input (L1); others triaged normally (L2)
4. **Messaging** (iMessage/WhatsApp): Usually more urgent
5. **Email**: Lowest priority sense

Each sense normalizes its input to `SenseEvent` format before sending to the coordinator.

### Notification Channels
When the agent needs to contact the user:
- App in foreground -> in-app notification
- App in background -> preferred messaging channel (iMessage/WhatsApp)
- Configurable quiet hours
