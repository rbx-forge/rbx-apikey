# JSON output

`list`, `status` and `scopes show` take `--json`: one JSON document on stdout, and nothing else (notes and warnings go to stderr). Writes and `resolve` do not.

!!! warning "No secret, and no piece of one"
    No document carries a secret, a secret preview, or the path of a secret file. `list --remote --json` also omits key ids, since most keys on an account belong to other projects.

Optional fields are absent rather than `null`. `schema_version` is `1`.

## rbx-apikey list --json

```json
{
  "schema_version": 1,
  "sort": "name",
  "count": 2,
  "keys": [
    {
      "name": "deploy",
      "declared": true,
      "created": true,
      "id": "aaaaaaaa-bbbb-4ccc-8ddd-000000000001",
      "creator_id": "1234567890",
      "universe_ids": ["5544332211"],
      "expires_at": "2027-08-01T10:00:00.000Z",
      "days_until_expiry": 351,
      "secret_present": true,
      "secret_backend": "file",
      "secret_readable": true
    },
    { "name": "newkey", "declared": true, "created": false, "universe_ids": [] }
  ]
}
```

`secret_backend` is `lockfile`, `file`, or `external` for a key printed with `--stdout` (then `secret_present` is `false`: the secret is not held here). `secret_readable` is `false` when the secret's file is there and could not be read; `secret_present` is then `false` too, and the error, which names the file, is in the human form only. `declared` without `created` is pending; `created` without `declared` is an orphan. Lockfile fields are absent for a key never created. `days_until_expiry` is the whole days left, `0` being under a day. From the second of expiry it is negative: `-1` within the first day after, then `-1` minus the whole days since. It is absent without expiry. Not combinable with `--expiry-only`.

## rbx-apikey list --remote --json

```json
{
  "schema_version": 1,
  "owner": { "kind": "user", "id": "1234567890" },
  "totals": { "total": 12, "tracked": 1, "untracked": 11, "expired": 3, "disabled": 1 },
  "keys": [
    {
      "name": "myproject_viewer",
      "state": "active",
      "tracked": true,
      "tracked_as": "viewer",
      "created_time": "2026-08-03T09:15:00.123Z",
      "expiration_time": "2026-11-01T09:15:00.123Z",
      "days_until_expiry": 90
    },
    { "name": "otherproject_shop", "state": "active", "tracked": false }
  ],
  "missing_on_account": []
}
```

`state` is `active`, `expired` or `disabled`. `tracked_as` is the lockfile name. `missing_on_account` lists lockfile keys Roblox no longer has. `days_until_expiry` counts as in `list --json`.

## rbx-apikey status --json

```json
{
  "schema_version": 1,
  "remote": false,
  "count": 2,
  "issues": 1,
  "keys": [
    { "name": "deploy", "status": "HEALTHY", "healthy": true, "days_until_expiry": 351 },
    { "name": "newkey", "status": "PENDING", "healthy": false }
  ]
}
```

`healthy` is `true` only for `HEALTHY`. `status` takes the values listed under [`rbx-apikey status`](../commands.md#rbx-apikey-status), `SECRET_UNREADABLE` among them. `remote` says whether Roblox was asked (`ORPHAN_REMOTE` needs it). `days_until_expiry` counts as in `list --json`. A drift between the lockfile and `rbxplace.toml` fails before any document.

```sh
rbx-apikey status --json | jq -r '.keys[] | select(.healthy | not) | "\(.name): \(.status)"'
```

## rbx-apikey scopes show --json

```json
{ "schema_version": 1, "scope_type": "universe", "known": true, "catalog_version": "2026-05-13", "target_type": "universe", "operations": ["read", "write"] }
```

An unknown scope gives `"known": false`, exit `0`, without `target_type` and `operations`.
