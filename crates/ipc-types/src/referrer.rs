//! Referrer policies and the `Referer` a request carries (W3C Referrer
//! Policy, Fetch), with the owner's cap (plan O23): the looser policies
//! act as `strict-origin-when-cross-origin` on cross-site requests.

use serde::{Deserialize, Serialize};
use url::Url;

/// A referrer policy (Referrer Policy §3). `Empty` is "no policy here":
/// a request falls back to its client's policy, and a client with none
/// uses the default, `strict-origin-when-cross-origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ReferrerPolicy {
    #[default]
    Empty,
    NoReferrer,
    NoReferrerWhenDowngrade,
    SameOrigin,
    Origin,
    StrictOrigin,
    OriginWhenCrossOrigin,
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

/// Every policy with its token, `Empty` first (the IDL enum's order).
const TOKENS: [(ReferrerPolicy, &str); 9] = [
    (ReferrerPolicy::Empty, ""),
    (ReferrerPolicy::NoReferrer, "no-referrer"),
    (ReferrerPolicy::NoReferrerWhenDowngrade, "no-referrer-when-downgrade"),
    (ReferrerPolicy::SameOrigin, "same-origin"),
    (ReferrerPolicy::Origin, "origin"),
    (ReferrerPolicy::StrictOrigin, "strict-origin"),
    (ReferrerPolicy::OriginWhenCrossOrigin, "origin-when-cross-origin"),
    (ReferrerPolicy::StrictOriginWhenCrossOrigin, "strict-origin-when-cross-origin"),
    (ReferrerPolicy::UnsafeUrl, "unsafe-url"),
];

impl ReferrerPolicy {
    /// The default referrer policy (Referrer Policy §3).
    pub const DEFAULT: ReferrerPolicy = ReferrerPolicy::StrictOriginWhenCrossOrigin;

    /// The policy's token, as `Request.referrerPolicy` reports it.
    pub fn as_str(self) -> &'static str {
        TOKENS.iter().find(|(p, _)| *p == self).map_or("", |(_, t)| t)
    }

    /// The policy a token names exactly (a `Referrer-Policy` header's
    /// tokens are matched as written, §8.1), the empty token included.
    pub fn from_token(token: &str) -> Option<Self> {
        TOKENS.iter().find(|(_, t)| *t == token).map(|(p, _)| *p)
    }

    /// A `referrerpolicy` content attribute (HTML §2.5.5): an enumerated
    /// attribute whose keywords are the policies, the empty string among
    /// them, matched ASCII case-insensitively; a missing or invalid value
    /// is the empty state.
    pub fn from_attribute(value: Option<&str>) -> Self {
        value
            .and_then(|v| TOKENS.iter().find(|(_, t)| t.eq_ignore_ascii_case(v)))
            .map_or(ReferrerPolicy::Empty, |(p, _)| *p)
    }

    /// A `<meta name=referrer>`'s `content` (HTML §4.2.5.1): lowercased,
    /// the legacy keywords mapped, then a policy or nothing. An empty
    /// content is ignored by the caller before this.
    pub fn from_meta(content: &str) -> Option<Self> {
        let value = content.to_ascii_lowercase();
        let value = match value.as_str() {
            "never" => "no-referrer",
            "default" => Self::DEFAULT.as_str(),
            "always" => "unsafe-url",
            "origin-when-crossorigin" => "origin-when-cross-origin",
            other => other,
        };
        Self::from_token(value)
    }

    /// The policy of a response's `Referrer-Policy` headers (§8.1): their
    /// values split on commas (Fetch's get, decode and split), the last
    /// token that names a policy winning; `Empty` when none does.
    pub fn from_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut policy = ReferrerPolicy::Empty;
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("referrer-policy") {
                continue;
            }
            for token in split_header_value(value) {
                if let Some(p) = Self::from_token(&token)
                    && p != ReferrerPolicy::Empty
                {
                    policy = p;
                }
            }
        }
        policy
    }

    /// This policy, or `fallback` when it is empty.
    pub fn or(self, fallback: ReferrerPolicy) -> Self {
        if self == ReferrerPolicy::Empty { fallback } else { self }
    }
}

