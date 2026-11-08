//! Step 3: the key's allowed CIDRs against this machine's public IP.
//!
//! A stale allowlist entry is refused with the same opaque 401 as a wrong
//! secret, which is the failure `doctor` exists to name. Making the comparison
//! needs one thing nothing else in this tool has: the caller's public
//! address, which a machine behind NAT cannot read off its own interfaces. The
//! only way to learn it is to ask something on the outside.
//!
//! So this module is behind `--check-ip` and does nothing unless asked. Three
//! rules shape it:
//!
//! 1. **One request, to a host that is named in the output.** This tool argues
//!    for least privilege; it does not get to quietly tell a third party where you
//!    are. The service is printed next to the answer, not only documented.
//! 2. **A lookup that fails is a check that could not run.** Offline, service
//!    down, timed out: none of those mean the IP is outside the allowlist, and
//!    reporting them as a mismatch would send somebody editing a key that is
//!    fine.
//! 3. **A short, explicit timeout.** `doctor` is the command people run when
//!    the network is already misbehaving, so it stays usable offline.

use std::net::IpAddr;
use std::time::Duration;

use rbx_core::api::ApiBase;

/// The echo service, named here and printed to the user by `lib.rs`.
///
/// Chosen for what it does not do: it answers `GET /` with the caller's
/// address as bare text and nothing else, so there is no JSON envelope to
/// misread, no API key to register, and no query string carrying anything
/// about this machine beyond the connection itself.
pub const ECHO_SERVICE: &str = "https://api.ipify.org";

/// Deliberately far below the suite's 60s default. This is an optional
/// convenience on a diagnostic command, and a caller who is offline should get
/// their report back rather than watch a spinner: waiting a minute to be told
/// the network is down is the opposite of the help being asked for.
pub const ECHO_TIMEOUT: Duration = Duration::from_secs(3);

/// What asking the echo service produced.
#[derive(Debug)]
pub enum IpLookup {
    Found(IpAddr),
    /// Nothing came back, phrased as what happened. Never a mismatch.
    Unavailable(String),
}

/// The public-IP lookup, pointed at a host the caller owns.
///
/// The seam is the same `#[cfg(test)] with_base_url` the read probe carries:
/// the comparison this feeds is the whole point of the flag, and it can only be
/// asserted end to end if the echo half can be answered by a mock.
#[derive(Debug)]
pub struct IpEcho {
    base: ApiBase,
}

impl Default for IpEcho {
    fn default() -> Self {
        Self {
            base: ApiBase::new(ECHO_SERVICE),
        }
    }
}

impl IpEcho {
    /// Point the lookup at another host. Tests only, and compiled only for
    /// them: outside the test build this would be dead code under
    /// `-D warnings`.
    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base = ApiBase::new(url);
        self
    }

    /// The host that will be contacted, so the caller can name it before it is.
    pub fn host(&self) -> &str {
        self.base.as_str()
    }

    /// One `GET`, with its own short-timeout client rather than the suite's
    /// shared one: `build_client` allows 60 seconds, which is right for a large
    /// asset upload and wrong for an optional lookup on a diagnostic.
    pub async fn resolve(&self) -> IpLookup {
        let client = match reqwest::Client::builder().timeout(ECHO_TIMEOUT).build() {
            Ok(c) => c,
            Err(e) => return IpLookup::Unavailable(format!("no HTTP client could be built: {e}")),
        };

        let response = match client.get(self.base.join("/")).send().await {
            Ok(r) => r,
            Err(e) => {
                let why = if e.is_timeout() {
                    format!("it did not answer within {}s", ECHO_TIMEOUT.as_secs())
                } else {
                    e.to_string()
                };
                return IpLookup::Unavailable(why);
            }
        };

        let status = response.status();
        if !status.is_success() {
            return IpLookup::Unavailable(format!("it answered {status}"));
        }

        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => return IpLookup::Unavailable(format!("its answer could not be read: {e}")),
        };
        parse_echo_body(&body)
    }
}

