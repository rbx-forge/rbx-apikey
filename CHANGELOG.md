# Changelog

## [Unreleased]

### Security

- An answer from Roblox that could not be read is no longer quoted in the error. The answer to a create or a regenerate carries the new secret, and a listing the first characters of every secret, so a change on Roblox's side would have printed one. The error gives the parse error with its quoted values withheld, the answer's length and its field names; the same holds for the universe answer read when resolving an owner. A lockfile entry, a listed key's secret preview and the other values holding a secret print it as `<redacted>` under `{:?}`.
- `regenerate`'s `Secret (save this manually)` line, printed when a rotated secret could not be stored, goes to stderr. It went to stdout, which a script captures or pipes into a file.

### Fixed

- When `create` cannot store or print the secret of a key Roblox has just created, the key is still recorded in the lockfile, marked `secret_unsaved = true` with no secret in the entry, and the command exits non-zero. It stopped with nothing recorded, leaving a live key that nothing in the project named. The secret is printed on stderr to save by hand, except under `--stdout`, where it is never put on stderr: the message says part of it may have reached stdout and to run `regenerate <key> --stdout`. `status` shows such a key as `SECRET_MISSING`, "never stored", and `regenerate` clears the mark. A `regenerate` whose new secret could not be stored marks the key the same way, so the dead secret is no longer served by `status` or `resolve`.
- `create` stores the secret and writes the lockfile as soon as Roblox has created the key, before re-applying an expiry Roblox ignored and before the check that reads back its scopes, so a hang or a Ctrl-C in either cannot leave a live key unrecorded. A `regenerate` whose lockfile could not be saved says that the file on disk still describes the key as before, with its old, dead secret. A lockfile that cannot be written then names the live key and its id, and hands over the secret on stderr when the lockfile was its only copy.
- `create --force` on a key the lockfile tracks deletes that key on Roblox before creating the new one. It created the new key and wrote it over the record of the old one, which stayed live on Roblox with nothing in the project naming it. The new key's request (targets, owners, scopes, IP ranges, name and description) is built before the delete, so one that cannot be built leaves the old key working, and a name or description containing `rbx` or `roblox`, which Roblox refuses, stops it before the delete. A plain `create` still leaves that judgement to Roblox. The keys it replaces are named before the confirmation; a key of another account is refused before anything happens; a delete that fails creates nothing and keeps the record, and a delete Roblox made that the lockfile could not record is reported as such.
- A key that expired less than a day ago counts as expired. Days left were rounded toward zero, so it read `in 0d`, `status` called it `EXPIRING_SOON`, `list --remote` and `doctor` showed it active, `prune --expired-only` left it out, and `days_until_expiry` was `0`. A key counts as expired from the second of its expiry: `days_until_expiry` is `-1` within the day after it and `0` within the day before, a key that expired exactly N days ago reads `N d ago`, and the text says `under 1d` instead of `0d`, in both directions.
- `can-manage` sends a cookie given with its `.ROBLOSECURITY=` prefix as it is. It added the prefix a second time, and Roblox refused a valid session as a stale cookie.
- `introspect` no longer asks to use a signed-in Studio session. It authenticates with the key's secret alone and never sent the cookie it asked for.
- A lockfile of an older schema is refused with advice that deletes the keys it records before anything recreates them. The advice was to delete the file and run `create --all`, which gave every key a second one and left the first live with no record. A lockfile of a newer schema asks for a newer build.
- The configuration docs said to accept a repointed env by deleting the key's lockfile entry, which leaves the key live with no record and makes the next `create` a duplicate. The section to delete is `[envs.<name>]`, which is what the refusal itself says.
- `regenerate --help` no longer says `--yes` is required with `--all`: without it, `--all` asks before rotating.
- `create` stops before asking anything when an env the lockfile records now resolves to another universe in `rbxplace.toml`, as `update`, `regenerate` and `delete` do. It resolved the env afresh and wrote the new universe into the lockfile's shared entry, so the keys created for the old universe no longer showed as drifted.

## [0.2.0] - 2026-11-10

### Fixed

