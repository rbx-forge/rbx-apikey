# rbx-apikey

Declare Roblox Open Cloud API keys in a file, and keep Roblox in step with it.

`rbxapikey.toml` says which keys exist, which scopes and universes each one gets, when it expires and which addresses may use it. `rbx-apikey` creates them from that file, re-applies it when it changes, rotates and deletes them, and tells you when what Roblox holds no longer matches what the file says. `rbx-apikey doctor` answers the other question: why a key does not work.

## Install

With [mise](https://mise.jdx.dev) or [Rokit](https://github.com/rojo-rbx/rokit), either of which pins the latest release in your project's config:

```bash
mise use --pin github:rbx-forge/rbx-apikey
rokit add rbx-forge/rbx-apikey   # or, with Rokit
```

Or take a binary for your platform from the [releases page](https://github.com/rbx-forge/rbx-apikey/releases), which ships a `SHA256SUMS` alongside the archives, or build from source:

```bash
cargo install --git https://github.com/rbx-forge/rbx-apikey --locked
```

## Quick start

```toml
# rbxapikey.toml
[settings]
default_expiration_months = 3
default_allowed_cidrs = ["203.0.113.7/32"]
default_secret_file = ".secrets/{name}.env"

[keys.deploy]
universe_ids = [9876543210]
scopes = ["universe-places:write", "universe:read"]
```

```bash
rbx-apikey create deploy        # mint it on Roblox, write its secret
rbx-apikey status --remote      # compare the file, the lockfile and Roblox
rbx-apikey regenerate deploy    # rotate the secret
rbx-apikey resolve deploy       # print the secret, for a script
rbx-apikey doctor               # why doesn't this key work?
```

Gitignore `.secrets/` (or `rbxapikey.lock.toml`, if you keep secrets there): `create` refuses to write a secret into a file git is tracking.

## What it is careful about

- **The cookie.** Roblox's key administration is not on Open Cloud, so these commands authenticate with a `.ROBLOSECURITY` cookie: `--cookie`, `RBX_COOKIE`, or a signed-in Roblox Studio **if you say yes**. A run with nowhere to ask (CI, a pipe) refuses rather than borrowing a session.
- **Targets live in the file.** A key's universes are fixed when it is created, so `--env`, `--universe-id` and friends are refused on the key subcommands rather than silently ignored. `can-manage` and `doctor` are the two that read them.
- **One key per environment.** `envs` written as a table of groups declares one key per group, so a production key and a CI key share their scopes without sharing their reach.
- **Read-only by declaration.** `readonly = true` refuses to load a file that asks for any operation but `read` and `list`.
- **No secret in a document.** `--json` output never carries a secret, a piece of one, or the path to the file holding one.

## Documentation

- [docs/apikey.md](docs/apikey.md): every subcommand, the file format, secret storage, `can-manage`, `list --remote` and `prune`, the JSON documents
- [docs/doctor.md](docs/doctor.md): the five checks `doctor` runs, and what each failure means

`schemas/rbxapikey.schema.json` validates `rbxapikey.toml` in editors that read JSON Schema (taplo, Even Better TOML).

## License

[MPL-2.0](LICENSE)