/// Fetch's "split": a header value cut at commas outside quoted strings,
/// each piece with its HTTP whitespace (tab, space) trimmed.
fn split_header_value(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            '\\' if quoted => {
                current.push(c);
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            }
            ',' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter().map(|s| s.trim_matches([' ', '\t']).to_owned()).collect()
}

/// Where a request's `Referer` comes from: the URL Fetch's "determine
/// request's referrer" starts from (the client document's URL, a
/// stylesheet's, or the referrer an earlier hop determined) and the
/// request's policy, already resolved against the client's (never
/// `Empty` once a request is made; `Empty` is read as the default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestReferrer {
    pub source: Url,
    pub policy: ReferrerPolicy,
}

/// A navigation's referrer (HTML's navigate): the request's referrer
/// source and resolved policy, none for no referrer (the user's own
/// navigations, `noreferrer`); and the policy the navigation itself was
/// given (a link's `referrerpolicy`, `no-referrer` for `noreferrer`),
/// kept in its history entry, `Empty` when it fell back to the source
/// document's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationReferrer {
    pub request: Option<RequestReferrer>,
    pub policy: ReferrerPolicy,
}

/// Strip a URL for use as a referrer (§8.4): none for a local scheme
/// (`about`, `blob`, `data`); credentials and fragment gone; for the
/// origin only, the path `/` and no query.
pub fn strip(url: &Url, origin_only: bool) -> Option<Url> {
    if matches!(url.scheme(), "about" | "blob" | "data") {
        return None;
    }
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_fragment(None);
    if origin_only {
        u.set_path("");
        u.set_query(None);
    }
    Some(u)
}

/// Secure Contexts' "potentially trustworthy URL", for the schemes a
/// request can have: `https`/`wss`, `file`, `data`/`about:blank`/
/// `about:srcdoc`, and http(s) to a loopback host (`localhost` names
/// and the 127.0.0.0/8 and `::1` addresses).
pub fn potentially_trustworthy(url: &Url) -> bool {
    match url.scheme() {
        "https" | "wss" | "file" | "data" => true,
        "about" => matches!(url.path(), "blank" | "srcdoc"),
        _ => match url.host() {
            Some(url::Host::Ipv4(a)) => a.is_loopback(),
            Some(url::Host::Ipv6(a)) => a.is_loopback(),
            Some(url::Host::Domain(d)) => {
                let d = d.trim_end_matches('.').to_ascii_lowercase();
                d == "localhost" || d.ends_with(".localhost")
            }
            None => false,
        },
    }
}

/// A domain's registrable domain by the public suffix list (the `psl`
/// crate's compiled-in copy): the public suffix and one label more;
/// `None` when the domain is itself a public suffix (or has no label
/// beyond one). Lowercased, a trailing dot ignored.
pub fn registrable_domain(domain: &str) -> Option<String> {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    psl::domain_str(&d).map(str::to_owned)
}

/// Whether a domain is a public suffix (`com`, `co.uk`, `github.io`).
pub fn is_public_suffix(domain: &str) -> bool {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    !d.is_empty() && psl::suffix_str(&d) == Some(d.as_str())
}

/// HTML's "obtain a site" for a host: its registrable domain, else the
/// host itself (an IP address, a public suffix, a single label).
fn site_host(host: &url::Host<&str>) -> String {
    match host {
        url::Host::Domain(d) => registrable_domain(d).unwrap_or_else(|| d.trim_end_matches('.').to_ascii_lowercase()),
        other => other.to_string(),
    }
}

/// Whether two URLs are same site (HTML's schemeful "same site"): the
/// same scheme and the same site of their hosts.
pub fn same_site(a: &Url, b: &Url) -> bool {
    if a.scheme() != b.scheme() {
        return false;
    }
    match (a.host(), b.host()) {
        (Some(x), Some(y)) => site_host(&x) == site_host(&y),
        _ => false,
    }
}

/// `same_site` for hosts alone (CORP's `same-site` compares the
/// request's and the response's hosts this way).
pub fn same_site_hosts(a: &str, b: &str) -> bool {
    let site = |h: &str| match url::Host::parse(h) {
        Ok(url::Host::Domain(d)) => Some(site_host(&url::Host::Domain(d.as_str()))),
        Ok(other) => Some(other.to_string()),
        Err(_) => None,
    };
    matches!((site(a), site(b)), (Some(x), Some(y)) if x == y)
}

