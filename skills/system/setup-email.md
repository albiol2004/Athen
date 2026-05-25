# Connect Your Email to Athen

Connecting your email lets Athen monitor your inbox, summarize messages, draft replies, and send email on your behalf. This guide covers the most common providers and takes about 5 minutes to complete.

## Prerequisites

- Your email address and the password for your account
- For Gmail, iCloud, Yahoo, Outlook, and most others: a special **app-specific password** — your regular account password will not work for IMAP access
- Two-factor authentication enabled if your provider requires it before app passwords can be generated

## Steps

### 1. Open the Email settings panel

Go to **Settings** → **Email** and click **Add Account**.

### 2. Enter your email address

Type your full email address. Athen checks it against a built-in list of known providers and fills in the mail server details automatically. The following providers are recognised:

- **Gmail** (gmail.com, googlemail.com)
- **Outlook.com** (outlook.com, hotmail.com, live.com, msn.com)
- **iCloud Mail** (icloud.com, me.com, mac.com)
- **Fastmail** (fastmail.com, fastmail.fm)
- **Yahoo Mail** (yahoo.com, yahoo.co.uk, ymail.com, rocketmail.com)
- **Proton Mail** (proton.me, protonmail.com, pm.me) — requires Proton Bridge (see below)
- **Yandex Mail** (yandex.com, yandex.ru)
- **GMX** (gmx.com, gmx.net, gmx.de)
- **Zoho Mail** (zoho.com, zoho.eu)
- **AOL** (aol.com)

For providers not on this list, Athen tries the Thunderbird autoconfig database. If that also finds nothing, you can enter server details manually using the Advanced fields (see the end of this guide).

### 3. Create an app-specific password

Most major providers block standard account passwords from being used by email clients. You need to generate a one-time app password specifically for Athen.

**Gmail:**
1. Go to [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords)
2. If you do not see that page, first enable 2-Step Verification at [myaccount.google.com/security](https://myaccount.google.com/security)
3. Choose "Other (custom name)", type "Athen", click Generate
4. Copy the 16-character password — it will not be shown again

**Outlook.com / Hotmail / Live:**
1. Go to [account.live.com/proofs/AppPassword](https://account.live.com/proofs/AppPassword)
2. Create a new app password and copy it

**iCloud Mail:**
1. Go to [appleid.apple.com](https://appleid.apple.com) → Sign-In and Security → App-Specific Passwords
2. Click the `+` button, name it "Athen", click Create
3. Two-factor authentication must be enabled on your Apple ID

**Yahoo Mail:**
1. Go to [login.yahoo.com/account/security/app-passwords](https://login.yahoo.com/account/security/app-passwords)
2. Click Generate app password, give it a name, click Generate
3. 2-Step Verification must be active first

**Fastmail:**
1. Go to [fastmail.com/settings/security/apppasswords](https://www.fastmail.com/settings/security/apppasswords)
2. Click New App Password, name it "Athen", click Generate

**Yandex Mail:**
1. Go to [id.yandex.com/security/app-passwords](https://id.yandex.com/security/app-passwords)
2. Create a new app password for "Mail"

**Zoho Mail:**
1. Go to [accounts.zoho.com/home#security/apppasswords](https://accounts.zoho.com/home#security/apppasswords)
2. Generate an app-specific password

**AOL:**
1. Go to [login.aol.com/account/security/app-passwords](https://login.aol.com/account/security/app-passwords)
2. 2-Step Verification must be active first

**GMX:** Use your regular GMX password. Before connecting, go to GMX webmail Settings → Connections (or POP3/IMAP) and enable IMAP access.

### 4. Proton Mail (special case)

Proton Mail encrypts all messages on their servers. To access it from any email client including Athen, you need the **Proton Bridge** app running locally on the same computer as Athen. Bridge is available at [proton.me/mail/bridge](https://proton.me/mail/bridge) and requires a paid Proton plan.

Once Bridge is running, copy the IMAP credentials Bridge shows (username and a Bridge-generated password — not your Proton login). Athen auto-fills the server as `127.0.0.1` on ports 1143 (IMAP) and 1025 (SMTP) when it detects a Proton address.

### 5. Enter your password and test

Paste the app-specific password into the password field. Click **Test Connection**. Athen tests incoming (IMAP) and outgoing (SMTP) mail separately and reports each result.

If both pass, click **Save**. Athen starts syncing your inbox immediately.

### 6. Advanced: manual server settings

If autodetection did not fill in the correct values, expand the **Advanced** section to enter settings by hand:

- **IMAP host and port** — your provider's incoming mail server. Typically port 993 with SSL/TLS.
- **SMTP host and port** — your provider's outgoing mail server. Typically port 465 or 587 (SMTP over SSL/TLS or STARTTLS respectively).
- **Security** — SSL/TLS or STARTTLS, to match what your provider expects.

Your email provider's help pages or support team can supply the exact values if you are unsure.

## Common Issues

**"Authentication failed" on Gmail**
You used your Google account password instead of an app-specific password. Generate one at [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords) and try again. If that page is not visible, you need to enable 2-Step Verification first.

**"Google flagged this sign-in" / web login required**
Google's security system blocked the IMAP connection attempt as suspicious. Open the security checkup link shown and confirm the sign-in, then try connecting in Athen again.

**"We can't reach the mail server" / connection refused**
The host name or port may be wrong, or a firewall is blocking the connection. Corporate networks sometimes block IMAP and SMTP — try a different network or contact your IT team. If you entered settings manually, double-check them against your provider's documentation.

**"Too many email clients on this account"**
Too many apps are connected at once. Close other email clients temporarily (Thunderbird, Apple Mail, Outlook), wait a minute, then try again.

**Outlook shows "535 5.7.139" error**
Microsoft is phasing out basic password authentication on Outlook.com. Generate an app-specific password from the Outlook app passwords page, or wait for OAuth browser sign-in in a future Athen update.

**"STARTTLS isn't supported here"**
Change the IMAP security setting to SSL/TLS on port 993 in the Advanced section. All major providers use SSL/TLS on IMAP; STARTTLS on port 143 is not supported in this build.
