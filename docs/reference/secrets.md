# Secret storage

A key's secret goes to `rbxapikey.lock.toml` by default, or to the file named by `secret_file` (or `default_secret_file`):

```toml
[settings]
default_secret_file = ".secrets/{name}.env"
```

!!! warning "Gitignore it"
    Nothing here edits your `.gitignore`. Every command that writes a secret refuses to write it into a file git is not ignoring, and names the line to add: `create` for a new key, `regenerate` for a rotated secret, `update` when a changed `secret_file` moves one. The lockfile and a secret file are checked alike, for every key of the run, before the first question and before Roblox is asked. One refused key refuses the whole run, and the message names every refused file with its keys, so nothing has changed when it stops. Each line to add is the file's path from the root of its repository, starting with `/`, for the `.gitignore` there. A file git already tracks is not untracked by an ignore line, so for one the message also gives the `git rm --cached` to run. When git cannot answer (not installed, or a directory it cannot look at), a warning says so and the write goes ahead.

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

## Where a secret is never printed

An error never quotes a Roblox answer that could not be read: it gives the parse error with any quoted value withheld, the answer's length and its field names. The answer to a create or a regenerate holds the new secret, and a listing holds the first characters of every secret.

One line prints a secret on purpose: when `create` or `regenerate` got a secret from Roblox and could not store it, it prints `Secret (save this manually)` on stderr, since Roblox will not show it again, and the command exits non-zero. Under `--stdout` it never does: stderr is what a CI job logs. Printing can fail partway, so the message says part of the secret may have reached stdout and to run `regenerate <key> --stdout` for a new one. The same line appears when `create` stored the secret in the lockfile and the lockfile then could not be written; the error names the live key and its id.

A key left that way is still recorded in the lockfile, marked `secret_unsaved = true`, with no secret in its entry, so it is never a live key nothing names and no command serves a dead or older secret for it. The entry keeps its `secret_file`, which may still hold a dead secret to clean up. `status` shows `SECRET_MISSING` with "never stored". The printed secret works wherever you save it; for this project to hold one again, run `rbx-apikey regenerate <key>`, which stores a new secret and clears the mark.
