# Setting Up GitHub Identity

GitHub Identity lets Athen commit code, push branches, and open pull requests on your behalf. You can configure two separate identities — a "Bot" identity (a dedicated Athen account separate from yours) and a "User" identity (your own GitHub account) — and choose which one each agent profile uses.

## Prerequisites

- A GitHub account for the identity you want to configure (either your own or a dedicated bot account).
- A Personal Access Token (PAT) for that account. You will create this on github.com in the steps below.
- Athen running with the vault available (it is by default on a normal install).

## Steps

### Step 1 — Create a Personal Access Token on GitHub

1. Open your browser and go to **github.com**. Sign in as the account you want to use (your own account for "User" identity, or a dedicated bot account for "Bot" identity).
2. Click your profile picture in the top-right corner and choose **Settings**.
3. Scroll down in the left sidebar and click **Developer settings**.
4. Click **Personal access tokens**, then **Tokens (classic)**.
5. Click **Generate new token** → **Generate new token (classic)**.
6. Give the token a name you will recognize, for example "Athen Bot" or "Athen User".
7. Set an expiration. A longer expiration (90 days or 1 year) means fewer trips back here to renew.
8. Under **Select scopes**, check at minimum:
   - `repo` — full control of private and public repositories (needed to push branches and open PRs)
   - `workflow` — needed if Athen will trigger or modify GitHub Actions
9. Click **Generate token** at the bottom.
10. Copy the token that appears. **You will not see it again.** Paste it somewhere safe for the next step.

### Step 2 — Enter the token in Athen

1. Open Athen and click **Settings** (the gear icon in the sidebar).
2. Navigate to **Connections** → **GitHub Identity**.
3. You will see two sections: **Bot** and **User**. Click the one you are setting up.
4. Paste the token you copied into the **Personal Access Token** field.
5. Fill in **Name** — this is the commit author name that will appear in git history (for example, "Athen Bot" or your real name).
6. Fill in **Email** — the email address linked to the GitHub account. This must match the account's verified email or GitHub will reject the commits.
7. Click **Save**.

A green checkmark confirms the token was stored securely in the system keychain (or encrypted file on systems without a keychain). The token never appears in plain text after this point.

### Step 3 — Test the connection (optional but recommended)

On the same screen, click **Test**. Athen will make a read-only call to GitHub to verify the token is valid. If the test fails, double-check that you copied the full token and that the `repo` scope is checked.

### Step 4 — Assign the identity to an agent profile

Each agent profile can independently use the Bot identity, the User identity, or no GitHub credentials at all.

1. In Settings, go to **Agents & Tools** → **Profiles**.
2. Select the profile you want to configure (for example, "Coder" or "DevOps").
3. Find the **GitHub Identity** dropdown. The choices are:
   - **None** — git and gh commands run without any injected credentials.
   - **Bot** — injects the Bot token and commit author you configured above.
   - **User** — injects your own GitHub token and author details.
4. Choose the identity you want and click **Save profile**.

From now on, every time that profile runs a shell command, Athen automatically injects the right `GH_TOKEN`, `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, and `GIT_COMMITTER_EMAIL` environment variables. The agent does not need to know your credentials — they are injected transparently at the shell level.

## Common issues

**"Vault not available" error when saving.**
This means Athen could not open the system keychain. Restart Athen and try again. On Linux, make sure a keyring daemon (such as GNOME Keyring or KWallet) is running. If the problem persists, Athen falls back to an encrypted file in its data directory automatically.

**Test passes but git push fails with "remote: Permission to ... denied".**
The PAT was created on the correct account but that account does not have write access to the repository. Either add the account as a collaborator on the repo (in GitHub → repository Settings → Collaborators) or switch to the User identity if your own account has access.

**Commits appear under the wrong name in GitHub history.**
Check that the **Email** field in Athen matches a verified email on the GitHub account. GitHub uses the email to link commits to a profile. Go to github.com → Settings → Emails to see which addresses are verified.

**Token expired — the agent is getting authentication errors.**
PATs expire. Go back to github.com → Developer settings → Personal access tokens, generate a new token for the same account, and repeat Step 2. You can paste the new token over the old one; only the most recent token is stored.

**I want Athen to act as me on some repos and as the bot on others.**
Set up both identities (Steps 1–2 for each account). Then assign different agent profiles to use different identities. For example, set the "Coder" profile to "Bot" for open-source work and create a second "Personal Dev" profile set to "User" for private repos.
