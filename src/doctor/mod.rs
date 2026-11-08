//! `rbx-apikey doctor`: answer "why doesn't it work" before the user has to guess.
//!
//! Five checks, in the order a failure cascades: which credentials are active
//! and whether the session cookie is still one, whether that key is still
//! valid, what its IP allowlist says, whether it carries the scopes the tools
//! configured here need, and whether one real authenticated read succeeds. Each
//! is read-only. `doctor` never creates, updates or deletes anything.
//!
//! # The one check that is opt-in
//!
//! Comparing the key's CIDRs against the caller's public IP needs an address a
//! machine behind NAT cannot read off its own interfaces, so it means asking a
//! third party. `doctor` does not do that on its own initiative: without
//! `--check-ip` the allowlist is printed and explicitly not compared, and no
//! packet leaves for anyone but Roblox. With the flag, one request goes to the
//! service named in the output and in `docs/doctor.md`. See `ip`.
//!
//! # Exit status
//!
//! `0` when nothing is broken, `1` when a check failed. A check that could not
//! run is not a failure and does not change the status, but it is never
//! reported as a pass either, and the summary line says how many there were.

mod coverage;
mod ip;
mod probe;
mod report;
mod session;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use crate::diagnostics::{self, DeclaredKey, KeyFacts, KeyMatch};
use rbx_core::places::PlacesFile;
use rbx_core::session::Session;
use rbx_core::GlobalFlags;

use report::{Line, Report, Section};

#[derive(Args, Debug)]
pub struct DoctorCli {
    /// Diagnose this key from rbxapikey.toml instead of the one in the environment.
    ///
    /// Without it, `doctor` reports on whatever `RBX_API_KEY` holds, since that
    /// is the key the other commands will actually use. Name a key to ask about
    /// one this project declares but has not loaded.
    #[arg(long)]
    pub key: Option<String>,

    /// Skip the authenticated read probe.
    ///
    /// The probe is one `GET` against a universe and changes nothing, but it is
    /// the only check that leaves the machine on the key's behalf.
    #[arg(long = "no-probe")]
    pub no_probe: bool,

    /// Compare the key's IP allowlist against this machine's public address.
    ///
    /// Off by default, and the only thing in this tool that talks to anyone but
    /// Roblox: your public IP is not readable from behind NAT, so learning it
    /// means one request to an echo service, which therefore sees your address.
    /// The service is named in the output and in `docs/doctor.md`. A stale
    /// allowlist entry is refused as an opaque 401 that looks exactly like a
    /// wrong key, which is what this turns into a diagnosis.
    #[arg(long = "check-ip")]
    pub check_ip: bool,
}

/// Where the active API key came from. The point of step 1: `RBX_API_KEY` is
/// one variable shared by every tool in the suite, so "which key is loaded" is
/// a question people cannot answer by looking at their own config.
#[derive(Debug)]
enum KeyOrigin {
    Flag,
    EnvVar,
    Declared { name: String, origin: String },
}

impl KeyOrigin {
    fn describe(&self) -> String {
        match self {
            KeyOrigin::Flag => "--api-key".to_string(),
            KeyOrigin::EnvVar => "RBX_API_KEY (environment)".to_string(),
            KeyOrigin::Declared { name, origin } => {
                format!("\"{name}\" in rbxapikey.toml, secret from {origin}")
            }
        }
    }
}

#[derive(Debug)]
struct ActiveKey {
    secret: String,
    origin: KeyOrigin,
}

pub async fn run(cli: DoctorCli, global: &GlobalFlags) -> Result<()> {
    println!("{}", "rbx-apikey doctor".cyan().bold());

    let declared = diagnostics::declared_keys().unwrap_or_default();
    let mut r = Report::default();

    let active = resolve_active_key(&cli, global, &declared);
    // Resolved once for the whole run: `resolve_cookie` announces an
    // auto-detected cookie on stderr, and calling it per check would announce
    // it per check.
    let cookie = global.resolve_cookie();
    let mut credentials = credentials_section(
        global,
        &declared,
        &active,
        cli.key.as_deref(),
        cookie.as_deref(),
    );
    credentials.push(session_line(cookie.as_deref(), &session::SessionCheck::default()).await);
    r.push(credentials);

    let facts = fetch_facts(global, &active, &declared, cli.key.as_deref()).await;
    r.push(validity_section(&facts));
    r.push(allowlist_section(&cli, &facts, &ip::IpEcho::default()).await);
    r.push(coverage_section(&facts, std::path::Path::new(".")));
    r.push(probe_section(&cli, global, &active, &facts, &probe::Probe::default()).await);

    r.print();

    if r.failures() > 0 {
        // Through the error channel so the exit status is 1 without this crate
        // reaching for `std::process::exit`, which would skip the rest of the
        // binary's teardown. The message is deliberately terse: the report
        // above already said everything, and `main` prefixes this with
        // "Error:".
        anyhow::bail!("{} check(s) failed - see the report above", r.failures());
    }
    Ok(())
}

// ---------------- Step 1: which credential is active ----------------

fn resolve_active_key(
    cli: &DoctorCli,
    global: &GlobalFlags,
    declared: &[DeclaredKey],
) -> Option<ActiveKey> {
    // An explicit --key is an instruction about what to diagnose, so it wins
    // over whatever happens to be in the environment.
    if let Some(name) = cli.key.as_deref() {
        return declared.iter().find(|k| k.name == name).and_then(|k| {
            k.secret.as_ref().map(|secret| ActiveKey {
                secret: secret.clone(),
                origin: KeyOrigin::Declared {
                    name: k.name.clone(),
                    origin: k.secret_origin.clone(),
                },
            })
        });
    }

    let secret = global.api_key.as_deref()?;
    // clap merges the flag and the env var into one Option, so the only way to
    // tell them apart is to look the variable up again. Equal values mean the
    // env var supplied it (or both did, with the same value, which reads the
    // same to every other command).
    let origin = match std::env::var("RBX_API_KEY") {
        Ok(v) if v == secret => KeyOrigin::EnvVar,
        _ => KeyOrigin::Flag,
    };
    Some(ActiveKey {
        secret: secret.to_string(),
        origin,
    })
}

