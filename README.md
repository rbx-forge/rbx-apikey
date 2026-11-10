# rbx-apikey

Declare Roblox Open Cloud API keys in a file, and keep Roblox in step with it: create, update, rotate and delete keys from `rbxapikey.toml`, see when Roblox no longer matches, and find out why a key does not work.

**Documentation: <https://rbx-forge.github.io/rbx-apikey/>**

## Install

With [mise](https://mise.jdx.dev):

```bash
mise use --pin github:rbx-forge/rbx-apikey
```

Or with [Rokit](https://github.com/rojo-rbx/rokit):

```bash
rokit add rbx-forge/rbx-apikey
```

Either one pins the latest release in your project's config.

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

Gitignore `.secrets/`: `create`, `regenerate` and `update` refuse to write a secret into a file git does not ignore, before anything is sent to Roblox. Key administration is not on Open Cloud, so these commands use a Roblox session cookie, from `--cookie`, `RBX_COOKIE`, or a signed-in Studio after asking.

`schemas/rbxapikey.schema.json` validates `rbxapikey.toml` in editors that read JSON Schema.

## License

[MPL-2.0](LICENSE). A community tool, not affiliated with Roblox Corporation.