- A write of the lockfile or a secret file that fails leaves no empty file behind, and says when it could not put the old content back. A file a scanner holds for a moment is written once it lets go.
- `regenerate` and `update` refuse to write a secret into a file git does not ignore, as `create` does, and all three check a `secret_file` as well as the lockfile. `create` checked only the lockfile, and `regenerate` and `update` checked nothing. Every key of the run is checked before the first question and before Roblox is asked, and one refused key refuses the whole run with a message naming every refused file and its keys, so nothing has been created, rotated or moved when it stops.
- The git guard answers through a symlink or a junction: git was handed the linked path, called it outside the repository, and the check read as one that could not run, so the secret was written with a warning. It tells a file git already tracks from one it does not ignore, since an ignore line does not untrack a file, and says to run `git rm --cached` for it. The line it suggests is the file's path from the repository root, anchored with `/`: it copied the config's path, which matched nothing for `./.secrets/`, was not a pattern for `../x/`, and anchored elsewhere when run from a subdirectory. A relative secret file path is checked where it is, and a `.git` that cannot be looked at gives a warning instead of passing as "not a repository".
- An `rbxplace.toml`, an `rbxapikey.toml` or a lockfile that is there and cannot be looked at stops the command, naming the file. Each read as absent somewhere: the env file switched the drift check off without a word, the config was reported missing with advice to create it, and `list --remote` and `doctor` showed every key of the project as untracked. `doctor` reports a config or lockfile it cannot look at, read or parse as a finding naming the file, and goes on with what does not depend on it, such as the key in `RBX_API_KEY`; it used to report a directory declaring no keys.
- `doctor`'s read probe names an `rbxplace.toml` it cannot look at or parse, with the error, and fails on it. It used to skip with "no universe to read" and advise adding an env to the file.
- A secret file that is there and cannot be looked at or read is an error naming the file. `update`, `resolve` and `introspect` stop on it. `list` shows it on that key as `secret cannot be read` with the error, and `--json` as `secret_readable: false`; `status` gives it the verdict `SECRET_UNREADABLE`; `doctor` reports it on that key, with the error and the way out (write the secret back, or `regenerate`). All three go on with the other keys. Deleting an old secret file that cannot be looked at says so, naming it, instead of deleting nothing in silence. It read as a missing secret: `update` reported it "not found" and sent the reader to rotate a key whose secret was fine, and `status` and `list` showed `SECRET_MISSING`.
- `doctor`'s read probe no longer claims to need `universe:read`. Roblox answers GET universe and GET place without `universe:read` or `universe.place:read`, so a pass now says it tested no scope, a 403 is no longer explained as a missing scope, and the note warning that a key without `universe:read` could not run the probe is gone.
- `doctor`'s scope coverage asks only for `universe:write` to sync universe fields and `universe.place:write` to sync place fields. It also required `universe:read` and `universe.place:read`, which Roblox does not check on those reads, and failed a key the sync works with.

## [0.1.2] - 2026-11-10

### Fixed

- The lockfile and a custom secret file are never emptied before they are written. They were truncated, then written, so a full disk or a run killed mid-write left the only record of the keys empty. The content now goes over the old one, the file is cut to length afterwards, and the old content is put back if a step fails.
- `update --all` records each key in the lockfile as soon as Roblox has taken its update. It saved once at the end, so a refusal or a "no" on a later key lost the record of the keys already updated, including where a migrated secret had moved to after its old file was deleted.
- `prune` stops on a config or a lockfile it cannot read. It read either as empty, which labelled every key of the project UNTRACKED and deleted it without its record, then overwrote the unreadable lockfile with an empty one. It also leaves no lockfile behind when no tracked key was deleted.
- `prune` and `delete --all` record each key as soon as Roblox has deleted it, like `update --all`. They saved once at the end, so a run stopped at the question about a secret file kept listing keys that no longer existed.
- `regenerate` writes the lockfile as soon as the new secret is in it, before it asks about an old secret file. It saved after that question, so a run stopped there, or a save that failed, lost a secret Roblox shows only once while the old one was already dead. A save that fails now prints the secret to store by hand.
- A lockfile that is there and cannot be looked at stops the command, and so does a config in that state for `prune`. Each read as absent, which is the same empty start an unreadable one used to give.

## [0.1.1] - 2026-11-09

### Security

- Release archives carry a build provenance attestation: `gh attestation verify <zip> --repo rbx-forge/rbx-apikey`. The build jobs run with read access only, and the publishing job alone may write.

### Fixed

- The help of `doctor --check-ip` pointed at `docs/doctor.md`, a path that does not exist beside an installed binary.

## [0.1.0] - 2026-11-08

First release as a standalone tool.

- Declares Open Cloud API keys in `rbxapikey.toml` and creates, updates, rotates, resolves and deletes them from it.
- `status` reconciles the file, the lockfile and what Roblox holds; `list --remote` and `prune` see the keys the account holds that this project never made.
- `create --stdout` and `regenerate --stdout` print the secret instead of storing it, for a pipe into a vault; the lockfile marks the key as held elsewhere, so `status` does not call it missing.
- `can-manage` says beforehand whether you can create keys for a universe.
- `doctor` diagnoses the loaded key: provenance, session, validity, IP allowlist, scope coverage for the configs in the directory, and one authenticated read.
- `scopes` and `catalog` inspect and refresh the embedded scope catalog, which a scheduled workflow keeps in step with Roblox.
- `schemas/rbxapikey.schema.json` for editor validation, regenerated by `rbx-apikey-schema`.
- The lockfile and a custom secret file are written readable by your account only: mode `0600` on Unix, inherited access removed on Windows, before the secret is written.
