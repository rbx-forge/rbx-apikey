# rbx-apikey

Manage Roblox Open Cloud API keys declaratively from `rbxapikey.toml`. Supports multiple secret backends (lockfile, custom file) and automatic scope validation against Roblox's live API.

## Features

- **Declarative configuration** - Define all keys in `rbxapikey.toml` with explicit scopes
- **Multi-backend secrets** - Store API key secrets in lockfile (default) or custom file
- **Scope validation** - Embedded catalog with warnings (not errors) for unknown scopes, enabling durability as Roblox adds new scopes
- **State reconciliation** - `status` command detects drift between config, lockfile, and Roblox
- **Auto-introspection** - Verify key creation succeeded and detect configuration drift
- **Datastore granularity** - Restrict universe-datastore scopes to specific datastore names

## Usage

### Create a key

```bash
rbx-apikey create <key>              # Create one key from rbxapikey.toml
rbx-apikey create --all              # Create all keys
rbx-apikey create <key> --no-ip      # Allow all IPs (no CIDR restriction)
rbx-apikey create <key> --force      # Overwrite existing lockfile entry
rbx-apikey create <key> --no-verify  # Skip post-create introspection
```

### Manage keys

```bash
# List and inspect
rbx-apikey list                      # Overview of all keys and expiry
rbx-apikey list --expiry-only        # Compact view: just name and expiration dates
rbx-apikey list --sort expiry        # Sort keys by expiration date (nearest first)
rbx-apikey list --remote             # Every key on the account, tracked or not
rbx-apikey list --remote --group-id 445566778   # A group's keys instead of yours
rbx-apikey list --json               # One JSON document on stdout, never a secret

# Clean up the account
rbx-apikey prune --dry-run           # Show what would be offered, delete nothing
rbx-apikey prune --untracked-only    # Only offer keys this project does not track
rbx-apikey prune --expired-only      # Only offer keys past their expiry

# Status and reconciliation
rbx-apikey status                    # Compare config vs lockfile vs Roblox
rbx-apikey status --remote           # Also query live Roblox API for drift detection
rbx-apikey status --json             # One verdict per key, as a JSON document

# Update and regenerate
rbx-apikey update <key>|--all        # Apply TOML configuration to Roblox
rbx-apikey update <key> --no-ip      # Update without CIDR restriction
rbx-apikey regenerate <key>|--all    # Rotate the API key secret

# Delete and introspect
rbx-apikey delete <key>|--all        # Delete key from Roblox and lockfile
rbx-apikey delete <key> --yes        # Skip confirmation prompts
rbx-apikey delete <key> --clean-files  # Also delete secret_file without asking
rbx-apikey introspect <key>          # Show what Roblox has stored (key < 1h old)
rbx-apikey resolve <key>             # Print the raw secret (for scripts)

# Scope catalog
rbx-apikey scopes list               # All known scopes grouped by target type
rbx-apikey scopes show <scopeType>   # Details for one scope type
rbx-apikey scopes show universe --json          # The same, as a JSON document
rbx-apikey catalog regenerate [url]  # Refresh the catalog (maintainers)
```

### Where a scope's target comes from

`regenerate` reads two documents, because they answer different questions.

`openapi.json` says which scopes exist and what they are for. It describes **endpoints**, so a scope's target there is inferred from the routes that mention it, and that inference is wrong for fourteen scopes: the group routes are annotated as creator resources, so `group` came out creator-targeted.

The key service's own list, `cloud-authentication/v1/scopes`, says what a creation request may send. It is the document that accepts or rejects, so it wins on every `target_type` and on the operation lists. `rbx-apikey scopes list` prints both URLs, and `regenerate` prints every correction it applied.

The distinction is not cosmetic. Sending a target part for a scope that takes none does not come back as a rejection: the service throws, and `create` reports `500 Internal Server Error: Exception was thrown by handler`, naming neither the scope nor the field. When that happens, `create` now prints the scope entries it sent, which is the one thing it knows for certain.

Because the catalog is embedded at compile time and the service's list is live, the two can drift apart on a day `openapi.json` does not move. A daily workflow regenerates the catalog against the live list and opens a pull request when anything but the date stamp changed, so a stale target is caught before someone meets that 500.

