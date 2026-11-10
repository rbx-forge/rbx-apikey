# rbx-apikey doctor

Why doesn't it work? `doctor` reads the credential actually loaded, asks Roblox what it holds for that key, compares it with the tools this directory configures, and makes one authenticated read. It changes nothing, and contacts nobody but Roblox unless you pass `--check-ip`.

```sh
rbx-apikey doctor                    # whatever RBX_API_KEY holds
rbx-apikey doctor --key deploy       # a key declared in rbxapikey.toml
rbx-apikey doctor --env prod         # probe this env's universe
rbx-apikey doctor --check-ip         # also compare the IP allowlist
```

| Flag | Meaning |
| --- | --- |
| `--key <name>` | Diagnose a declared key, read from its secret backend |
| `--no-probe` | Skip the authenticated read |
| `--check-ip` | Compare the allowlist with this machine's public IP (asks `api.ipify.org`) |

`--env`, `--universe-id` choose the probe target; with one env in `rbxplace.toml`, that one is used. An `rbxplace.toml` that cannot be read or parsed fails the probe, naming the file and the error.

```
Credentials
  ✓ API key         RBX_API_KEY (environment)
  ! Studio cookie   auto-detected from a local Studio install
  ✓ session         live: signed in as builderman (156)

Key validity
  · identified as  "deploy" (this project): opsdev_deploy on Roblox
  ✓ expiry         2026-11-12T09:00:00Z (in 90d)

Scope coverage
  ✗ upload / promote a place     missing universe-places:write
      → Add universe-places:write to the key's `scopes` in rbxapikey.toml and run
        `rbx-apikey update <key>`.

Read probe
  ✓ authenticated read  200: read "My Game"
  · scopes              not tested: Roblox answers this read without universe:read, so a 200
                        shows the key is accepted, not what it may do

1 problem(s) found.
```

| Symbol | Meaning |
| --- | --- |
| `✓` | Checked, fine |
| `!` | Fine, worth knowing |
| `✗` | Broken, followed by `→` what to do |
| `·` | A fact, not a check |
| `-` | Could not be checked, and why. Never a pass |

Exits `1` if a check failed, `0` otherwise. The summary counts checks that could not run.

## The checks

1. **Which credential.** From `--api-key`, `RBX_API_KEY` or `--key`. `RBX_API_KEY` is shared by every tool, so a key from another project is a common cause of refusals. The cookie's source is shown too, and its session is checked. A declared key whose secret file is there and cannot be read is reported on that key, and the diagnosis goes on.
2. **Key validity.** The loaded secret is matched against the account's keys by Roblox's secret preview, then enabled and expiry are reported (a warning under two weeks). Needs the cookie.
3. **IP allowlist.** The stored ranges are always printed: a stale one fails as an opaque `401`, like a wrong key. With `--check-ip`, one request to `https://api.ipify.org` learns this machine's address and compares. An unanswered lookup, or an address family the list lacks, is reported as not checked, never as a mismatch.
4. **Scope coverage.** For each of `rbxplace.toml`, `rbxmeta.toml`, `rbxconfig.toml`, `rbxrtbf.toml` and `rbxshop.toml` present, whether the key has the scopes that tool's operations need. Based on the file being present, not its content, and on scope type and operation, not target. Syncing universe and place fields asks only for `universe:write` and `universe.place:write`: Roblox reads a universe and a place without `universe:read` or `universe.place:read`.
5. **Read probe.** One `GET /cloud/v2/universes/{id}`. It shows the key is accepted (right secret, inside the allowlist, enabled), not what it may do: Roblox answers this read without `universe:read`, and GET place without `universe.place:read`, so no read can verify those two scopes. Scope coverage (step 4) is read from the key's stored configuration instead.

| Probe status | Means |
| --- | --- |
| 401 | Wrong secret, **or** this machine is outside the IP allowlist (identical failures; `--check-ip` tells them apart) |
| 403 | Key accepted, read refused; not a missing scope. Check the universe is one the key's owner can open |
| 404 | Universe does not exist, or the key's owner cannot see it |