fn credentials_section(
    global: &GlobalFlags,
    declared: &[DeclaredKey],
    active: &Option<ActiveKey>,
    requested_key: Option<&str>,
    cookie: Option<&str>,
) -> Section {
    let mut s = Section::new("Credentials");

    match active {
        Some(key) => s.push(Line::ok("API key", key.origin.describe())),
        None => match requested_key {
            Some(name) if !declared.iter().any(|k| k.name == name) => s.push(Line::fail(
                "API key",
                format!("no key named \"{name}\" in rbxapikey.toml"),
                format!(
                    "Declared keys: {}. Run `rbx-apikey doctor` with no --key to report on \
                     whatever RBX_API_KEY holds.",
                    key_names(declared)
                ),
            )),
            Some(name) => s.push(Line::fail(
                "API key",
                format!("\"{name}\" has no readable secret"),
                format!(
                    "The key is declared but its secret is not on this machine. Create it \
                     with `rbx-apikey create {name}`, or pull the secret file it points at."
                ),
            )),
            None if declared.is_empty() => s.push(Line::fail(
                "API key",
                "none loaded, and this directory declares none",
                "Set one with `export RBX_API_KEY=<key>`, or declare keys in rbxapikey.toml \
                 and run `rbx-apikey create --all`. Every Open Cloud command needs a key.",
            )),
            None => s.push(Line::fail(
                "API key",
                "none loaded",
                format!(
                    "This directory declares {}. Load one with \
                     `export RBX_API_KEY=\"$(rbx-apikey resolve <name>)\"`.",
                    key_names(declared)
                ),
            )),
        },
    }

    // The cookie arrives already resolved: `run` does it once, because
    // `GlobalFlags::resolve_cookie` announces an auto-detected cookie on
    // stderr and calling it per check would announce it per check.
    match cookie {
        Some(_) if global.cookie.is_some() => s.push(Line::ok(
            "Studio cookie",
            "explicit (--cookie / RBX_COOKIE)",
        )),
        Some(_) => s.push(
            Line::warn("Studio cookie", "auto-detected from a local Studio install").with_action(
                "A session cookie is a full-account credential, more powerful than any scoped \
                 key. Pass --no-auto-cookie or set RBX_COOKIE= if you did not intend it.",
            ),
        ),
        None => s.push(Line::info(
            "Studio cookie",
            "none (auto-detection off or no Studio install)",
        )),
    }

    if diagnostics::config_file_present() {
        let created = declared.iter().filter(|k| k.is_created()).count();
        s.push(Line::info(
            "rbxapikey.toml",
            format!("{} key(s) declared, {} created", declared.len(), created),
        ));
    } else {
        s.push(Line::info("rbxapikey.toml", "not in this directory"));
    }

    s
}

/// Whether the cookie is still a session, as the last line of step 1.
///
/// It sits in Credentials rather than in a section of its own because it
/// answers the same question the two lines above it do (what am I
/// authenticated as) and reads next to them: where the cookie came from, then
/// whether it still works.
///
/// The three non-passing outcomes are deliberately three different statuses.
/// A refusal is a failure with a remedy. No cookie is a check that could not
/// run, not a problem: most Open Cloud work never needs one. An unanswered check
/// is also skipped, never a failure, for the same reason the IP lookup is:
/// reporting "your session expired" because a service was unreachable sends
/// somebody to re-authenticate a session nobody has shown to be dead.
///
/// `check` is a parameter so the whole line can be asserted against a mock,
/// including the refusal, which is the one that matters and the one that
/// cannot be produced on purpose against the real host.
async fn session_line(cookie: Option<&str>, check: &session::SessionCheck) -> Line {
    let Some(cookie) = cookie else {
        return Line::skipped(
            "session",
            "there is no cookie to check. That is only a problem for the commands that need \
             one: creating, updating, rotating, listing or deleting keys on the account.",
        );
    };

    match check.ask(cookie).await {
        Session::Valid(account) => {
            Line::ok("session", format!("live: signed in as {}", account.label()))
        }
        Session::Refused => Line::fail(
            "session",
            "expired or revoked: Roblox refused this cookie",
            "Every command that needs the cookie will be refused until it is renewed: sign in \
             to Roblox Studio again, or supply a fresh one with --cookie / RBX_COOKIE.",
        ),
        // Not a failure: `export RBX_COOKIE=` is the documented way to turn
        // the cookie off for a whole shell, so an empty one is usually somebody
        // getting what they asked for. It is still never a pass, because the
        // commands that need a cookie will refuse.
        Session::Empty => Line::skipped(
            "session",
            "the cookie is empty. RBX_COOKIE= (or --cookie \"\") means \"no cookie\", which is \
             how one deliberately turns it off; the commands that need one will refuse until it \
             holds a value.",
        ),
        Session::Unknown(why) => Line::skipped(
            "session",
            format!(
                "{why}. That is this check not answering, not a refusal: it does not mean the \
                 session is over."
            ),
        ),
    }
}