/// The body is expected to be one bare address. Anything else is a lookup that
/// did not produce an IP, which is not the same thing as an IP outside the
/// allowlist: a captive portal answering every request with an HTML login page
/// is the case this catches.
fn parse_echo_body(body: &str) -> IpLookup {
    match body.trim().parse::<IpAddr>() {
        Ok(ip) => IpLookup::Found(ip),
        Err(_) => IpLookup::Unavailable(format!(
            "it answered something that is not an IP address: {}",
            truncate(body.trim(), 60)
        )),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

// ---------------- the comparison itself ----------------

/// One entry of an allowlist: a network and how many of its leading bits are
/// fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix_len: u8,
}

impl Cidr {
    /// `a.b.c.d/n`, or a bare address read as a host route.
    ///
    /// Roblox always stores the prefixed form, but a bare address is what a
    /// human writes, and reading it as `/32` beats discarding an entry that
    /// says exactly what it means.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let network: IpAddr = addr.trim().parse().ok()?;
        let bits: u8 = match network {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix_len = match prefix {
            Some(p) => p.trim().parse::<u8>().ok()?,
            None => bits,
        };
        (prefix_len <= bits).then_some(Cidr {
            network,
            prefix_len,
        })
    }

    /// Whether `addr` falls inside. A v4 address is never inside a v6 network
    /// or the other way round: they are different address spaces, and treating
    /// a mapped form as equivalent would let an entry appear to match an
    /// address Roblox would see differently.
    pub fn contains(&self, addr: IpAddr) -> bool {
        match (self.network, addr) {
            (IpAddr::V4(net), IpAddr::V4(a)) => {
                shares_prefix(&net.octets(), &a.octets(), self.prefix_len)
            }
            (IpAddr::V6(net), IpAddr::V6(a)) => {
                shares_prefix(&net.octets(), &a.octets(), self.prefix_len)
            }
            _ => false,
        }
    }

    fn is_v4(&self) -> bool {
        self.network.is_ipv4()
    }
}

/// Compare the first `prefix_len` bits of two same-length octet strings.
fn shares_prefix(net: &[u8], addr: &[u8], prefix_len: u8) -> bool {
    let whole = usize::from(prefix_len / 8);
    if net[..whole] != addr[..whole] {
        return false;
    }
    let leftover = prefix_len % 8;
    if leftover == 0 {
        return true;
    }
    // Only the high `leftover` bits of the next octet are fixed by the prefix.
    let mask = 0xffu8 << (8 - leftover);
    net[whole] & mask == addr[whole] & mask
}

/// What comparing one address against one allowlist established.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Inside, carrying the entry that covers it so the reader can see which.
    Inside(String),
    /// Outside every entry, and the allowlist was well-formed enough to say so.
    Outside,
    /// The comparison could not be made. Carries why, and is never a failure:
    /// the same rule as a lookup that did not answer.
    Inconclusive(String),
}

