# Quick start

## 1. Install

=== "mise"

    ```sh
    mise use --pin github:rbx-forge/rbx-apikey
    ```

=== "Rokit"

    ```sh
    rokit add rbx-forge/rbx-apikey
    ```

## 2. Declare a key

```toml title="rbxapikey.toml"
[settings]
default_expiration_months = 3
default_allowed_cidrs = ["203.0.113.7/32"]   # the addresses allowed to use keys
default_secret_file = ".secrets/{name}.env"

[keys.deploy]
universe_ids = [9876543210]
scopes = ["universe-places:write", "universe:read"]
```

Add `.secrets/` to `.gitignore`: `create` refuses otherwise.

## 3. Check you may create it

Sign in to Roblox Studio with the account that will own the key (`rbx-apikey` uses its session, after asking), then:

```sh
rbx-apikey can-manage --universe-id 9876543210
```

## 4. Create and use it

```sh
rbx-apikey create deploy
export RBX_API_KEY=$(rbx-apikey resolve deploy)
rbx-apikey doctor --universe-id 9876543210
```

## 5. Keep it in step

Edit `rbxapikey.toml`, then:

```sh
rbx-apikey status --remote
rbx-apikey update deploy
rbx-apikey regenerate deploy    # rotate the secret
```