fn key_names(declared: &[DeclaredKey]) -> String {
    if declared.is_empty() {
        return "no keys".to_string();
    }
    declared
        .iter()
        .map(|k| k.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------- Steps 2-4: what Roblox holds for the key ----------------

/// The key's stored configuration, or why it could not be read.
enum Facts {
    Known(Box<KeyFacts>),
    /// Could not be read, with the reason phrased as what would make it work.
    Unavailable(String),
}

async fn fetch_facts(
    global: &GlobalFlags,
    active: &Option<ActiveKey>,
    declared: &[DeclaredKey],
    requested_key: Option<&str>,
) -> Facts {
    // A key named but not held locally still has a cloud_auth_id in the
    // lockfile, which identifies it without a secret.
    if let Some(name) = requested_key {
        if let Some(id) = declared
            .iter()
            .find(|k| k.name == name)
            .and_then(|k| k.cloud_auth_id.as_deref())
        {
            return match diagnostics::facts_for_id(global, id).await {
                Ok(Some(facts)) => Facts::Known(Box::new(facts)),
                Ok(None) => Facts::Unavailable(format!(
                    "\"{name}\" is in the lockfile but the signed-in account does not hold it. \
                     It was deleted on Roblox, or the cookie signs in as a different account."
                )),
                Err(e) => Facts::Unavailable(format!("{e}")),
            };
        }
    }

    let Some(active) = active else {
        return Facts::Unavailable(
            "no API key to ask about. Load one, or name a declared key with --key <name>."
                .to_string(),
        );
    };

    match diagnostics::identify_secret(global, &active.secret).await {
        Ok(KeyMatch::One(facts)) => Facts::Known(facts),
        Ok(KeyMatch::None) => Facts::Unavailable(
            "the loaded key is not among those the signed-in account holds, so its stored \
             configuration cannot be read. That can mean it belongs to another account or to \
             a group, that it was deleted, or that it was never tracked here. Check you are \
             signed in to the account that owns it."
                .to_string(),
        ),
        Ok(KeyMatch::Ambiguous(names)) => Facts::Unavailable(format!(
            "the loaded key's prefix matches more than one key on the account ({}), so it \
             cannot be identified. Regenerate one of them to tell them apart.",
            names.join(", ")
        )),
        Err(e) => Facts::Unavailable(format!("{e}")),
    }
}

fn validity_section(facts: &Facts) -> Section {
    let mut s = Section::new("Key validity");
    let facts = match facts {
        Facts::Known(f) => f,
        Facts::Unavailable(why) => {
            s.push(Line::skipped("stored configuration", why.clone()));
            return s;
        }
    };

    s.push(Line::info(
        "identified as",
        match &facts.tracked_as {
            Some(name) => format!(
                "\"{}\" (this project), {} on Roblox",
                name, facts.remote_name
            ),
            None => format!(
                "{}, not tracked by this project's lockfile",
                facts.remote_name
            ),
        },
    ));

    if facts.enabled {
        s.push(Line::ok("enabled", "yes"));
    } else {
        s.push(Line::fail(
            "enabled",
            "no: the key is disabled on Roblox",
            "Every call with this key fails until it is re-enabled. Turn it back on in the \
             Creator Hub, or set `enabled = true` on the key in rbxapikey.toml and run \
             `rbx-apikey update <key>`.",
        ));
    }

    let expiry = diagnostics::expiry_text(facts.expires_at.as_deref());
    if facts.is_expired() {
        s.push(Line::fail(
            "expiry",
            format!(
                "{} ({})",
                facts.expires_at.as_deref().unwrap_or("?"),
                expiry
            ),
            "An expired key is refused with the same opaque error as a wrong one. Rotate it \
             with `rbx-apikey regenerate <key>`, then reload it into RBX_API_KEY.",
        ));
    } else {
        let detail = match facts.expires_at.as_deref() {
            Some(at) => format!("{at} ({expiry})"),
            None => expiry,
        };
        // Two weeks: long enough to notice before a deploy, short enough not to
        // nag for a year.
        match facts.days_left {
            Some(d) if d <= 14 => s.push(Line::warn("expiry", detail).with_action(
                "Rotate before it lapses: `rbx-apikey regenerate <key>`, then reload \
                 RBX_API_KEY.",
            )),
            _ => s.push(Line::ok("expiry", detail)),
        }
    }

    s
}

/// `echo` is a parameter for the same reason `probe` is: the comparison this
/// section makes is the whole point of `--check-ip`, and it can only be
/// asserted end to end if the lookup half can be answered by a mock. `run`
/// passes a default one, which is the real echo service.
///
/// Nothing here leaves the machine unless `--check-ip` was passed *and* there
/// is an allowlist that could actually refuse somebody. An empty allowlist, an
/// allowlist containing `0.0.0.0/0`, and a key whose configuration could not be
/// read all answer the question without asking anyone.
async fn allowlist_section(cli: &DoctorCli, facts: &Facts, echo: &ip::IpEcho) -> Section {
    let mut s = Section::new("IP allowlist");
    let facts = match facts {
        Facts::Known(f) => f,
        Facts::Unavailable(_) => {
            s.push(Line::skipped(
                "allowed CIDRs",
                "the key's stored configuration could not be read: see above",
            ));
            return s;
        }
    };

    if facts.allowed_cidrs.is_empty() {
        s.push(Line::info("allowed CIDRs", "none recorded"));
        return s;
    }

    let list = facts.allowed_cidrs.join(", ");
    if facts.allowed_cidrs.iter().any(|c| c == "0.0.0.0/0") {
        s.push(Line::info(
            "allowed CIDRs",
            format!("{list}: every IP, so the allowlist cannot be the cause of a refusal"),
        ));
        return s;
    }

    if !cli.check_ip {
        s.push(Line::info("allowed CIDRs", list).with_action(format!(
            "Not compared against this machine's public IP. Doing that means asking a \
             third-party echo service ({}) for an address this machine cannot read off its \
             own interfaces, so it is opt-in: re-run with `--check-ip`. A stale entry here \
             fails as an opaque 401 that looks exactly like a wrong key, so check it first \
             when a call that should work does not. See https://rbx-forge.github.io/rbx-apikey/doctor/",
            echo.host()
        )));
        return s;
    }

    s.push(Line::info("allowed CIDRs", list));
    let lookup = echo.resolve().await;
    let addr = match lookup {
        ip::IpLookup::Found(addr) => {
            // Named on the same line as the answer, not only in the docs:
            // nobody should find out afterwards that a third party was told
            // where they are.
            s.push(Line::info(
                "public IP",
                format!("{addr}: asked {}, which therefore saw it", echo.host()),
            ));
            addr
        }
        ip::IpLookup::Unavailable(why) => {
            s.push(Line::skipped(
                "public IP",
                format!(
                    "{} was asked and {why}. The allowlist above was not compared: that is \
                     this check not answering, not a refusal, and it does not mean the key \
                     is locked out.",
                    echo.host()
                ),
            ));
            return s;
        }
    };

    match ip::compare(&facts.allowed_cidrs, addr) {
        ip::Verdict::Inside(entry) => s.push(Line::ok(
            "this machine",
            format!("{addr} is inside {entry}"),
        )),
        ip::Verdict::Outside => {
            // The host route for the address's own family: a `/32` pasted next
            // to an IPv6 address is advice that does not apply.
            let host_route = if addr.is_ipv4() { 32 } else { 128 };
            s.push(Line::fail(
                "this machine",
                format!("{addr} is in none of the allowed CIDRs"),
                format!(
                    "Every call with this key is refused as an opaque 401 until the allowlist \
                     covers this address. Add {addr}/{host_route} to the key's `allowed_cidrs` \
                     in rbxapikey.toml and run `rbx-apikey update <key>`, or pass `--no-ip` to \
                     that command to drop the restriction. A home connection's address usually \
                     changes, so a host route written today is next month's 401."
                ),
            ))
        }
        ip::Verdict::Inconclusive(why) => s.push(Line::skipped("this machine", why)),
    }

    s
}

/// `dir` is a parameter rather than a hardcoded `.` so the coverage rules can
/// be tested against a directory laid out on purpose. Callers pass `.`: the
/// config files a tool reads are the ones next to where it was run.
fn coverage_section(facts: &Facts, dir: &std::path::Path) -> Section {
    let mut s = Section::new("Scope coverage");

    let present = coverage::present_in(dir);
    if present.is_empty() {
        // Named from the table rather than restated here: a config file added
        // to `REQUIREMENTS` and forgotten in this sentence would be a tool
        // `doctor` looks for and then denies knowing about.
        let known: Vec<&str> = coverage::REQUIREMENTS
            .iter()
            .map(|r| r.config_file)
            .collect();
        s.push(Line::info(
            "config files",
            format!("none of {} are here", known.join(", ")),
        ));
        return s;
    }

    let facts = match facts {
        Facts::Known(f) => f,
        Facts::Unavailable(_) => {
            s.push(Line::skipped(
                "scopes",
                "the key's stored configuration could not be read: see above",
            ));
            return s;
        }
    };

    for tool in present {
        // Which config file put these lines here. Without it, a reader who has
        // never touched the shop sees four failures with no explanation of why
        // they are being asked about badges at all.
        s.push(Line::info(
            tool.tool,
            format!("{} is here", tool.config_file),
        ));
        for op in tool.operations {
            let missing: Vec<_> = op
                .scopes
                .iter()
                .filter(|(scope_type, operation)| !facts.grants(scope_type, operation))
                .copied()
                .collect();

            if missing.is_empty() {
                s.push(Line::ok(op.what, "covered"));
            } else {
                s.push(Line::fail(
                    op.what,
                    format!("missing {}", coverage::spell_scopes(&missing)),
                    format!(
                        "{} is here, so this is a call you can expect to make. Add \
                         {} to the key's `scopes` in rbxapikey.toml and run \
                         `rbx-apikey update <key>`.",
                        tool.config_file,
                        coverage::spell_scopes(&missing)
                    ),
                ));
            }
        }
    }

    s
}

// ---------------- Step 5: the read probe ----------------

/// `probe` is a parameter rather than a value built here so the whole section
/// (the target line, the outcome, and the explanation attached to a refusal)
/// can run against a mock server. `run` passes a default one, which is the real
/// Open Cloud host.
async fn probe_section(
    cli: &DoctorCli,
    global: &GlobalFlags,
    active: &Option<ActiveKey>,
    facts: &Facts,
    probe: &probe::Probe,
) -> Section {
    let mut s = Section::new("Read probe");

    if cli.no_probe {
        s.push(Line::skipped("authenticated read", "--no-probe was passed"));
        return s;
    }
    let Some(active) = active else {
        s.push(Line::skipped(
            "authenticated read",
            "no API key is loaded, so there is nothing to probe with",
        ));
        return s;
    };
    let Some((universe_id, how)) = probe_target(global) else {
        s.push(Line::skipped(
            "authenticated read",
            "no universe to read. Pass --env <name> or --universe-id <id>, or add an env to \
             rbxplace.toml.",
        ));
        return s;
    };

    // Stated before the result: a reader who disagrees with the target needs to
    // know which one was used before they interpret the answer.
    s.push(Line::info(
        "target",
        format!("universe {universe_id} ({how})"),
    ));

    match probe.read_universe(&active.secret, universe_id).await {
        probe::ProbeOutcome::Ok { universe_name } => s.push(Line::ok(
            "authenticated read",
            match universe_name {
                Some(name) => format!("200: read \"{name}\""),
                None => "200".to_string(),
            },
        )),
        probe::ProbeOutcome::Refused { status, message } => {
            let detail = if message.is_empty() {
                status.to_string()
            } else {
                format!("{}: {}", status.as_u16(), message)
            };
            s.push(Line::fail(
                "authenticated read",
                detail,
                probe::explain(status),
            ));
        }
        probe::ProbeOutcome::Unreachable(e) => s.push(Line::fail(
            "authenticated read",
            format!("no answer: {e}"),
            "Roblox was not reached at all. This is the network between here and \
             apis.roblox.com, not the key.",
        )),
    }

    // A refusal is far easier to read next to what the key is allowed to do.
    if let Facts::Known(f) = facts {
        if !f.grants("universe", "read") {
            s.push(Line::warn(
                "note",
                "the key does not carry universe:read, which this probe needs",
            ));
        }
    }

    s
}

/// The universe to probe, and how it was chosen.
///
/// `single_universe` covers the explicit forms. The extra step is a
/// `rbxplace.toml` with exactly one env: a normal command refuses to guess
/// there, but `doctor` is being asked what is wrong, and "I did not check
/// because you did not say which of your one envs to use" is not an answer.
/// Which env was picked is printed either way.
fn probe_target(global: &GlobalFlags) -> Option<(u64, String)> {
    if let Ok(universe_id) = global.single_universe() {
        let how = match global.env.as_deref() {
            Some(env) => format!("--env {env}"),
            None => "--universe-id".to_string(),
        };
        return Some((universe_id, how));
    }

    let places = PlacesFile::load(&global.places).ok()?;
    let names = places.env_names();
    let [only] = names.as_slice() else {
        return None;
    };
    let env = places.get(only).ok()?;
    Some((
        env.universe_id,
        format!("the only env in {}", global.places.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(name: &str, created: bool, secret: Option<&str>) -> DeclaredKey {
        DeclaredKey {
            name: name.to_string(),
            cloud_auth_id: created.then(|| "id-1".to_string()),
            secret_origin: "rbxapikey.lock.toml".to_string(),
            secret: secret.map(|s| s.to_string()),
        }
    }

    fn flags() -> GlobalFlags {
        GlobalFlags {
            api_key: None,
            cookie: None,
            no_auto_cookie: true,
            auto_cookie: false,
            env: None,
            place: None,
            places: "rbxplace.toml".into(),
            universe_id: None,
            place_id: Vec::new(),
        }
    }

    fn cli(key: Option<&str>) -> DoctorCli {
        DoctorCli {
            key: key.map(|k| k.to_string()),
            no_probe: true,
            check_ip: false,
        }
    }

    #[test]
    fn an_explicit_key_wins_over_the_environment() {
        let mut global = flags();
        global.api_key = Some("from-env".into());
        let declared = vec![declared("deploy", true, Some("from-file"))];

        let active = resolve_active_key(&cli(Some("deploy")), &global, &declared).unwrap();

        assert_eq!(active.secret, "from-file");
        assert!(matches!(active.origin, KeyOrigin::Declared { .. }));
    }

    #[test]
    fn a_named_key_with_no_readable_secret_resolves_to_nothing() {
        let declared = vec![declared("deploy", true, None)];
        assert!(resolve_active_key(&cli(Some("deploy")), &flags(), &declared).is_none());
    }

    #[test]
    fn a_named_key_that_is_not_declared_resolves_to_nothing() {
        let declared = vec![declared("deploy", true, Some("s"))];
        assert!(resolve_active_key(&cli(Some("other")), &flags(), &declared).is_none());
    }

    /// The failure is reported against the name that was asked for, listing
    /// what is actually there: the whole point of asking.
    #[test]
    fn naming_an_undeclared_key_fails_with_the_names_that_do_exist() {
        let declared = vec![declared("deploy", true, Some("s"))];
        let section = credentials_section(&flags(), &declared, &None, Some("typo"), None);

        let line = &section.lines[0];
        assert_eq!(line.status, report::Status::Fail);
        assert!(line.detail.contains("typo"));
        assert!(line.action.as_ref().unwrap().contains("deploy"));
    }

    #[test]
    fn no_key_anywhere_says_how_to_get_one() {
        let section = credentials_section(&flags(), &[], &None, None, None);
        let action = section.lines[0].action.as_ref().unwrap();
        assert!(action.contains("RBX_API_KEY"));
        assert!(action.contains("rbxapikey.toml"));
    }

    /// The interesting half of step 1: a key is loaded, and the point is saying
    /// where it came from, because RBX_API_KEY is shared by every tool.
    #[test]
    fn a_loaded_key_reports_where_it_came_from() {
        let active = Some(ActiveKey {
            secret: "s".into(),
            origin: KeyOrigin::EnvVar,
        });
        let section = credentials_section(&flags(), &[], &active, None, None);

        assert_eq!(section.lines[0].status, report::Status::Ok);
        assert!(section.lines[0].detail.contains("RBX_API_KEY"));
    }

    #[test]
    fn a_key_whose_configuration_could_not_be_read_is_skipped_not_passed() {
        let section = validity_section(&Facts::Unavailable("no cookie".into()));
        assert_eq!(section.lines[0].status, report::Status::Skipped);
    }

    fn facts_with(enabled: bool, days_left: Option<i64>, scopes: Vec<(&str, &str)>) -> Facts {
        Facts::Known(Box::new(KeyFacts {
            remote_name: "prod_deploy".into(),
            tracked_as: Some("deploy".into()),
            enabled,
            expires_at: days_left.map(|_| "2027-01-01T00:00:00Z".to_string()),
            days_left,
            allowed_cidrs: vec![],
            scopes: scopes
                .into_iter()
                .map(|(t, op)| crate::scope_builder::ScopeDef {
                    scope_type: t.to_string(),
                    target_parts: vec!["1".into()],
                    operations: vec![op.to_string()],
                })
                .collect(),
        }))
    }

    #[test]
    fn a_disabled_key_fails_with_something_to_do_about_it() {
        let section = validity_section(&facts_with(false, None, vec![]));
        let line = section.lines.iter().find(|l| l.label == "enabled").unwrap();
        assert_eq!(line.status, report::Status::Fail);
        assert!(line.action.as_ref().unwrap().contains("rbx-apikey update"));
    }

    #[test]
    fn an_expired_key_fails_and_a_live_one_does_not() {
        let expired = validity_section(&facts_with(true, Some(-1), vec![]));
        let live = validity_section(&facts_with(true, Some(300), vec![]));

        assert!(expired
            .lines
            .iter()
            .any(|l| l.status == report::Status::Fail));
        assert!(!live.lines.iter().any(|l| l.status == report::Status::Fail));
    }

    /// A key expiring next week is not broken, so it must not fail the run,
    /// but saying nothing is how a deploy breaks on a Monday.
    #[test]
    fn a_key_expiring_soon_warns_without_failing() {
        let section = validity_section(&facts_with(true, Some(3), vec![]));
        let expiry = section.lines.iter().find(|l| l.label == "expiry").unwrap();

        assert_eq!(expiry.status, report::Status::Warn);
        assert!(expiry.action.is_some());
    }

    /// The rest of step 1: whether the cookie is still a session.
    ///
    /// The refusal is the line this exists for, and it is the one that cannot
    /// be produced against the real host on purpose: hence the mock. The other
    /// three assert the rule that runs through the whole command: only a real
    /// refusal fails, and nothing that was not checked is reported as a pass.
    mod session_check {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn users_answering(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/users/authenticated"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        async fn line_for(cookie: Option<&str>, server: &MockServer) -> Line {
            session_line(
                cookie,
                &session::SessionCheck::default().with_base_url(server.uri()),
            )
            .await
        }

        #[tokio::test]
        async fn a_live_session_passes_and_names_the_account() {
            let server = users_answering(200, r#"{"id":156,"name":"builderman"}"#).await;

            let line = line_for(Some("doctor-live"), &server).await;

            assert_eq!(line.status, report::Status::Ok);
            assert!(line.detail.contains("builderman"), "got {}", line.detail);
            assert!(line.detail.contains("156"), "got {}", line.detail);
        }

        /// The question the command exists to answer, when the answer is no.
        /// It fails (an expired cookie is broken, not merely notable) and the
        /// remedy names both ways to renew rather than quoting a status.
        #[tokio::test]
        async fn a_refused_session_fails_with_the_way_to_renew_it() {
            let server = users_answering(401, "{}").await;

            let line = line_for(Some("doctor-expired"), &server).await;

            assert_eq!(line.status, report::Status::Fail);
            assert!(line.detail.contains("expired"), "got {}", line.detail);
            let action = line.action.as_ref().expect("a failure carries its remedy");
            assert!(action.contains("Roblox Studio"), "got {action}");
            assert!(action.contains("RBX_COOKIE"), "got {action}");
            assert!(!action.contains("401"), "got {action}");
        }

        /// The same rule as the IP echo: a service that did not answer is a
        /// check that did not run. Failing here would send somebody to
        /// re-authenticate a session nobody has shown to be dead.
        #[tokio::test]
        async fn an_unreachable_service_is_skipped_rather_than_failed() {
            let line = session_line(
                Some("doctor-offline"),
                &session::SessionCheck::default().with_base_url("http://127.0.0.1:1"),
            )
            .await;

            assert_eq!(line.status, report::Status::Skipped);
            let why = line.action.as_ref().unwrap();
            assert!(why.contains("not a refusal"), "got {why}");
        }

        /// No cookie is not a problem: most commands never need one. It is
        /// still not a pass: nothing was checked.
        #[tokio::test]
        async fn no_cookie_is_skipped_and_asks_nobody() {
            let server = users_answering(401, "{}").await;

            let line = line_for(None, &server).await;

            assert_eq!(line.status, report::Status::Skipped);
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        /// `export RBX_COOKIE=` is the documented way to turn the cookie off
        /// for a whole shell, so reporting it as a failure would give `doctor`
        /// exit status 1 for a configuration somebody chose on purpose.
        #[tokio::test]
        async fn an_empty_cookie_is_not_reported_as_a_broken_session() {
            let server = users_answering(401, "{}").await;

            let line = line_for(Some(""), &server).await;

            assert_eq!(line.status, report::Status::Skipped);
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }

    /// Step 3, both halves.
    ///
    /// The default path must reach no network at all, and the `--check-ip`
    /// path must turn what is otherwise an opaque 401 into a named cause,
    /// without ever reporting an unanswered lookup as a mismatch.
    mod ip_allowlist {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn facts_allowing(cidrs: &[&str]) -> Facts {
            let mut facts = facts_with(true, None, vec![]);
            if let Facts::Known(f) = &mut facts {
                f.allowed_cidrs = cidrs.iter().map(|c| c.to_string()).collect();
            }
            facts
        }

        fn checking_ip(check_ip: bool) -> DoctorCli {
            DoctorCli {
                key: None,
                no_probe: true,
                check_ip,
            }
        }

        /// An echo service answering with `body`, plus a counter the caller can
        /// read to prove whether it was contacted at all.
        async fn echoing(body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        async fn section(cli: &DoctorCli, facts: &Facts, echo: ip::IpEcho) -> Section {
            allowlist_section(cli, facts, &echo).await
        }

        fn line<'a>(section: &'a Section, label: &str) -> &'a Line {
            section
                .lines
                .iter()
                .find(|l| l.label == label)
                .unwrap_or_else(|| panic!("no {label:?} line in {:?}", section.lines))
        }

        #[tokio::test]
        async fn an_open_allowlist_is_reported_as_unable_to_cause_a_refusal() {
            let section = section(
                &checking_ip(false),
                &facts_allowing(&["0.0.0.0/0"]),
                ip::IpEcho::default(),
            )
            .await;
            assert!(section.lines[0].detail.contains("every IP"));
            assert!(section.lines[0].action.is_none());
        }

        /// Without the flag the comparison is not made, and the line has to say
        /// so: a printed allowlist with no caveat reads as a passed check.
        #[tokio::test]
        async fn a_restricted_allowlist_says_the_comparison_was_not_made() {
            let server = echoing("203.0.113.9").await;
            let section = section(
                &checking_ip(false),
                &facts_allowing(&["203.0.113.4/32"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;

            let action = section.lines[0].action.as_ref().unwrap();
            assert!(action.contains("public IP"), "got {action}");
            assert!(action.contains("401"), "got {action}");
            assert!(action.contains("--check-ip"), "got {action}");
            // The whole promise of the default: nothing left the machine.
            assert!(
                server.received_requests().await.unwrap().is_empty(),
                "the echo service was contacted without --check-ip"
            );
        }

        /// An allowlist that cannot refuse anybody is answered without telling
        /// a third party anything, flag or no flag.
        #[tokio::test]
        async fn an_open_allowlist_asks_nobody_even_with_the_flag() {
            let server = echoing("203.0.113.9").await;
            section(
                &checking_ip(true),
                &facts_allowing(&["0.0.0.0/0"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_unreadable_key_asks_nobody_even_with_the_flag() {
            let server = echoing("203.0.113.9").await;
            let section = section(
                &checking_ip(true),
                &Facts::Unavailable("no cookie".into()),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;
            assert_eq!(section.lines[0].status, report::Status::Skipped);
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn an_address_inside_the_allowlist_passes_and_names_the_service() {
            let server = echoing("203.0.113.9").await;
            let section = section(
                &checking_ip(true),
                &facts_allowing(&["198.51.100.0/24", "203.0.113.0/24"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;

            let resolved = line(&section, "public IP");
            assert!(resolved.detail.contains("203.0.113.9"));
            // Named where the user reads the answer, not only in the docs.
            assert!(
                resolved.detail.contains(&server.uri()),
                "got {}",
                resolved.detail
            );

            let verdict = line(&section, "this machine");
            assert_eq!(verdict.status, report::Status::Ok);
            assert!(verdict.detail.contains("203.0.113.0/24"));
        }

        /// The reason the flag exists: this is the 401 nobody guesses, turned
        /// into a line that says what to add and where.
        #[tokio::test]
        async fn an_address_outside_the_allowlist_fails_with_the_entry_to_add() {
            let server = echoing("203.0.113.9\n").await;
            let section = section(
                &checking_ip(true),
                &facts_allowing(&["198.51.100.0/24"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;

            let verdict = line(&section, "this machine");
            assert_eq!(verdict.status, report::Status::Fail);
            let action = verdict
                .action
                .as_ref()
                .expect("a failure carries its remedy");
            assert!(action.contains("203.0.113.9/32"), "got {action}");
            assert!(action.contains("rbx-apikey update"), "got {action}");
        }

        /// The rule that matters most: an echo service that did not answer is
        /// a check that could not run. Reporting it as a mismatch would send
        /// somebody editing a key that is fine.
        #[tokio::test]
        async fn an_unreachable_echo_service_is_skipped_rather_than_failed() {
            let section = section(
                &checking_ip(true),
                &facts_allowing(&["198.51.100.0/24"]),
                ip::IpEcho::default().with_base_url("http://127.0.0.1:1"),
            )
            .await;

            let resolved = line(&section, "public IP");
            assert_eq!(resolved.status, report::Status::Skipped);
            assert!(section
                .lines
                .iter()
                .all(|l| l.status != report::Status::Fail));
            let why = resolved.action.as_ref().unwrap();
            assert!(why.contains("not a refusal"), "got {why}");
        }

        /// A key with no allowlist cannot be refused on one, so there is
        /// nothing to compare and nobody to ask.
        #[tokio::test]
        async fn a_key_with_no_allowlist_at_all_asks_nobody() {
            let server = echoing("203.0.113.9").await;
            let section = section(
                &checking_ip(true),
                &facts_allowing(&[]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;

            assert_eq!(section.lines.len(), 1);
            assert_eq!(section.lines[0].status, report::Status::Info);
            assert!(section.lines[0].detail.contains("none recorded"));
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        /// A v6 answer against a v4-only allowlist is not a lockout: Roblox may
        /// well see this machine at a v4 address the list does cover.
        #[tokio::test]
        async fn an_address_of_a_family_the_allowlist_does_not_use_is_skipped() {
            let server = echoing("2001:db8::1").await;
            let section = section(
                &checking_ip(true),
                &facts_allowing(&["198.51.100.0/24"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;

            let verdict = line(&section, "this machine");
            assert_eq!(verdict.status, report::Status::Skipped);
        }

        /// One lookup per run, not one per entry.
        #[tokio::test]
        async fn the_echo_service_is_asked_exactly_once() {
            let server = echoing("203.0.113.9").await;
            section(
                &checking_ip(true),
                &facts_allowing(&["198.51.100.0/24", "203.0.113.0/24", "10.0.0.0/8"]),
                ip::IpEcho::default().with_base_url(server.uri()),
            )
            .await;
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[test]
    fn a_directory_with_no_tool_configs_reports_nothing_to_cover() {
        let dir = tempfile::tempdir().unwrap();
        let section = coverage_section(&facts_with(true, None, vec![]), dir.path());

        assert_eq!(section.lines.len(), 1);
        assert_eq!(section.lines[0].status, report::Status::Info);
    }

    /// Step 4 in one test: a config file is here, the key covers one of its
    /// operations and not the other, and the gap names the scope to add.
    #[test]
    fn a_missing_scope_is_named_against_the_config_file_that_needs_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rbxconfig.toml"), "").unwrap();
        let facts = facts_with(true, None, vec![("universe", "read")]);

        let section = coverage_section(&facts, dir.path());

        let read = section
            .lines
            .iter()
            .find(|l| l.label.contains("get / list"))
            .unwrap();
        assert_eq!(read.status, report::Status::Ok);

        let write = section
            .lines
            .iter()
            .find(|l| l.label.contains("sync / rollback"))
            .unwrap();
        assert_eq!(write.status, report::Status::Fail);
        assert!(write.detail.contains("universe:write"));
        let action = write.action.as_ref().unwrap();
        assert!(action.contains("rbxconfig.toml"));
        assert!(action.contains("rbx-apikey update"));
    }

    /// A file that is not here must not produce failures: telling somebody
    /// their key cannot manage badges when they have no rbxshop.toml is noise
    /// that buries the finding that matters.
    #[test]
    fn a_tool_that_is_not_configured_here_is_not_reported_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rbxconfig.toml"), "").unwrap();

        let section = coverage_section(&facts_with(true, None, vec![]), dir.path());

        assert!(section.lines.iter().all(|l| !l.label.contains("badges")));
    }

    #[test]
    fn coverage_is_skipped_rather_than_passed_when_the_key_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rbxshop.toml"), "").unwrap();

        let section = coverage_section(&Facts::Unavailable("no cookie".into()), dir.path());

        assert_eq!(section.lines.len(), 1);
        assert_eq!(section.lines[0].status, report::Status::Skipped);
    }

    #[test]
    fn the_probe_target_prefers_an_explicit_universe_id() {
        let mut global = flags();
        global.universe_id = Some(4242);
        let (universe_id, how) = probe_target(&global).unwrap();
        assert_eq!(universe_id, 4242);
        assert!(how.contains("--universe-id"));
    }

    #[test]
    fn there_is_no_probe_target_without_a_places_file() {
        let mut global = flags();
        global.places = "definitely/not/here.toml".into();
        assert!(probe_target(&global).is_none());
    }

    /// Step 5 against a real HTTP exchange.
    ///
    /// The explanation attached to a refusal is what the command is for, and
    /// until the probe took an injectable host it could only be checked by
    /// calling `probe::explain` directly, which proves the constants differ,
    /// not that the right one reaches the reader for the status Roblox actually
    /// sent. These assert the rendered line.
    mod read_probe {
        use super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const UNIVERSE: u64 = 5544332211;

        /// A mock answering the probe's one request, and the flags that point
        /// the probe at it.
        async fn answering(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/cloud/v2/universes/{UNIVERSE}")))
                .and(header("x-api-key", "test-key"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        async fn section(server: &MockServer) -> Section {
            let mut global = flags();
            global.universe_id = Some(UNIVERSE);
            let cli = DoctorCli {
                key: None,
                no_probe: false,
                check_ip: false,
            };
            let active = Some(ActiveKey {
                secret: "test-key".into(),
                origin: KeyOrigin::EnvVar,
            });
            let probe = probe::Probe::default().with_base_url(server.uri());

            probe_section(
                &cli,
                &global,
                &active,
                &Facts::Unavailable("n/a".into()),
                &probe,
            )
            .await
        }

        fn read_line(section: &Section) -> &Line {
            section
                .lines
                .iter()
                .find(|l| l.label == "authenticated read")
                .expect("the section always reports on the read")
        }

        #[tokio::test]
        async fn a_200_passes_and_quotes_the_name_it_read() {
            let server = answering(200, r#"{"displayName":"My Game"}"#).await;
            let section = section(&server).await;
            let line = read_line(&section);

            assert_eq!(line.status, report::Status::Ok);
            assert!(line.detail.contains("My Game"), "got {}", line.detail);
            assert!(line.action.is_none());
        }

        /// The one nobody guesses. A 401 that only said "unauthorized" would
        /// send the reader to rotate a key whose secret is fine.
        #[tokio::test]
        async fn a_401_fails_and_names_the_ip_allowlist() {
            let server = answering(
                401,
                r#"{"errors":[{"code":0,"message":"Invalid API Key"}]}"#,
            )
            .await;
            let section = section(&server).await;
            let line = read_line(&section);

            assert_eq!(line.status, report::Status::Fail);
            assert!(line.detail.contains("401"), "got {}", line.detail);
            assert!(
                line.detail.contains("Invalid API Key"),
                "got {}",
                line.detail
            );

            let action = line.action.as_ref().expect("a failure carries its remedy");
            assert!(action.contains("IP allowlist"), "got {action}");
            assert!(action.contains("secret is wrong"), "got {action}");
            // The 403 advice would be actively wrong here.
            assert!(!action.contains("widen the key"), "got {action}");
        }

        /// 401 and 403 are indistinguishable from inside a script and mean
        /// different things, so the wrong one reaching the reader is the
        /// failure mode this crate exists to prevent.
        #[tokio::test]
        async fn a_403_fails_and_sends_the_reader_to_the_scopes() {
            let server = answering(
                403,
                r#"{"code":"PERMISSION_DENIED","message":"missing scope universe:read"}"#,
            )
            .await;
            let section = section(&server).await;
            let line = read_line(&section);

            assert_eq!(line.status, report::Status::Fail);
            assert!(line.detail.contains("403"), "got {}", line.detail);

            let action = line.action.as_ref().expect("a failure carries its remedy");
            assert!(action.contains("scope"), "got {action}");
            assert!(action.contains("rbx-apikey update"), "got {action}");
            assert!(!action.contains("IP allowlist"), "got {action}");
        }

        /// A 404 is about the id, not the credential. Advice about the key
        /// would be a wild goose chase.
        #[tokio::test]
        async fn a_404_fails_and_sends_the_reader_to_the_universe_id() {
            let server = answering(404, r#"{"message":"Universe not found"}"#).await;
            let section = section(&server).await;
            let line = read_line(&section);

            assert_eq!(line.status, report::Status::Fail);
            assert!(line.detail.contains("404"), "got {}", line.detail);

            let action = line.action.as_ref().expect("a failure carries its remedy");
            assert!(action.contains("rbxplace.toml"), "got {action}");
            assert!(!action.contains("IP allowlist"), "got {action}");
            assert!(!action.contains("rbx-apikey update"), "got {action}");
        }

        /// A status with no reading of its own must still fail with something
        /// honest rather than borrow one of the three explanations above.
        #[tokio::test]
        async fn an_unclassified_refusal_says_nothing_local_explains_it() {
            let server = answering(500, r#"{"message":"internal"}"#).await;
            let section = section(&server).await;
            let line = read_line(&section);

            assert_eq!(line.status, report::Status::Fail);
            let action = line.action.as_ref().expect("a failure carries its remedy");
            assert!(action.contains("nothing local explains it"), "got {action}");
        }

        /// The target is printed before the outcome: a reader who disagrees
        /// with which universe was read has to see it to interpret the answer.
        #[tokio::test]
        async fn the_target_is_stated_before_the_outcome() {
            let server = answering(200, "{}").await;
            let section = section(&server).await;

            assert_eq!(section.lines[0].label, "target");
            assert!(section.lines[0].detail.contains(&UNIVERSE.to_string()));
        }
    }
}