### Commands explained

**`list` command:**
- `rbx-apikey list` - Show all keys with full details (alphabetical order)
- `rbx-apikey list --expiry-only` - Compact view: just key names and expiration dates
- `rbx-apikey list --sort expiry` - Sort by expiration date (nearest first)
- `rbx-apikey list --expiry-only --sort expiry` - Compact + sorted by expiry

**Expiration colors:**
- Green - Expires in >30 days (healthy)
- Yellow - Expires in <7 days (rotate soon)
- Red - Already expired or missing secret

**`update` vs `status`:**
- `rbx-apikey update <key>|--all` - One-way sync: apply your TOML configuration to Roblox. Updates metadata (enabled, expiry, IP restrictions) and scopes on existing keys.
- `rbx-apikey status` - Compare: shows differences between TOML (desired), lockfile (tracked), and Roblox (actual). Does not modify anything.
- `rbx-apikey status --remote` - Same as above, but also queries the live Roblox API to detect drift.

**Key creation workflow:**
1. Define keys in `rbxapikey.toml`
2. Run `rbx-apikey create <key>` to generate the key on Roblox and save the secret
3. Later, edit `rbxapikey.toml` and run `rbx-apikey update <key>` to re-apply the config

## Configuration

Create `rbxapikey.toml` in your project root. In a project, keys target Roblox universes by **env name**, declared in `rbxplace.toml`, the file that maps a project's env names to its universes and places. Outside one, a key names its universes by id instead: see [Without `rbxplace.toml`](#without-rbxplacetoml).

```toml
[settings]
default_enabled = true                       # optional
default_expiration_months = 3                # optional (omit for no default expiry)
default_allowed_cidrs = []                   # optional
default_envs = ["prod"]                      # optional; fallback for keys that omit `envs`
name_prefix = "mygame_"                      # optional; prepended verbatim (e.g. "mygame_deploy")
default_secret_file = ".secrets/{name}.env"  # optional; {name} is the key name, {env_group} its group

[keys.mykey]
envs = ["dev", "prod"]                       # resolves to universe_ids via rbxplace.toml
scopes = ["asset:read,write", "universe:read"]
expiration_months = 3                        # 3 months from now
# or: expiration_days = 90                   # 90 days from now (more precise)
# or: expires_at = "2025-12-31T00:00:00Z"    # exact date (ISO 8601)
```

### Without `rbxplace.toml`

`rbx-apikey` works on its own, in a directory with no `rbxplace.toml`. A key names what it targets by id:

```toml
[keys.analytics]
universe_ids = [9876543210]           # a universe, as is
place_ids = [234567890123456]         # a place, standing for the universe it belongs to
scopes = ["universe.analytics:read"]
```

- **`universe_ids`** is used as written. Nothing is asked of Roblox, unless a scope needs the universe's owner.
- **`place_ids`** is resolved to its universe once, on the first `create`, and cached in the lockfile like an env. A place never moves to another universe, so the answer is not asked again.
- **`envs` combines with both**: a key may name an env and an id side by side. What it does not combine with is a table of env groups, because every group's key would get the ids, and keeping one environment's key off another's universe is what the table is for.
- **`settings.default_envs` is not applied** to a key that names ids. It is the fallback for a key that names no target, and adding an env to one that named its own would also make it need an `rbxplace.toml` again.

