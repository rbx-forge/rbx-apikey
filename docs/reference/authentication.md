# Authentication

Roblox's API key administration is not on Open Cloud, so `rbx-apikey` authenticates with a `.ROBLOSECURITY` session cookie, taken in this order:

1. `--cookie <value>` or `RBX_COOKIE`, with or without the `.ROBLOSECURITY=` prefix;
2. a signed-in local Roblox Studio, **only if you say yes**: an interactive run asks once, a run with nobody to ask refuses. `--auto-cookie` is the standing yes, `--no-auto-cookie` the standing no (it never overrides a cookie you passed).

When the Studio session is used, a line on stderr says so. The cookie is held in memory for the run and sent only to Roblox; it is never written anywhere.

`update`, `regenerate` and `delete` check the session before their first write, so an expired one changes nothing.

!!! warning "The account follows Studio"
    The keys listed and created belong to whoever is signed into Studio. Switching accounts there switches the account `rbx-apikey` acts on. `list --remote` prints the account first: check it.

## In CI

A runner has no Studio, and one that did must not borrow it, so every write (`create`, `update`, `regenerate`, `delete`, `prune`) needs `RBX_COOKIE` from a secret store there. Better: keep key management out of CI. The keys CI uses are minted on a workstation, and CI only reads them (`rbx-apikey resolve`, or a secret store).

## Global flags

| Flag | Env var | Description |
| --- | --- | --- |
| `--cookie <value>` | `RBX_COOKIE` | Session cookie |
| `--auto-cookie` / `--no-auto-cookie` | | Use a signed-in Studio without asking / never |
| `--api-key <key>`, `-a` | `RBX_API_KEY` | The key `doctor` diagnoses |
| `--env`, `-e`, `--universe-id`, `--place-id` | | Read by `can-manage` and `doctor` only; refused elsewhere |
| `--places <path>` | | Path to `rbxplace.toml` |
