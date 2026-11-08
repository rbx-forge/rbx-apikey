# rbx-apikey

Declare Roblox Open Cloud API keys in a file, and keep Roblox in step with it: create, update, rotate and delete keys from `rbxapikey.toml`, see when Roblox no longer matches, and find out why a key does not work.

```sh
rbx-apikey create deploy        # mint it on Roblox, store its secret
rbx-apikey status --remote      # the file, the lockfile and Roblox, compared
rbx-apikey doctor               # why doesn't this key work?
```

[Quick start](quickstart.md){ .md-button .md-button--primary } [Configuration](configuration.md){ .md-button }

**Scopes and reach in a file.** Review a key's power in a pull request, not in the Creator Hub.

**One key per environment.** A CI key and a production key share their scopes without sharing their universes.

**No secret in output.** `--json` never carries a secret, a piece of one, or where it is stored.

## Install

=== "mise"

    ```sh
    mise use --pin github:rbx-forge/rbx-apikey
    ```

=== "Rokit"

    ```sh
    rokit add rbx-forge/rbx-apikey
    ```

Archives for every platform are on the [releases page](https://github.com/rbx-forge/rbx-apikey/releases).

---

A community tool under [MPL-2.0](https://github.com/rbx-forge/rbx-apikey/blob/main/LICENSE), not affiliated with Roblox Corporation. Source and changelog on [GitHub](https://github.com/rbx-forge/rbx-apikey).