/// Compare a resolved address against the CIDRs Roblox stores for the key.
///
/// The `Inconclusive` cases exist because a wrong "you are locked out" is more
/// expensive than no answer: it sends somebody to edit a working key. Two
/// things produce it: an allowlist with no entry in the address's own family
/// (nothing here can be compared), and an unparseable entry when nothing else
/// matched (the one entry that would have covered this address might be the one
/// that did not parse).
pub fn compare(cidrs: &[String], addr: IpAddr) -> Verdict {
    let mut unparsed: Vec<&str> = Vec::new();
    let mut same_family = 0usize;

    for entry in cidrs {
        let Some(cidr) = Cidr::parse(entry) else {
            unparsed.push(entry.as_str());
            continue;
        };
        if cidr.is_v4() == addr.is_ipv4() {
            same_family += 1;
        }
        if cidr.contains(addr) {
            return Verdict::Inside(entry.clone());
        }
    }

    if same_family == 0 {
        return Verdict::Inconclusive(format!(
            "this machine answers as {}, and the allowlist holds no {} entry, so there is \
             nothing here to compare it against. Roblox may still see this machine at an \
             address of the other family",
            addr,
            if addr.is_ipv4() { "IPv4" } else { "IPv6" }
        ));
    }
    if !unparsed.is_empty() {
        return Verdict::Inconclusive(format!(
            "no entry that could be read covers {}, but {} could not be read as a CIDR, so \
             the allowlist cannot be ruled out",
            addr,
            unparsed.join(", ")
        ));
    }
    Verdict::Outside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address")
    }

    fn list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    // ---------------- parsing ----------------

    #[test]
    fn a_host_route_covers_exactly_one_address() {
        let cidr = Cidr::parse("203.0.113.4/32").unwrap();
        assert!(cidr.contains(ip("203.0.113.4")));
        assert!(!cidr.contains(ip("203.0.113.5")));
    }

    #[test]
    fn a_bare_address_is_read_as_a_host_route() {
        let cidr = Cidr::parse("203.0.113.4").unwrap();
        assert_eq!(cidr, Cidr::parse("203.0.113.4/32").unwrap());
    }

    #[test]
    fn a_byte_aligned_prefix_covers_its_whole_block() {
        let cidr = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(cidr.contains(ip("10.255.255.255")));
        assert!(!cidr.contains(ip("11.0.0.1")));
    }

    /// The case a whole-octet comparison gets wrong: the boundary falls inside
    /// a byte, so `203.0.113.128` and `203.0.113.127` differ only in bits the
    /// mask has to look at.
    #[test]
    fn a_prefix_that_ends_mid_octet_masks_the_partial_byte() {
        let cidr = Cidr::parse("203.0.113.0/25").unwrap();
        assert!(cidr.contains(ip("203.0.113.127")));
        assert!(!cidr.contains(ip("203.0.113.128")));
    }

    #[test]
    fn a_zero_length_prefix_covers_everything_in_its_family() {
        let cidr = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(cidr.contains(ip("1.2.3.4")));
        assert!(cidr.contains(ip("203.0.113.9")));
    }

    #[test]
    fn a_prefix_longer_than_the_address_is_rejected_rather_than_clamped() {
        assert!(Cidr::parse("203.0.113.4/33").is_none());
        assert!(Cidr::parse("2001:db8::/129").is_none());
    }

    #[test]
    fn nonsense_is_not_a_cidr() {
        assert!(Cidr::parse("").is_none());
        assert!(Cidr::parse("not-an-ip/24").is_none());
        assert!(Cidr::parse("203.0.113.4/x").is_none());
    }

    #[test]
    fn ipv6_networks_parse_and_match_on_their_prefix() {
        let cidr = Cidr::parse("2001:db8::/32").unwrap();
        assert!(cidr.contains(ip("2001:db8:1234::1")));
        assert!(!cidr.contains(ip("2001:db9::1")));
    }

    #[test]
    fn an_ipv6_host_route_covers_exactly_one_address() {
        let cidr = Cidr::parse("2001:db8::1/128").unwrap();
        assert!(cidr.contains(ip("2001:db8::1")));
        assert!(!cidr.contains(ip("2001:db8::2")));
    }

    /// Different address spaces. A v4 address inside a v6 network would be a
    /// match Roblox does not agree with.
    #[test]
    fn the_two_families_never_match_each_other() {
        assert!(!Cidr::parse("0.0.0.0/0")
            .unwrap()
            .contains(ip("2001:db8::1")));
        assert!(!Cidr::parse("::/0").unwrap().contains(ip("1.2.3.4")));
    }

    // ---------------- the verdict ----------------

    #[test]
    fn an_address_inside_an_entry_names_the_entry_that_covers_it() {
        let verdict = compare(
            &list(&["198.51.100.0/24", "203.0.113.0/24"]),
            ip("203.0.113.9"),
        );
        assert_eq!(verdict, Verdict::Inside("203.0.113.0/24".to_string()));
    }

    #[test]
    fn an_address_in_none_of_the_entries_is_outside() {
        let verdict = compare(
            &list(&["198.51.100.0/24", "203.0.113.4/32"]),
            ip("203.0.113.9"),
        );
        assert_eq!(verdict, Verdict::Outside);
    }

    #[test]
    fn an_open_allowlist_covers_any_address_of_its_family() {
        assert!(matches!(
            compare(&list(&["0.0.0.0/0"]), ip("203.0.113.9")),
            Verdict::Inside(_)
        ));
    }

    /// The false alarm worth avoiding: the machine answered as v6, the key
    /// allows a v4 block, and Roblox may well see the v4. Saying "you are
    /// locked out" there sends somebody to edit a working key.
    #[test]
    fn an_allowlist_with_no_entry_of_the_addresss_family_is_inconclusive() {
        let verdict = compare(&list(&["203.0.113.0/24"]), ip("2001:db8::1"));
        match verdict {
            Verdict::Inconclusive(why) => assert!(why.contains("IPv6"), "got {why}"),
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    /// An entry that did not parse might have been the one that matched.
    #[test]
    fn an_unreadable_entry_blocks_a_confident_outside() {
        let verdict = compare(&list(&["203.0.113.0/24", "garbage"]), ip("198.51.100.7"));
        match verdict {
            Verdict::Inconclusive(why) => assert!(why.contains("garbage"), "got {why}"),
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    /// ...but not a match. A readable entry that covers the address answers the
    /// question whatever the rest of the list looks like.
    #[test]
    fn an_unreadable_entry_does_not_block_a_match() {
        let verdict = compare(&list(&["garbage", "203.0.113.0/24"]), ip("203.0.113.9"));
        assert_eq!(verdict, Verdict::Inside("203.0.113.0/24".to_string()));
    }

    // ---------------- the lookup ----------------

    #[test]
    fn a_bare_address_body_is_the_answer() {
        match parse_echo_body("203.0.113.9\n") {
            IpLookup::Found(ip) => assert_eq!(ip.to_string(), "203.0.113.9"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn an_ipv6_body_is_the_answer_too() {
        match parse_echo_body(" 2001:db8::1 ") {
            IpLookup::Found(ip) => assert_eq!(ip.to_string(), "2001:db8::1"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    /// A captive portal answers 200 with a login page. That is not an IP, and
    /// reading it as one would compare nonsense against the allowlist.
    #[test]
    fn a_body_that_is_not_an_address_is_unavailable_not_a_mismatch() {
        match parse_echo_body("<html>sign in to continue</html>") {
            IpLookup::Unavailable(why) => assert!(why.contains("not an IP address"), "got {why}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_long_junk_body_does_not_fill_the_terminal() {
        let IpLookup::Unavailable(why) = parse_echo_body(&"x".repeat(5000)) else {
            panic!("expected Unavailable");
        };
        assert!(
            why.chars().count() < 200,
            "got {} chars",
            why.chars().count()
        );
    }

    #[test]
    fn the_default_lookup_names_the_documented_service() {
        assert_eq!(IpEcho::default().host(), ECHO_SERVICE);
    }

    mod over_http {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn answering(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        #[tokio::test]
        async fn a_200_with_an_address_resolves_it_off_the_wire() {
            let server = answering(200, "203.0.113.9").await;
            let lookup = IpEcho::default()
                .with_base_url(server.uri())
                .resolve()
                .await;
            match lookup {
                IpLookup::Found(ip) => assert_eq!(ip.to_string(), "203.0.113.9"),
                other => panic!("expected Found, got {other:?}"),
            }
        }

        /// The service being broken is not the user being locked out.
        #[tokio::test]
        async fn a_service_error_is_unavailable_not_an_address() {
            let server = answering(503, "unavailable").await;
            let lookup = IpEcho::default()
                .with_base_url(server.uri())
                .resolve()
                .await;
            match lookup {
                IpLookup::Unavailable(why) => assert!(why.contains("503"), "got {why}"),
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }

        /// Port 1 rather than a mock started and dropped: tests in this binary
        /// run in parallel on ephemeral ports, so a just-freed port is one
        /// another test can be handed.
        #[tokio::test]
        async fn a_host_that_is_not_there_is_unavailable() {
            let lookup = IpEcho::default()
                .with_base_url("http://127.0.0.1:1")
                .resolve()
                .await;
            assert!(matches!(lookup, IpLookup::Unavailable(_)), "got {lookup:?}");
        }
    }
}
