# Security policy

`rbx-apikey` handles the most sensitive credentials of any rbx-forge tool: it
mints, rotates and prints Open Cloud API key secrets, and the key
administration endpoints it calls take a Roblox session cookie, a whole account
session. That makes the threat model worth stating.

## Reporting a vulnerability

Report it privately, through GitHub: on this repository, open the **Security**
tab and choose **Report a vulnerability**, or go straight to
<https://github.com/rbx-forge/rbx-apikey/security/advisories/new>.

Do not open a public issue for a vulnerability. Do not paste a real API key,
secret or cookie into a report: a redacted reproduction is always enough.

What helps: the `rbx-apikey --version`, the command you ran, what you
expected, what happened, and why it is a security problem rather than a bug. A
proof of concept is welcome but never required.

**What to expect.** This is a solo-maintained project. An acknowledgement
within a week is the realistic commitment; a fix lands as fast as the severity
warrants. You will be credited in the advisory and the changelog unless you
ask otherwise. There is no bounty program.

## Supported versions

Only the latest release. At 0.x, fixes go out as a new release rather than
being backported.

## What counts

In scope:

- A key secret or a cookie written, logged or printed anywhere it was not
  asked for, or sent anywhere other than the Roblox host the command is
  talking to. `resolve` printing the secret you asked for is not this.
- A secret written into a file git does not ignore, or a lockfile or secret
  file readable by anyone other than its owner.
- The Studio cookie read without asking, or on a run that has nobody to ask.
- A key created with more scopes, targets or allowed addresses than
  `rbxapikey.toml` declares.
- A path traversal or arbitrary write from remote-controlled data.
- Dependency vulnerabilities with a plausible path to any of the above.

Out of scope:

- A key doing what its declared scopes allow. A file that declares too much is
  fixed in the file.
- Anything requiring an attacker who already has your credentials, or write
  access to your machine or repository.
- Rate limits, quotas, and Roblox-side API behavior. Report those to Roblox.

## Verifying a download

Each release archive carries a build provenance attestation, a signed
statement of which workflow run and commit produced it:

```sh
gh attestation verify rbx-apikey-v0.0.0-x86_64-unknown-linux-gnu.zip --repo rbx-forge/rbx-apikey
```
