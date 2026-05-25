# Add an MCP Server to Athen

MCP servers are plugins that give Athen new tools — for example, a Slack connector so the agent can read and post messages, or a GitHub connector so it can open pull requests. Each MCP server runs as a small background program on your computer, and Athen talks to it automatically whenever it needs one of its tools.

## Prerequisites

- The MCP server software installed on your computer (for custom servers). Popular ones can be installed with a single command using Node.js or Python — the server's documentation will say which.
- The command needed to start the server (e.g., `npx @modelcontextprotocol/server-github`) or a URL if the server is remote.
- Any API keys or tokens the server needs (for example, a GitHub personal access token).

## Steps

### 1. Open the MCP Servers panel

Go to **Settings** → **Connections** → **MCP Servers**.

### 2. Choose a server to add

The panel shows two sections:

- **Built-in catalog** — servers that Athen ships ready to enable. Toggle one on to activate it immediately (some require configuration fields such as an API token).
- **Custom servers** — click **Add Server** to add any MCP server not in the catalog.

For built-in servers, toggling them on is all that is required if no extra configuration is shown. For custom servers, continue with the steps below.

### 3. Fill in the server details (custom servers)

Click **Add Server**. A form appears:

**Name** — a label you choose, for example "My GitHub" or "Notion (work)".

**Transport** — choose how Athen communicates with the server:

- **stdio (recommended for local servers)** — Athen launches the server as a subprocess and communicates over standard input/output. This is the most common type and works with all MCP servers written for Claude Desktop.
- **SSE (for remote servers)** — Athen connects to a URL over HTTP Server-Sent Events. Use this when the server is running on another machine or a hosted service.

**Command** (stdio only) — the command Athen should run to start the server, including any arguments. Examples:
- `npx -y @modelcontextprotocol/server-filesystem /Users/you/Documents`
- `npx -y @modelcontextprotocol/server-github`
- `python -m my_mcp_server`

**URL** (SSE only) — the full address of the remote server, including `https://`.

**Environment variables** — if the server needs an API key or token, enter it here. The name goes in the first field (e.g., `GITHUB_TOKEN`) and the value in the second. Athen stores the value in an encrypted vault — it is never stored in plain text.

### 4. Test the connection

Click **Test Connection**. Athen starts the server, completes the handshake, and lists the tools it found. If this succeeds, you will see the tool names. If it fails, the error message usually tells you what is wrong (missing binary, bad token, wrong command).

### 5. Save and enable

Click **Save**. If you checked "Enable now", the server starts immediately and its tools become available to the agent in all future conversations. If not, you can enable it later by toggling the switch in the MCP Servers list.

### 6. Adjust risk levels (optional)

By default, Athen assigns a medium risk level to all tools from a custom MCP server. This means the agent will ask for confirmation before using them on high-stakes tasks. To adjust this, expand the server row in the list and use the **Risk** dropdown next to each tool. Options range from Read-only (agent uses freely) to System (always confirms with you first).

## Popular MCP servers to try

Here are some widely-used servers and how to install them:

**Filesystem** — read and write files in a folder you choose:
Command: `npx -y @modelcontextprotocol/server-filesystem /path/to/your/folder`

**GitHub** — open issues, read pull requests, push commits:
Command: `npx -y @modelcontextprotocol/server-github`
Environment variable: `GITHUB_TOKEN` = your GitHub personal access token (generate one at github.com/settings/tokens)

**Slack** — read and post Slack messages:
Command: `npx -y @modelcontextprotocol/server-slack`
Environment variables: `SLACK_BOT_TOKEN` and `SLACK_TEAM_ID` (from your Slack app settings)

**Notion** — read and create Notion pages:
Command: `npx -y @modelcontextprotocol/server-notion`
Environment variable: `NOTION_API_KEY` = your Notion integration token (from notion.so/my-integrations)

All four require Node.js installed on your computer. You can check by opening a terminal and typing `node --version`. If Node is not installed, download it from [nodejs.org](https://nodejs.org).

## Common Issues

**"Command not found" or spawn error**
The program in the Command field is not installed or not on your PATH. For `npx`-based servers, make sure Node.js is installed. For Python-based servers, make sure Python is installed and the package is installed (`pip install <package-name>`).

**The test succeeds but the agent does not use the tools**
Make sure the server is enabled (the toggle next to it is on). Also check that the agent profile you are using has access to MCP tools — you can verify this under Settings → Profiles.

**"Handshake failed" during test**
The command runs but the server is not speaking the expected protocol. Double-check the command arguments — for example, `npx -y @modelcontextprotocol/server-filesystem` requires a folder path after the package name.

**The server stops working after a computer restart**
Athen re-launches the server process automatically when needed, but if the server requires extra setup (like a running service), you may need to start that service again. Check the server's documentation for any setup steps.

**Environment variable errors / "not set in vault"**
The secret you entered was not saved. Go to the server entry in the MCP Servers list, click Edit, and re-enter the token in the environment variables section. Leave the field blank to keep an existing saved value.
