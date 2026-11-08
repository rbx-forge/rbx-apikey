# Commands

Every command except `scopes` authenticates with a [session cookie](reference/authentication.md): Roblox's key administration is not on Open Cloud.

## Lifecycle

### rbx-apikey create

`rbx-apikey create <key> | --all`

Create the key on Roblox and store its secret. Refuses if the secret would land in a file git tracks.

| Flag | Meaning |
| --- | --- |
| `--all` | Every declared key |
| `--no-ip` | Allow every IP address |
| `--force` | Overwrite an existing lockfile entry |
| `--no-verify` | Skip the check that Roblox stored what was asked |
| `--yes` | Skip the confirmation |

A failed creation that Roblox reports as `500` prints the scopes it sent: a scope given a target it does not take fails that way.

### rbx-apikey update

`rbx-apikey update <key> | --all`

Apply the file to existing keys: enabled, expiry, IP ranges, scopes.

| Flag | Meaning |
| --- | --- |
| `--no-ip` | Allow every IP address |
| `--clean-files` | If `secret_file` moved, delete the old file without asking |
| `--yes` | Skip the confirmation |

### rbx-apikey regenerate

`rbx-apikey regenerate <key> | --all`

New secret, old one invalid. `--clean-files`, `--yes`.

### rbx-apikey delete

`rbx-apikey delete <key> | --all`

Delete on Roblox, then from the lockfile and the secret file. A failed delete keeps the record. `--clean-files`, `--yes`.

### rbx-apikey resolve

`rbx-apikey resolve <key>`

Print the raw secret on stdout, for a script: `export RBX_API_KEY=$(rbx-apikey resolve deploy)`.

### rbx-apikey introspect

`rbx-apikey introspect <key>`

What Roblox stored for the key. Only works while the secret is under an hour old.

## Inspecting

### rbx-apikey list

What this project declares and created.

| Flag | Meaning |
| --- | --- |
| `--expiry-only` | Names and expiry only |
| `--sort expiry` | Nearest expiry first |
| `--remote` | Every key on the **account**, tracked or not |
| `--group-id <id>` | With `--remote`: a group's keys instead of yours |
| `--json` | One JSON document, never a secret ([fields](reference/json.md#rbx-apikey-list-json)) |

Colours: green over 30 days, cyan 7 to 30, yellow under 7, red expired or secret missing.

`--remote` matches keys to the lockfile by id, never by name (`name_prefix` renames on purpose, and names repeat across accounts). It starts with the account it is listing: if that is not the account you expect, stop.

```text
API keys on Roblox for user 1234567890 (12 total):
  ✓ myproject_viewer         AAAAAAAAAA…  created 2026-08-03  in 90d           tracked → viewer
  ✗ oldgame_shop             CCCCCCCCCC…  created 2026-05-18  EXPIRED 17d ago  untracked
```

### rbx-apikey status

Compare the file, the lockfile and, with `--remote`, Roblox. Changes nothing.

| Status | Meaning |
| --- | --- |
| `HEALTHY` | All agree |
| `PENDING` | Declared, not created |
| `EXPIRED`, `EXPIRING_SOON` | |
| `ORPHAN_LOCK` | In the lockfile, no longer declared |
| `ORPHAN_REMOTE` | Gone from Roblox (`--remote` only) |
| `SECRET_MISSING` | No stored secret |
| `DISABLED` | |
| `CHECK_FAILED` | Roblox could not be asked |

`--json` gives one verdict per key ([fields](reference/json.md#rbx-apikey-status-json)).

### rbx-apikey can-manage

Can you create a key for this experience? For a group universe it depends on your role.

```sh
rbx-apikey can-manage --universe-id 5544332211
rbx-apikey can-manage --place-id 55443322110099 --place-id 66778899001122
rbx-apikey can-manage --env prod
```

!!! warning "`create` succeeds anyway"
    Roblox lets you create a key for a universe you cannot manage; it only fails at first use. This is the only check beforehand. A "no" has been verified end to end; a "yes" says nothing about scopes or the IP allowlist.

There is no positional id: a universe id can also be a valid place id elsewhere.

## Cleaning up the account

### rbx-apikey prune

Pick keys from the account listing and delete them.

| Flag | Meaning |
| --- | --- |
| `--group-id <id>` | A group's keys |
| `--untracked-only` | Only keys this project does not track |
| `--expired-only` | Only expired keys |
| `--dry-run` | Print the candidates, delete nothing (the only non-interactive mode) |
| `--clean-files`, `--yes` | As for `delete` |

Nothing is preselected and there is no `--all`. A tracked key goes through `delete`; an untracked key is deleted on Roblox only.

## The scope catalog

### rbx-apikey scopes

```sh
rbx-apikey scopes list               # every known scope, by target type
rbx-apikey scopes show universe      # one scope type
```

The catalog is built into the binary and advisory: an unknown scope is a warning, and any string is forwarded. `scopes show --json` ([fields](reference/json.md#rbx-apikey-scopes-show-json)).

### rbx-apikey catalog regenerate

For maintainers. Rebuilds the catalog from Roblox's `openapi.json` (which scopes exist) and the key service's own list (which targets and operations a creation accepts; it wins where they disagree). A daily workflow runs it and opens a pull request on change.

## doctor

Why a key does not work: see [doctor](doctor.md).
