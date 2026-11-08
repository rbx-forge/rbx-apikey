# Secret storage

A key's secret goes to `rbxapikey.lock.toml` by default, or to the file named by `secret_file` (or `default_secret_file`):

```toml
[settings]
default_secret_file = ".secrets/{name}.env"
```

!!! warning "Gitignore it"
    Nothing here edits your `.gitignore`. `create` refuses to write a secret into a file git is not ignoring, and names the line to add.

Both the lockfile and secret files are written readable by your account only: mode `0600` on Linux and macOS, inherited access removed on Windows (through `icacls`) before the secret is written. If that fails on Windows, a warning names the file and the write goes ahead: check who can read it. This matters most on a drive outside your profile, which usually grants every authenticated user.

To use a secret from a script:

```sh
export RBX_API_KEY=$(rbx-apikey resolve deploy)
```

`--json` output never includes a secret, a piece of one, or where it is stored.