/// Fetch's "determine request's referrer" (Referrer Policy §8.3) from
/// `source` under `policy` for a request now at `current`, with the
/// owner's cap (O23): `no-referrer-when-downgrade`,
/// `origin-when-cross-origin` and `unsafe-url` act as
/// `strict-origin-when-cross-origin` when `current` is not same site
/// with the referrer. `None` is no referrer.
pub fn determine(source: &Url, policy: ReferrerPolicy, current: &Url) -> Option<Url> {
    use ReferrerPolicy as P;
    let mut url = strip(source, false)?;
    let origin = strip(source, true)?;
    // A referrer longer than 4096 bytes is cut to its origin.
    if url.as_str().len() > 4096 {
        url = origin.clone();
    }
    let policy = policy.or(ReferrerPolicy::DEFAULT);
    let policy = match policy {
        P::NoReferrerWhenDowngrade | P::OriginWhenCrossOrigin | P::UnsafeUrl if !same_site(&url, current) => {
            P::StrictOriginWhenCrossOrigin
        }
        p => p,
    };
    let same_origin = url.origin() == current.origin();
    let downgrade = potentially_trustworthy(&url) && !potentially_trustworthy(current);
    match policy {
        P::NoReferrer => None,
        P::Origin => Some(origin),
        P::UnsafeUrl => Some(url),
        P::StrictOrigin => (!downgrade).then_some(origin),
        P::Empty | P::StrictOriginWhenCrossOrigin => {
            if same_origin {
                Some(url)
            } else if downgrade {
                None
            } else {
                Some(origin)
            }
        }
        P::SameOrigin => same_origin.then_some(url),
        P::OriginWhenCrossOrigin => Some(if same_origin { url } else { origin }),
        P::NoReferrerWhenDowngrade => (!downgrade).then_some(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).expect("test URL")
    }

    fn d(source: &str, policy: ReferrerPolicy, current: &str) -> Option<String> {
        determine(&u(source), policy, &u(current)).map(|u| u.to_string())
    }

    #[test]
    fn policies_parse_from_tokens_attributes_meta_and_headers() {
        assert_eq!(ReferrerPolicy::from_token("unsafe-url"), Some(ReferrerPolicy::UnsafeUrl));
        assert_eq!(ReferrerPolicy::from_token("Unsafe-URL"), None, "header tokens as written");
        assert_eq!(ReferrerPolicy::from_attribute(Some("NO-REFERRER")), ReferrerPolicy::NoReferrer);
        assert_eq!(ReferrerPolicy::from_attribute(Some("bogus")), ReferrerPolicy::Empty);
        assert_eq!(ReferrerPolicy::from_attribute(None), ReferrerPolicy::Empty);
        assert_eq!(ReferrerPolicy::from_meta("Never"), Some(ReferrerPolicy::NoReferrer));
        assert_eq!(ReferrerPolicy::from_meta("default"), Some(ReferrerPolicy::StrictOriginWhenCrossOrigin));
        assert_eq!(ReferrerPolicy::from_meta("always"), Some(ReferrerPolicy::UnsafeUrl));
        assert_eq!(ReferrerPolicy::from_meta("origin-when-crossorigin"), Some(ReferrerPolicy::OriginWhenCrossOrigin));
        assert_eq!(ReferrerPolicy::from_meta("ORIGIN"), Some(ReferrerPolicy::Origin));
        assert_eq!(ReferrerPolicy::from_meta("nope"), None);
        let h = |v: &[&'static str]| ReferrerPolicy::from_headers(v.iter().map(|v| ("Referrer-Policy", *v)));
        assert_eq!(h(&["no-referrer, unsafe-url"]), ReferrerPolicy::UnsafeUrl, "the last valid token");
        assert_eq!(h(&["origin", "bogus"]), ReferrerPolicy::Origin, "unknown tokens ignored");
        assert_eq!(h(&["same-origin", " strict-origin "]), ReferrerPolicy::StrictOrigin, "headers combine");
        assert_eq!(h(&[""]), ReferrerPolicy::Empty);
        assert_eq!(h(&["\"origin, unsafe-url\""]), ReferrerPolicy::Empty, "a quoted string is one token");
    }

    #[test]
    fn the_referrer_is_stripped_and_follows_the_policy() {
        use ReferrerPolicy as P;
        let page = "https://user:pw@a.example/p/q?x=1#frag";
        // The default: full URL same origin, origin cross origin, nothing
        // on a downgrade.
        assert_eq!(d(page, P::Empty, "https://a.example/other").as_deref(), Some("https://a.example/p/q?x=1"));
        assert_eq!(d(page, P::Empty, "https://b.test/").as_deref(), Some("https://a.example/"));
        assert_eq!(d(page, P::Empty, "http://b.test/"), None);
        assert_eq!(d(page, P::NoReferrer, "https://a.example/"), None);
        assert_eq!(d(page, P::Origin, "https://a.example/z").as_deref(), Some("https://a.example/"));
        assert_eq!(d(page, P::Origin, "http://b.test/").as_deref(), Some("https://a.example/"));
        assert_eq!(d(page, P::SameOrigin, "https://sub.a.example/"), None);
        assert_eq!(d(page, P::StrictOrigin, "http://a.example/"), None);
        assert_eq!(d(page, P::StrictOrigin, "https://b.test/").as_deref(), Some("https://a.example/"));
        // The looser policies within a site.
        assert_eq!(d(page, P::UnsafeUrl, "https://sub.a.example/").as_deref(), Some("https://a.example/p/q?x=1"));
        assert_eq!(d(page, P::OriginWhenCrossOrigin, "https://sub.a.example/").as_deref(), Some("https://a.example/"));
        assert_eq!(d(page, P::NoReferrerWhenDowngrade, "https://sub.a.example/").as_deref(), Some("https://a.example/p/q?x=1"));
        // ... and capped across sites (O23).
        assert_eq!(d(page, P::UnsafeUrl, "https://b.test/").as_deref(), Some("https://a.example/"));
        assert_eq!(d(page, P::UnsafeUrl, "http://b.test/"), None);
        assert_eq!(d(page, P::NoReferrerWhenDowngrade, "https://b.test/").as_deref(), Some("https://a.example/"));
        assert_eq!(d("http://a.example/x", P::UnsafeUrl, "https://a.example/y").as_deref(), Some("http://a.example/"), "another scheme is another site");
        // Local schemes give none; a loopback http page is trustworthy.
        assert_eq!(d("data:text/html,x", P::UnsafeUrl, "https://a.example/"), None);
        assert_eq!(d("about:blank", P::UnsafeUrl, "https://a.example/"), None);
        assert_eq!(d("https://a.example/", P::Empty, "http://127.0.0.1:8080/").as_deref(), Some("https://a.example/"));
        assert_eq!(d("https://a.example/", P::Empty, "http://localhost/").as_deref(), Some("https://a.example/"));
        // Sites by the public suffix list: `co.uk` and `github.io` are
        // suffixes, so their subdomains are sites of their own.
        assert_eq!(d(page, P::UnsafeUrl, "https://x.a.example/").as_deref(), Some("https://a.example/p/q?x=1"));
        assert_eq!(d("https://a.co.uk/p", P::UnsafeUrl, "https://b.co.uk/").as_deref(), Some("https://a.co.uk/"), "another site");
        assert_eq!(d("https://w.a.co.uk/p", P::UnsafeUrl, "https://a.co.uk/").as_deref(), Some("https://w.a.co.uk/p"), "the same site");
        assert_eq!(d("https://me.github.io/p", P::UnsafeUrl, "https://you.github.io/").as_deref(), Some("https://me.github.io/"));
        assert!(is_public_suffix("co.uk") && is_public_suffix("COM.") && !is_public_suffix("example.com"));
        assert_eq!(registrable_domain("www.Example.co.uk").as_deref(), Some("example.co.uk"));
        assert!(same_site_hosts("a.example.com", "b.example.com") && !same_site_hosts("a.co.uk", "b.co.uk"));
        assert!(same_site_hosts("127.0.0.1", "127.0.0.1") && !same_site_hosts("127.0.0.1", "127.0.0.2"));
        // Over 4096 bytes: the origin.
        let long = format!("https://a.example/{}", "x".repeat(4100));
        assert_eq!(d(&long, P::UnsafeUrl, "https://a.example/").as_deref(), Some("https://a.example/"));
    }
}
