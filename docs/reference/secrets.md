# Secret storage

A key's secret goes to `rbxapikey.lock.toml` by default, or to the file named by `secret_file` (or `default_secret_file`):

```toml
[settings]
default_secret_file = ".secrets/{name}.env"
```

!!! warning "Gitignore it"
    Nothing here edits your `.gitignore`. Every command that writes a secret refuses to write it into a file git is not ignoring, and names the line to add: `create` for a new key, `regenerate` for a rotated secret, `update` when a changed `secret_file` moves one. The lockfile and a secret file are checked alike, and the check runs before Roblox is asked, so a refusal leaves the key and its secret as they were. When git cannot answer (not installed, or a directory it cannot look at), a warning says so and the write goes ahead.

Both the lockfile and secret files are written readable by your account only: mode `0600` on Linux and macOS, inherited access removed on Windows (through `icacls`) before the secret is written. If that fails on Windows, a warning names the file and the write goes ahead: check who can read it. This matters most on a drive outside your profile, which usually grants every authenticated user.

To use a secret from a script:

```sh
export RBX_API_KEY=$(rbx-apikey resolve deploy)
```

## Straight to a vault

`--stdout` on `create` or `regenerate` prints the secret on stdout and stores it nowhere: no secret file, nothing in the lockfile. Every other line goes to stderr, so a pipe receives the secret alone, with no trailing newline.

```sh
rbx-apikey create deploy --stdout | <your vault's store command>
rbx-apikey regenerate deploy --stdout | <your vault's store command>
```

The lockfile records `secret_external = true` for the key, so `status` does not report it as missing, `list` shows it as held elsewhere, and `resolve` refuses it. A later `regenerate` without `--stdout` stores the secret again where the config says. Rotating with `--stdout` offers to delete an old secret file, which now holds a dead secret.

!!! warning "Roblox shows a secret once"
    If the pipe fails, the secret is lost: rotate again with `regenerate --stdout`. `--stdout` takes one key, never `--all`.

`--json` output never includes a secret, a piece of one, or where it is stored.
