# Configuration

`rbxapikey.toml` declares every key: its scopes, the universes it reaches, its expiry and the addresses allowed to use it. It is the source of truth; the lockfile and Roblox are the applied state. Edit the file, then `rbx-apikey update <key>`.

```toml
[settings]
default_expiration_months = 3
default_allowed_cidrs = ["203.0.113.7/32"]
default_secret_file = ".secrets/{name}.env"
name_prefix = "mygame_"

[keys.deploy]
envs = ["prod"]                         # universes from rbxplace.toml
scopes = ["universe-places:write", "universe:read"]

[keys.analytics]
universe_ids = [9876543210]             # or by id, without rbxplace.toml
scopes = ["universe.analytics:read"]
expiration_days = 30
```

`schemas/rbxapikey.schema.json` validates the file in editors that read JSON Schema.

## `[settings]`

| Field | Description |
| --- | --- |
| `default_enabled` | Keys enabled by default (`true`) |
| `default_expiration_months` | Default expiry. Omit to allow keys without expiry |
| `default_allowed_cidrs` | IP ranges for keys that set none. **Required in practice**: without an allowlist from here or the key, `create` and `update` refuse unless you pass `--no-ip`. Nothing is inferred |
| `default_envs` | Envs for keys that name no target at all |
| `name_prefix` | Prepended to every display name on Roblox, separator included (`mygame_`) |
| `default_secret_file` | Path template for secrets: `{name}` is the key, `{env_group}` its group |
| `readonly` | Refuse to load if any key asks for more than `read` or `list`. See [below](#read-only-keys) |

## `[keys.<name>]`

| Field | Description |
| --- | --- |
| `scopes` | `"scopeType:op1,op2"` strings. Duplicates are collapsed and reported |
| `envs` | Env names from `rbxplace.toml`, or a table of groups: see [One key per environment](#one-key-per-environment) |
| `universe_ids` | Universes by id, used as written |
| `place_ids` | Places by id, each resolved once to its universe and cached |
| `group_ids`, `user_ids` | Targets for group and user scopes |
| `datastores` | Restrict data store scopes to named stores. See [below](#data-store-restrictions) |
| `name` | Display name on Roblox (default: the key) |
| `description` | Description on Roblox (default: generated) |
| `enabled` | `true` / `false` |
| `expires_at` / `expiration_days` / `expiration_months` | One of them; first wins in that order. None: no expiry, unless a default applies |
| `allowed_cidrs` | IP ranges allowed to use the key |
| `secret_file` | Where the secret goes, templated like `default_secret_file`. Default: the lockfile |
| `readonly` | This key may only `read` and `list` |

!!! warning "Roblox refuses its own brand"
    A name or description with `rbx` or `roblox` next to an API or commerce term is rejected as `InvalidNameOrDescription`, without saying which field. The generated description avoids both words.

## Targets

Targets live in the file only. `--env`, `--place`, `--universe-id` and `--place-id` are refused on key subcommands (except [`can-manage`](commands.md#rbx-apikey-can-manage) and [`doctor`](doctor.md)): a universe given on `create` would be forgotten by the next `update`.

- `envs`, `universe_ids` and `place_ids` combine. `default_envs` applies only to a key naming none of them.
- The universes resolved from envs are recorded in the lockfile, under `[envs.<name>]`. If `rbxplace.toml` later points an env elsewhere, `create`, `update`, `regenerate`, `delete` and `status` refuse and show the change. To accept it, delete that `[envs.<name>]` section of the lockfile, not a key's entry, then `rbx-apikey update` the keys on that env so their scopes follow. A key's entry is the only record of a live key: without it, `create` would make a second one.

## One key per environment

`envs = ["dev", "staging", "prod"]` is **one** key reaching all three universes, which suits a read-only key and little else. A table of groups declares one key per group, sharing everything but the envs:

```toml
[keys.deploy]
scopes = ["universe-places:write"]

[keys.deploy.envs]
ci   = ["dev", "staging"]   # key deploy_ci
prod = ["prod"]             # key deploy_prod
```

The group name is part of the key's identity: `deploy_ci` is its name on the command line and in the lockfile, `mygame_deploy_ci` on Roblox (with `name_prefix`), `.secrets/deploy_ci.env` for its secret. Renaming a group renames the key: delete the old one on Roblox.

Refused at load: a generated name that collides with another key, a fan-out writing every secret to one literal path, and an empty group (a key with no universe reaches all of them).

## Read-only keys

```toml
[settings]
readonly = true     # the whole file

[keys.viewer]
readonly = true     # or one key
```

Loading fails, naming the key and scope, if a scope asks for anything but `read` or `list`. Roblox lets `update` add write scopes to an existing key, so this check at load is the only guard.

## Data store restrictions

```toml
[keys.datastores_key]
envs = ["prod"]
scopes = ["universe-datastores.objects:read"]

[[keys.datastores_key.datastores]]
universe_id = 9876543210
name = "UserData"
operations = ["read"]
```
