# Connect Your Calendar to Athen

Athen can read and create events on your calendar so the agent can schedule meetings, check your availability, and remind you of upcoming commitments. This guide walks you through connecting a calendar account using CalDAV — a standard protocol supported by iCloud, Google Calendar, Fastmail, Yandex, and Nextcloud.

## Prerequisites

- Your calendar account credentials (email address and password)
- For most providers: a special **app-specific password** (not your regular account password) — details below
- Two-factor authentication enabled on your account (required by Gmail, iCloud, and others before app passwords can be created)

## Steps

### 1. Open the Calendar Sources panel

Go to **Settings** (the gear icon) → **Connections** → **Calendar Sources** → click **Add Source**.

### 2. Pick your provider

A preset picker appears with these options:

- **iCloud** — Apple Calendar (icloud.com, me.com, mac.com addresses)
- **Google** — Google Calendar via CalDAV (gmail.com or Google Workspace addresses)
- **Fastmail** — Fastmail Calendar
- **Yandex** — Yandex Calendar
- **Nextcloud** — Self-hosted Nextcloud server
- **Custom** — Any other CalDAV-compatible server

Select the one that matches your account. The server URL fills in automatically for all presets except Nextcloud and Custom.

### 3. Create an app-specific password

Most providers require a special password just for third-party apps like Athen — your regular account password will be rejected. Here is how to create one:

**iCloud:** Go to [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords → click the `+` button. Two-factor authentication must be enabled on your Apple ID first.

**Google Calendar:** Go to [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords). If you do not see this page, enable 2-Step Verification first under Security. Choose "Other (custom name)", type "Athen", and click Generate. Copy the 16-character password shown.

**Fastmail:** Go to [fastmail.com/settings/security/apppasswords](https://www.fastmail.com/settings/security/apppasswords) → New App Password. Give it a name like "Athen". The generated password is what you paste into Athen.

**Yandex:** Go to [id.yandex.com/security/app-passwords](https://id.yandex.com/security/app-passwords) → Create app password.

**Nextcloud:** You can use your regular Nextcloud password, or create a device-specific password under your Nextcloud user profile → Security → App passwords.

### 4. Enter your credentials

Back in Athen's Add Source panel:

- **Username** — your full email address (e.g., you@gmail.com)
- **Password** — paste the app-specific password you just created (not your account password)
- **Server URL** — filled in automatically for presets; for Nextcloud, enter your server address in the format `https://your-server.com/remote.php/dav/calendars/your-username/`

For Google, Athen substitutes your email address into the CalDAV URL automatically.

### 5. Test the connection

Click **Test Connection**. Athen connects to the server and checks your credentials. If it succeeds, you will see a green confirmation. If it fails, check the Common Issues section below.

### 6. First sync and choosing calendars

After a successful test, Athen runs a first sync that pulls in events from one year in the past to one year ahead. This may take a moment depending on how many events you have.

Once the sync completes, an expanded row appears listing every calendar found on your account. Use the checkboxes to select which calendars Athen should watch — for example, "Work" but not "Birthdays".

Background syncs then run every 5 minutes, pulling in recent changes automatically.

## Common Issues

**"Authentication failed" or 401 error**
You likely entered your account password instead of an app-specific password. Go back to your provider's security page, generate an app password, and paste that instead. Your regular login password is blocked by design for third-party apps.

**iCloud: connection fails even with an app password**
Make sure two-factor authentication is turned on for your Apple ID. App-specific passwords cannot be created without it. Also confirm you are using your full Apple ID email address (not an alias) as the username.

**Google: can't find the app passwords page**
The app passwords page only appears when 2-Step Verification is active on your Google account. Go to [myaccount.google.com/security](https://myaccount.google.com/security) and enable it first. Google Workspace administrators may have disabled app passwords — contact them if the option is missing.

**Nextcloud: "connection refused" or timeout**
Double-check the server URL format. It must be the full CalDAV path including your username: `https://your-server.com/remote.php/dav/calendars/your-username/`. If your Nextcloud uses a non-standard port such as 8080, include it in the URL.

**Calendars appear but events are missing**
Make sure you selected the correct calendars in the "Choose calendars" step after the first sync. Events only appear from calendars that have their checkbox ticked. You can revisit this by clicking the calendar source row in the list and adjusting your selection.

**Sync stops working after a while**
App-specific passwords can be revoked if you change your account password or sign out of all devices. Generate a new app password and update the entry under Settings → Connections → Calendar Sources.
