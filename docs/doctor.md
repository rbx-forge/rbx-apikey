# rbx-apikey doctor

Answer "why doesn't it work" in one command. `rbx-apikey doctor` reads the credential that is actually loaded, asks Roblox what it holds for that key, compares it against the tools this directory configures, and makes one real authenticated call. Every check is read-only: `doctor` never creates, updates or deletes anything. Nothing it does contacts anyone but Roblox, unless you pass `--check-ip`: see [the IP allowlist check](#3-ip-allowlist).

## Features

- **Credential provenance** - Which API key is live and where it came from. `RBX_API_KEY` is one variable shared by every tool, so the loaded key is often not the one you think
- **Session validity** - Whether Roblox still accepts the Studio cookie, and which account it signs in as
- **Key validity** - Enabled, expired, expiring soon, with the date and the time left
- **IP allowlist** - The CIDRs Roblox stores for the key, which fail as an opaque 401 when stale. With `--check-ip`, compared against this machine's public address
- **Scope coverage** - For each config file present, which of that tool's operations the key can and cannot run
- **Read probe** - One `GET` against a universe, to prove the whole chain works end to end
- **Actionable failures** - Every failing line carries what to do about it. A check that could not run says so, and is never reported as a pass

## Usage

```bash
rbx-apikey doctor                          # Diagnose whatever RBX_API_KEY holds
rbx-apikey doctor --env prod               # Resolve the probe target from rbxplace.toml
rbx-apikey doctor --universe-id 123        # Name the probe target directly
rbx-apikey doctor --key deploy             # Diagnose a key declared in rbxapikey.toml
rbx-apikey doctor --no-probe               # Skip the authenticated read
rbx-apikey doctor --check-ip               # Also compare the IP allowlist (asks an echo service)
```

## Output

```
rbx-apikey doctor

Credentials
  ✓ API key         RBX_API_KEY (environment)
  ! Studio cookie   auto-detected from a local Studio install
      → A session cookie is a full-account credential, more powerful than any scoped key. Pass
        --no-auto-cookie or set RBX_COOKIE= if you did not intend it.
  · rbxapikey.toml  3 key(s) declared, 3 created
  ✓ session         live: signed in as builderman (156)

Key validity
  · identified as  "deploy" (this project): opsdev_deploy on Roblox
  ✓ enabled        yes
  ✓ expiry         2026-11-12T09:00:00Z (in 90d)

IP allowlist
  · allowed CIDRs  203.0.113.7/32
      → Not compared against this machine's public IP. Doing that means asking a third-party
        echo service (https://api.ipify.org) for an address this machine cannot read off its
        own interfaces, so it is opt-in: re-run with `--check-ip`. A stale entry here fails as
        an opaque 401 that looks exactly like a wrong key, so check it first when a call that
        should work does not. See docs/doctor.md.

Scope coverage
  · places                       rbxplace.toml is here
  ✓ list a universe's places     covered
  ✗ upload / promote a place     missing universe-places:write
      → rbxplace.toml is here, so this is a call you can expect to make. Add
        universe-places:write to the key's `scopes` in rbxapikey.toml and run
        `rbx-apikey update <key>`.

Read probe
  · target              universe 5544332211 (--env prod_readonly)
  ✓ authenticated read  200: read "My Game"

1 problem(s) found.
```

### Symbols

| Symbol | Meaning |
| --- | --- |
| `✓` | Checked, and fine |
| `!` | Checked, fine, but worth knowing |
| `✗` | Checked, and broken. Always followed by `→`, what to do about it |
| `·` | Not a check: a fact needed to read the rest |
| `-` | Could not be checked, and why. Never counted as a pass |

## Exit status

`0` when nothing is broken, `1` when a check failed. A check that could not run does not change the exit status, but the summary line says how many there were: the difference between "your scopes are fine" and "your scopes were never looked at" is the point of the command.

## The checks

### 1. Which credential is active

The API key comes from `--api-key`, from `RBX_API_KEY`, or (with `--key <name>`) from the secret backend of a key declared in `rbxapikey.toml`. `doctor` says which, because `RBX_API_KEY` is a single variable shared by every tool that reads it and by anything else in your shell, so a key left over from another project is a routine cause of a refusal that looks like a scope problem.

The Studio cookie is reported separately: explicit (`--cookie` / `RBX_COOKIE`), auto-detected, or absent. An auto-detected cookie is a `!` rather than a `✓` on purpose: a session cookie is a full-account credential, strictly more powerful than any scoped API key.

Its **session** is then checked, with one call to `users.roblox.com/v1/users/authenticated`: where the cookie came from is not the same question as whether Roblox still accepts it, and a cookie that has expired refuses every command that needs one. A live session passes and names the account it signs in as. A refused one fails, with both ways to renew it. No cookie is skipped rather than failed (most commands never need one) and so is a check that could not run at all, because a service that did not answer is not a session that was refused. `update`, `regenerate` and `delete` make the same check before their first write, so an expired session refuses them before anything changes.

### 2. Key validity

`doctor` identifies the loaded secret against the keys the signed-in account holds, matching on the secret preview Roblox publishes with each key: the same string the Creator Hub shows in its "Key" column. That is what makes the check work for a key you have been using for a month; `rbx-apikey introspect` is authoritative but only while the JWT inside the secret is valid, roughly an hour after create or regenerate.

It then reports **enabled**, and **expiry** with the time remaining. A key inside two weeks of lapsing warns without failing.

This check needs the cookie, since Roblox's key-administration endpoints are not on Open Cloud. Without one it is skipped, with the reason given, rather than passed.

### 3. IP allowlist

`doctor` always prints the CIDRs Roblox stores for the key. A stale entry fails as an opaque 401 that looks exactly like a wrong key, so seeing the allowed addresses next to the probe's result is often enough to spot it.

Comparing them against this machine's address is **opt-in**, behind `--check-ip`.

#### Why it is opt-in

A machine behind NAT cannot read its own public address off its interfaces. The only way to learn it is to ask something on the outside, which means telling a third party where you are, in a tool whose whole pitch is least privilege. So `rbx-apikey doctor` does not do it on its own initiative:

- **Without `--check-ip`, no packet leaves for anyone but Roblox.** The allowlist is printed with a line saying the comparison was not made, and the flag that would make it.
- **With `--check-ip`, exactly one request goes out**, to `https://api.ipify.org`. That service is named on the line that reports your address, not only here, so nobody finds out afterwards that a third party saw it.
- **No request is made when the answer is already known.** An empty allowlist, an allowlist containing `0.0.0.0/0`, and a key whose configuration could not be read all answer the question without asking anyone, flag or no flag.

`https://api.ipify.org` was chosen for what it does not do: it answers `GET /` with the caller's address as bare text and nothing else. No API key, no registration, no query string carrying anything about this machine beyond the connection itself.

#### What it reports

```
IP allowlist
  · allowed CIDRs  203.0.113.0/24, 198.51.100.7/32
  · public IP      203.0.113.9: asked https://api.ipify.org, which therefore saw it
  ✓ this machine   203.0.113.9 is inside 203.0.113.0/24
```

and when it is not:

```
  ✗ this machine   198.51.100.42 is in none of the allowed CIDRs
      → Every call with this key is refused as an opaque 401 until the allowlist covers this
        address. Add 198.51.100.42/32 to the key's `allowed_cidrs` in rbxapikey.toml and run
        `rbx-apikey update <key>`, or pass `--no-ip` to that command to drop the restriction.
        A home connection's address usually changes, so a host route written today is next
        month's 401.
```

#### What it never does

**An unresolved address is never reported as a mismatch.** Offline, service down, timed out, a captive portal answering with a login page: every one of those is a check that could not run (`-`), with the reason given, and none of them changes the exit status. A false "you are locked out" is expensive: it sends you to edit a key that is fine.

Three things produce a `-` rather than a `✓` or a `✗`:

| Situation | Why not an answer |
| --- | --- |
| The echo service did not answer | Nothing was resolved. Whether you are inside the allowlist is simply unknown |
| The allowlist holds no entry of your address's family | A v6 answer against a v4-only list compares nothing. Roblox may still see this machine at a v4 address the list covers |
| An entry could not be read as a CIDR, and nothing else matched | The entry that would have covered you might be the one that did not parse |

**The lookup times out in 3 seconds**, far below the 60s the rest of the tool allows. `rbx-apikey doctor` is what you run when the network is already misbehaving, so it stays usable offline: a caller with no connectivity gets their report back rather than a spinner.

### 4. Scope coverage

For each of `rbxplace.toml`, `rbxmeta.toml`, `rbxconfig.toml`, `rbxrtbf.toml` and `rbxshop.toml` that is present, `doctor` lists the operations a tool configured by that file runs, and whether the key carries the scopes each one needs, as that tool documents them.

Two limits worth knowing:

- **Presence, not parsing.** The unit of detection is the config file existing. `doctor` does not read `rbxshop.toml` to find out whether you actually declare badges, so a repo that only uses game passes is still told it cannot manage badges. Each line names the operation and the missing scope, so an operation you never run is visibly not your problem. The alternative (parsing every file properly) means carrying a model of each format and keeping it in step with tools that evolve on their own.
- **Scope type and operation, not target.** A scope's `targetParts` name universes, datastores or creators, and deciding whether a given target covers a given call means resolving your env, which `doctor` would have to guess at. It answers the narrower question honestly: a key that lacks the scope outright is the failure people actually hit.

### 5. Read probe

One `GET /cloud/v2/universes/{id}` with the key. It needs `universe:read`, changes nothing, costs nothing, and is the request a metadata sync opens with, so a failure here is a failure you were going to hit anyway.

The target comes from `--universe-id`, from `--env <name>` against `rbxplace.toml`, or, when `rbxplace.toml` defines exactly one env, from that one. Which was used is printed before the result. With no target the probe is skipped rather than guessed at, and `--no-probe` skips it outright.

A refusal is read for you:

| Status | What it means |
| --- | --- |
| 401 | The key was rejected before permissions were considered. Either the secret is wrong, or the IP allowlist no longer contains this machine: the two fail identically, and the second is the one nobody guesses. `--check-ip` tells them apart |
| 403 | The key is valid but not allowed to make this call: a missing scope, or a scope whose target does not cover this universe |
| 404 | The universe id does not exist, or the key's owner cannot see it |

## Related

- [docs/apikey.md](./apikey.md): declaring and creating the keys `doctor` reports on