The targets live in the file only, never on the command line. A key is a long-lived credential whose universes are fixed when it is created, and `update` rebuilds them from the file; a universe passed as a flag on `create` would be forgotten by the next `update`. So the global `--env`, `--place`, `--universe-id` and `--place-id` are refused on every `rbx-apikey` subcommand except [`can-manage`](#rbx-apikey-can-manage), which reads them, rather than silently ignored.

### One key per environment

`envs = ["dev", "staging", "prod"]` declares **one** key that can reach all three universes. That is right for a read-only observability key and wrong for everything else: the safety model this tool is built on is that scopes and universes are bound at creation, so the blast radius of a key is whatever you gave it on the day you made it.

Writing `envs` as a **table of named groups** instead declares one key per group:

```toml
[keys.deploy]
scopes = ["universe-places:write"]

[keys.deploy.envs]
ci   = ["dev", "staging"]   # → key deploy_ci, scoped to the dev + staging universes
prod = ["prod"]             # → key deploy_prod, scoped to the prod universe only
```

Two keys, one declaration. Everything except the envs (scopes above all) is written once, so a scope added for production cannot be forgotten for CI. That is the whole point: the alternative is three near-identical `[keys.*]` blocks, and a scope added to one and not the others compiles fine, syncs fine, and desyncs your environments silently.

TOML distinguishes an array from a table on its own, so there is no flag to set and the array form means exactly what it always meant.

**The group name is identity, not decoration.** It names four things at once:

| | with `name_prefix = "mygame_"` |
| --- | --- |
| the key you name on the command line | `deploy_ci` |
| its display name on Roblox | `mygame_deploy_ci` |
| its secret file under `.secrets/{name}.env` | `.secrets/deploy_ci.env` |
| its lockfile entry | `[keys.deploy_ci]` |

So `rbx-apikey create deploy_ci`, `status`, `regenerate`, `delete` and the rest all work on a generated key exactly as they do on a hand-written one. Naming the *declaration* (`rbx-apikey create deploy`) matches no key, and says so, listing the keys it did produce.

- **Adding an env to a group extends that key.** `ci = ["dev", "staging", "qa"]` widens `deploy_ci` on the next `update`, which is an ordinary change surfaced by drift detection.
- **Renaming a group renames a key.** `ci` → `build` makes `deploy_ci` disappear from the config and `deploy_build` appear. `status` reports the first as `ORPHAN_LOCK` and the second as `PENDING`, which is what renaming a hand-written key already does. Roblox does not learn the new name until you create it; delete the old key rather than leaving it live.
- **`{env_group}`** is available in `secret_file` and `default_secret_file` for layouts that want the group as a path segment of its own: `.secrets/{env_group}/{name}.env`. It expands to nothing for a key that does not fan out. You rarely need it, because `{name}` already carries the group.

Two shapes are refused at load rather than at create time, both because they produce keys that cannot be told apart:

- a group whose generated name collides with another key in the file;
- a fan-out whose keys would all write their secret to one path, because `secret_file` was given a literal path with no placeholder in it. The last `create` would overwrite the previous key's secret, leaving a live key on Roblox that nothing local can authenticate as.

An empty group (`ci = []`) is refused too. A key targeting no universe is scoped to *all* of them, which is the opposite of what the syntax is for.

### Duplicate scopes

A scope written twice is collapsed on the way in, at both levels (`["asset:read", "asset:read"]` and `["asset:read,read"]` alike) and the collapse is reported on stderr, naming the table and the entry. First-seen order survives, so what you read back from `apikey introspect` is in the order your file wrote.

Collapsing is safe: a duplicate grants nothing extra, it only makes the payload Roblox is asked to store redundant. It is reported rather than dropped in silence because it is almost always a merge artefact, and the line it was meant to be is worth a look.

Two entries sharing a scope type but listing different operations (`asset:read` next to `asset:write`) are **not** duplicates and are left alone. Folding them into their union is a normalisation, not a deduplication.

### Drift validation

Each create/update/regenerate operation writes the resolved `universe_ids` into the lockfile as a snapshot of what's actually deployed. On the next run, the tool re-resolves the envs and compares against the snapshot. **When they differ, the tool refuses to proceed and prints the discrepancy.** This catches accidental retargeting (e.g. someone edited `rbxplace.toml` to point env `dev` at a fresh universe) before the change is silently pushed to Roblox.

Only envs can drift: a universe id is its own answer, and a place's universe never changes. With no `rbxplace.toml` at all there is nothing to compare against, and the check is skipped.

To acknowledge an intentional change, delete that key's entry from `rbxapikey.lock.toml` and re-run the operation. The next run will re-resolve, re-create owner cache, and write a fresh snapshot.

### Global settings

- `default_enabled` - (optional) Whether keys are enabled by default (defaults to `true`)
- `default_expiration_months` - (optional) Default months until key expiry; omit to allow keys without expiry
- `default_allowed_cidrs` - Default IP CIDR blocks for keys that set no `allowed_cidrs` of their own. **Not optional in practice**: with no allowlist from either place, `create` and `update` refuse and name the three ways out: set it here, set `allowed_cidrs` on the key, or pass `--no-ip` to allow every address. Nothing is inferred; neither this tool nor Roblox fills in your address for you
- `default_envs` - (optional) Default env list used when a key names no target of its own: no `envs`, no `universe_ids`, no `place_ids`. Each name must exist in `rbxplace.toml`.
- `name_prefix` - (optional) Prepended verbatim to every key's display name on Roblox. You control the separator: use `mygame_` for underscore, `mygame-` for dash, etc. Useful when the same `rbxapikey.toml` is reused across games so the Creator Hub distinguishes same-named keys (e.g. `mygame_deploy`, `othergame_deploy`). Does not change the local TOML key.
- `readonly` - (optional) Refuse to load this file if any key in it asks for an operation other than `read` or `list`. See [Read-only by declaration](#read-only-by-declaration)
- `default_secret_file` - (optional) Path template used when a key has no explicit `secret_file`. The literal `{name}` is replaced with the TOML key and `{env_group}` with the key's env group, if it has one. Example: `.secrets/{name}.env` makes `[keys.deploy]` write to `.secrets/deploy.env` by default, and a fan-out of `deploy` write to `.secrets/deploy_ci.env` and `.secrets/deploy_prod.env`. Explicit `secret_file` on a key always wins.

### Key attributes

- `name` - (optional) Display name on Roblox Creator Hub (defaults to key ID)
- `description` - (optional) Description on Roblox Creator Hub; auto-generated if omitted. **Roblox refuses a name or description carrying a brand**, answering `Response.InvalidNameOrDescription` without saying which of the two it means. The working rule, measured against the live API, is that `rbx` or `roblox` glued to an API or commerce term is rejected: a description reading ``Validate `rbx secret` against real Open Cloud`` was refused, and the same text without the brand was accepted. Nothing is checked locally, because "glued to an API or commerce term" is a judgement and not a substring, so a local check would reject text Roblox takes; the tool explains the refusal when it happens, and its auto-generated fallback names neither Roblox nor itself
- `envs` - Either a list of env names from `rbxplace.toml` (one key, targeting every one of them), or a table of named groups (one key per group: see [One key per environment](#one-key-per-environment)). The list form falls back to `settings.default_envs` when omitted or empty and the key names no id either; the table form always names its envs.
- `universe_ids` - (optional) Universe ids this key targets directly, with no `rbxplace.toml`. See [Without `rbxplace.toml`](#without-rbxplacetoml)
- `place_ids` - (optional) Place ids this key targets, each resolved once to its universe and cached in the lockfile
- `group_ids` - (optional) List of group IDs for group-target scopes
- `user_ids` - (optional) List of user IDs for user-target scopes
- `scopes` - List of scope strings in format `"scopeType:operation1,operation2"`. A scope listed twice is collapsed and reported, at both levels: see [Duplicate scopes](#duplicate-scopes).
- `enabled` - (optional) `true`/`false` to enable/disable the key (defaults to `settings.default_enabled`)
- `expiration_months` - (optional) Months until key expires
- `expiration_days` - (optional) Days until key expires (more precise than months)
- `expires_at` - (optional) Exact expiration date in ISO 8601 format (e.g., `"2025-12-31T00:00:00Z"`); all three expiration fields are mutually exclusive (priority: `expires_at` > `expiration_days` > `expiration_months` > global default); omit all for keys that never expire
- `allowed_cidrs` - (optional) List of IP CIDR blocks allowed to use this key (defaults to `settings.default_allowed_cidrs`)
- `secret_file` - (optional) Custom file path to store the secret (defaults to lockfile). Templated like `default_secret_file`: `{name}` is the key name, `{env_group}` its env group. A fan-out declaration needs one of the two in the path, or its keys would share one file.
- `datastores` - (optional) Array of datastore restrictions (fine-grained datastore scopes by universe and name)

### Read-only by declaration

```toml
[settings]
readonly = true          # nothing in this file may ask for more than read or list

[keys.viewer]
readonly = true          # or just this one, in a file that is not
```

Loading fails, naming the key and the scope, if either is set and a scope asks for anything else:

```
[keys.viewer] is readonly and asks for `universe:write`. A readonly key may only use
read and list. Drop the operation, or drop `readonly`, but if this file is the one
that is not supposed to hold write scopes, dropping `readonly` is the change to think
twice about.
```

**Why this is a check rather than a comment.** Roblox binds a key to its scopes when it is created, and it is tempting to conclude that a key declared read-only cannot be made to write. Half of that is true: nothing widens a key at runtime, so whatever holds the secret cannot escalate it. The other half is false, and was disproved by running `rbx-apikey update` on an already-created key and watching Roblox accept a write scope. `update` is a verb this tool ships, and the file that declares the rule is the same file somebody would edit to break it.

So the guard sits at config load, which is the one place that sees the declaration before anything reaches Roblox.

It is an **allow-list** (`read` and `list`, nothing else) rather than a list of forbidden writes. Roblox adds operations whenever it likes, and a deny-list would quietly let each new one through, which is the failure this exists to close.

`[settings] readonly` and a per-key `readonly` add to each other; neither turns the other off. A key produced by [fan-out](#one-key-per-environment) is checked and named by its generated name, since that is the name it would be created under.

### Datastore granularity

Restrict `universe-datastore` scopes to specific datastore names:

```toml
[keys.datastores_key]
envs = ["prod"]
scopes = ["universe-datastores.objects:read"]

[[keys.datastores_key.datastores]]
universe_id = 9876543210
name = "UserData"
operations = ["read"]

[[keys.datastores_key.datastores]]
universe_id = 9876543210
name = "GameState"
operations = ["read", "write"]
```

## Secret storage

By default, secrets are stored in `rbxapikey.lock.toml`, which **you** have to gitignore: nothing in this tool writes into your `.gitignore`. `rbx-apikey create` checks before it creates anything and refuses if git is not ignoring that file, naming the line to add.

Both the lockfile and a custom secret file are written readable by your account only. On Linux and macOS that is mode `0600`, set when the file is created and tightened if the file already existed. On Windows `rbx-apikey` removes the inherited access and grants your user alone, through `icacls`, before the secret is written. If `icacls` fails, a warning names the file and the write still goes through: check who can read it. This matters most for a repository on a drive other than your profile folder, which usually inherits access for every authenticated user.

### Custom file backend

```toml
[keys.mykey]
secret_file = "/path/to/secret"
```

The secret will be stored in `/path/to/secret` instead of the lockfile.

## Workflow

All permanent configuration lives in `rbxapikey.toml`. To make changes:

1. **Edit the TOML** - Add/remove/modify keys and their settings
2. **Run `rbx-apikey update <key>|--all`** - Apply the TOML to Roblox

Example:

```bash
# Disable a key
vim rbxapikey.toml    # Set enabled = false
rbx-apikey update mykey

# Rotate expiry
vim rbxapikey.toml    # Change expiration_months
rbx-apikey update mykey
```

The TOML is the source of truth. The lockfile and Roblox are the applied state.

## Authentication

`rbx-apikey` uses cookie auth (the API key admin endpoints aren't on Open Cloud yet). The `.ROBLOSECURITY` cookie is supplied via the global `--cookie` flag, the `RBX_COOKIE` env var, or a local Roblox Studio install.

Studio detection is **opt-in**: `--auto-cookie` is the standing yes, an interactive run is asked once, `--no-auto-cookie` is the standing no, and a run with nowhere to ask declines by itself.

**In CI, that last rule means `--auto-cookie` is not the answer**: there is no Studio on a runner, and a runner that happens to have one must not reach into it. Every write verb here (`create`, `update`, `regenerate`, `delete`, `prune`) requires a cookie, so a scheduled `rbx-apikey create --all` needs `RBX_COOKIE` from a secret store. Prefer arranging the pipeline so none of these run there at all: the keys they mint are the credential CI should be *using*, not making.

A session cookie is a full-account credential, strictly more powerful than the scoped keys this command creates, so it is handled accordingly:

- **One resolution order.** `--cookie`, then `RBX_COOKIE`, then a signed-in Studio if the rule above allows it. An explicit value always wins, and `--no-auto-cookie` never suppresses it: that flag governs detection, not what you typed.
- **It says when it borrows.** When the Studio session is used, one line on stderr says so and names the flags that stop it.
- **It is never written down.** No cache, no keychain entry, no session file: it is read for the run and sent only to the Roblox endpoint of the call being made.

## `rbx-apikey can-manage`

Can you create a key for this experience at all? For a group-owned universe the answer depends on your group role, which `rbxapikey.toml` knows nothing about.

```sh
rbx-apikey can-manage --universe-id 5544332211
rbx-apikey can-manage --place-id 55443322110099   # resolved to its universe
rbx-apikey can-manage --env prod
```

```text
place 55443322110099 is in universe 5544332211
universe 5544332211: can create keys yes
```

**It authenticates with your Studio cookie, not with an API key**, and that is the design. Asking with a key is circular: a key is bound to its universes at creation, so a key for universe A answers `Forbidden` about universe B.

`--place-id` is repeatable here, and this is the command it is repeatable for:

```sh
rbx-apikey can-manage --place-id 55443322110099 --place-id 66778899001122
```

The other key subcommands refuse it by name, along with `--env`, `--place` and `--universe-id`: a key's targets come from `rbxapikey.toml`, and a flag that parsed and was then ignored would read like it worked.

**There is no positional id.** A place id and a universe id are both plain integers and the two spaces overlap: `5544332211` is one game's universe id *and* a valid place id belonging to a different universe. Say which you mean.

### Why it is worth running

Because `create` tells you nothing. Measured against a universe belonging to somebody else:

- `can-manage` said **no**
- `rbx-apikey create` **succeeded**, and introspection confirmed the scopes. Roblox does not check ownership when a key is made.
- the key then failed at first use: `The authorized user does not have sufficient permissions`

So a created key proves nothing. This is the only place the answer exists beforehand.

### How far to trust it

A **no** was verified end to end, through to the failed call. A **yes** was right twice, but it says nothing about the scopes you will request or about your IP allowlist, the two other ways a created key ends up unusable.

Two caveats. `canManage` means "can administer this experience", and that it also means "can use API keys here" is an inference from three observations, not something Roblox documents. And the endpoint is `develop.roblox.com`, legacy rather than Open Cloud, with no Open Cloud equivalent: if Roblox retires it, this command goes with it.

## `rbx-apikey list --remote` and `rbx-apikey prune`

Three commands answer three different questions, and only the last one can see a key this project never made:

| Command | Question | Source |
| --- | --- | --- |
| `list` | What does this project declare? | `rbxapikey.toml` + lockfile |
| `status --remote` | Is what this project declares still there? | one `GET` per lockfile entry |
| `list --remote` | What does the **account** actually hold? | one listing call, everything |

The gap the third one fills is the one nobody expects to be wide. An account used for any length of time accumulates keys from other checkouts, from other tools, and from clicking through the Creator Hub, and the project you are standing in tracks none of them. Nothing else in the CLI can see them.

```text
API keys on Roblox for user 1234567890 (12 total):
  ✓ myproject_viewer         AAAAAAAAAA…  created 2026-08-03  in 90d           tracked → viewer
  ✓ otherproject_shop        BBBBBBBBBB…  created 2026-06-18  in 44d           untracked
  ✗ oldgame_shop             CCCCCCCCCC…  created 2026-05-18  EXPIRED 17d ago  untracked

1 tracked by this project, 11 untracked (3 expired, 1 disabled).
```

The fourth column is Roblox's own secret preview, the same one the Creator Hub shows: the first characters of the secret and nothing more. It is enough to recognise a key you already hold without the tool ever storing the secret. The values above are placeholders: real previews are fragments of live credentials and do not belong in documentation.

### Names are not identity

The join between the account and your lockfile is on `cloud_auth_id`, **never on the name**, for two measured reasons:

- your lockfile calls a key `viewer` while Roblox calls it `myproject_viewer` (`name_prefix` does this on purpose);
- two different accounts can each hold a key called `deploy`.

Matching on names would report your own key as untracked and offer it for deletion, or worse, tie your lockfile entry to a stranger's key.

### `prune` is deliberately awkward

`prune` lists the account, you select with space, and it deletes what you picked.

- **Nothing is ever preselected.** A prune where the safe answer is "press enter" eventually deletes a production key.
- **There is no `--all`.** `delete --all` is bounded by your lockfile; a `prune --all` would not be.
- Selecting a **tracked** key routes through the ordinary `delete` path, so the lockfile entry and stored secret go with it. An **untracked** key is deleted on Roblox only: the tool never had its secret.
- `--dry-run` prints the candidates and exits, which is the only non-interactive mode. Deleting other people's keys from a script is not a workflow this supports.

### The listing is scoped to the active cookie

Output starts with the account id for a reason: switching the signed-in Studio account changes which account answers, and the same key names recur across accounts. If the header says a user id you did not expect, stop before selecting anything.

Group-owned keys are **not** included by default. Pass `--group-id`. An empty result for a group you belong to means the listing was not asked for that group, not that the group has no keys.

### The endpoint

Undocumented, and worth recording because nothing about it is guessable:

```
POST https://apis.roblox.com/cloud-authentication/v1/apiKeys
{"cursor": "", "limit": 100, "reverse": false, "groupId": <optional>}
→ {"cloudAuthInfo": [...], "nextCursor": "id_…", "previousCursor": "id_…"}
```

It is a **POST that reads**, and the resource is plural. Every `GET` spelling returns 404, and `GET /v1/apiKey/list` answers `Malformed CloudAuthId` because `list` lands in the by-id route. Cookie auth plus CSRF, like the rest of this crate. Found in the Creator Hub's own bundle (`getApiKeys` → `v1ApiKeysPost`) and confirmed against the live API and the browser's network log.

## Machine-readable output

`--json` on the three reads (`list`, `status` and `scopes show`) writes one JSON document to stdout and nothing else. Everything that is not the result (the "N item(s) need attention" line, the `status` summary and its tip, the auto-detected-cookie notice, the unknown-key warning from `rbxplace.toml`) goes to stderr, so `jq` reads the pipe and a human still reads the terminal.

Nothing that writes takes it. `create`, `update`, `regenerate`, `delete` and `prune` all stop and ask before they act, and a format that owns stdout cannot stop and ask. `resolve` is excluded for a harder reason: it prints the raw secret, so there is no document that could carry its answer.

### No secret, and no piece of one

This is the command that holds live Open Cloud credentials, and a document goes into a pipe, a CI log, an artifact upload. So the rule is absolute rather than careful: **no field in any of these documents carries a secret or any part of one.** Two consequences are worth stating, because both are things the human form does print:

- **`list --remote --json` has no secret preview.** The fourth column of the human listing is Roblox's own preview, the first characters of a live secret, and it is there so a person can recognise a key on their own screen. A prefix is still credential material and a document is not a screen.
- **`list --json` carries no path to the secret file.** The human form prints `set (file: .secrets/deploy.env)` because the person reading it is standing in that directory. The document says `secret_backend` (`lockfile` or `file`) and `secret_present`, which is what a script needs; where on disk a credential lives is not something to publish next to a report about it.

Two more omissions, for the same reason applied one step further out:

- **`status --json` carries no free-text detail.** The human form's trailing sentence is advice for a person, and one of its branches names the secret file. The status word says the same thing, so the document keeps the word and drops the sentence.
- **`list --remote --json` carries no `cloud_auth_id`.** The account listing prints a name, a preview, dates and a tracked tag, never the id, and most of the keys it returns belong to other checkouts and other tools. The **local** listing does print `id:` for the keys this project created, so the local document carries `id`.

### `rbx-apikey list --json`

What this project declares, joined to what it created: `rbxapikey.toml` and the lockfile, and nothing from Roblox.

```json
{
  "schema_version": 1,
  "sort": "name",
  "count": 2,
  "keys": [
    {
      "name": "deploy",
      "declared": true,
      "created": true,
      "id": "f58b4055-cafe-4e2f-9c2a-000000000001",
      "creator_id": "1234567890",
      "universe_ids": ["5544332211"],
      "expires_at": "2027-08-01T10:00:00.000Z",
      "days_until_expiry": 351,
      "secret_present": true,
      "secret_backend": "file"
    },
    { "name": "newkey", "declared": true, "created": false, "universe_ids": [] }
  ]
}
```

`declared` and `created` are the two tags the human listing prints, kept as separate booleans rather than folded into one word: a declared key with no lockfile entry is *pending*, a lockfile entry with no declaration is an *orphan*, and they are fixed in opposite directions. Everything the lockfile would have said (`id`, `creator_id`, `expires_at`, both secret fields) is **absent** for a key that was never created.

`days_until_expiry` is negative once it has passed, and **absent** both when there is no expiry and when the timestamp could not be parsed, which is the `(unparseable)` the human listing marks. `expires_at` stays at full precision where the listing shortens it to the date.

`sort` says which order `keys` is in, because the order of an array is meaningful and a stored document should say which one it got. `--expiry-only` is refused alongside `--json`: it is a narrower rendering of the same rows, and a document has no narrower rendering. `--sort` is fine.

### `rbx-apikey list --remote --json`

What the **account** holds, which is mostly not this project's doing. Same call as the human listing, so it costs no extra round trip: the `users/authenticated` request it already makes for the account line is the one that proves the session is live.

```json
{
  "schema_version": 1,
  "owner": { "kind": "user", "id": "1234567890" },
  "totals": { "total": 12, "tracked": 1, "untracked": 11, "expired": 3, "disabled": 1 },
  "keys": [
    {
      "name": "myproject_viewer",
      "state": "active",
      "tracked": true,
      "tracked_as": "viewer",
      "created_time": "2026-08-03T09:15:00.123Z",
      "expiration_time": "2026-11-01T09:15:00.123Z",
      "days_until_expiry": 90
    },
    { "name": "otherproject_shop", "state": "active", "tracked": false }
  ],
  "missing_on_account": []
}
```

`owner` is there for the same reason the human listing starts with the account id: switching the signed-in Studio account changes which account answers and the same key names recur across accounts. `tracked_as` is the lockfile's name for a key, which is not the name Roblox has for it (`name_prefix` makes `viewer` into `myproject_viewer` on purpose) and it is **absent** when `tracked` is false. `state` is `active`, `expired` or `disabled`. `missing_on_account` lists lockfile names Roblox no longer has, the same warning the human form prints.

### `rbx-apikey status --json`

```json
{
  "schema_version": 1,
  "remote": false,
  "count": 3,
  "issues": 1,
  "keys": [
    { "name": "deploy", "status": "HEALTHY", "healthy": true, "days_until_expiry": 351 },
    { "name": "newkey", "status": "PENDING", "healthy": false }
  ]
}
```

`status` is one of `HEALTHY`, `PENDING`, `EXPIRED`, `EXPIRING_SOON`, `ORPHAN_LOCK`, `ORPHAN_REMOTE`, `SECRET_MISSING`, `DISABLED`, `CHECK_FAILED`: the word the human form prints beside the glyph. `healthy` is true only for the first, so a gate reads one field instead of enumerating eight spellings of "no". `remote` says whether Roblox was asked: `ORPHAN_REMOTE` cannot be reported without it, so a consumer that sees no orphan needs to know which of the two runs it is reading. `issues` is the count the human form prints as "N key(s) need attention".

A drift between the lockfile and `rbxplace.toml` still fails the command outright, before any document is written. Empty stdout next to a non-zero exit says nothing was read.

```sh
rbx-apikey status --json | jq -r '.keys[] | select(.healthy | not) | "\(.name): \(.status)"'
```

### `rbx-apikey scopes show --json`

```json
{
  "schema_version": 1,
  "scope_type": "universe",
  "known": true,
  "catalog_version": "2026-05-13",
  "target_type": "universe",
  "operations": ["read", "write"]
}
```

The catalog is advisory: an unknown scope is a warning and never an error, and `rbxapikey.toml` forwards any string Roblox will take. So an unknown scope is a document with `"known": false` and exit 0, not a failure, and `target_type` and `operations` are **absent** there, because the catalog has no answer rather than an empty one. `catalog_version` is what tells "Roblox does not have this scope" from "this catalog is older than that scope".
