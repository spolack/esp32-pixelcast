//! Minimal HTTP/1.1 request parsing for the built-in web server. One request
//! per connection; bodies are streamed by the caller.

use alloc::string::String;

/// The request line and the headers the server cares about.
#[derive(Debug, PartialEq, Eq)]
pub struct Head<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The part after `?`, or "".
    pub query: &'a str,
    pub content_length: Option<usize>,
    pub authorization: Option<&'a str>,
    /// The `Host` header, without a port.
    pub host: Option<&'a str>,
    /// `X-Requested-With` is present (scripts of our own pages send it).
    pub xrw: bool,
    /// The client waits for `100 Continue` before sending the body (curl does
    /// this for large uploads).
    pub expect_continue: bool,
}

/// Returns the length of the head including the blank line, once the buffer
/// holds a complete one.
pub fn head_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Parses a complete head (as found by [`head_len`]).
pub fn parse_head(head: &[u8]) -> Option<Head<'_>> {
    let text = core::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let method = request.next()?;
    let target = request.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if !request.next()?.starts_with("HTTP/1.") {
        return None;
    }

    let mut content_length = None;
    let mut authorization = None;
    let mut expect_continue = false;
    let mut host = None;
    let mut xrw = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse().ok()?);
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value);
        } else if name.eq_ignore_ascii_case("x-requested-with") {
            xrw = true;
        } else if name.eq_ignore_ascii_case("host") {
            host = Some(value.rsplit_once(':').map_or(value, |(h, _)| h));
        } else if name.eq_ignore_ascii_case("expect") {
            expect_continue = value.eq_ignore_ascii_case("100-continue");
        }
    }
    Some(Head {
        method,
        path,
        query,
        content_length,
        authorization,
        host,
        xrw,
        expect_continue,
    })
}

/// Checks an `Authorization` header against the expected Basic credentials
/// (already Base64-encoded), in constant time.
pub fn basic_auth_ok(header: Option<&str>, expected_b64: &str) -> bool {
    let Some(value) = header else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("basic") && ct_eq(token.trim().as_bytes(), expected_b64.as_bytes())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Standard Base64 with padding. `out` must hold `4 * input.len().div_ceil(3)`
/// bytes.
pub fn base64_encode<'a>(input: &[u8], out: &'a mut [u8]) -> &'a str {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut n = 0;
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out[n] = T[(v >> 18) as usize & 63];
        out[n + 1] = T[(v >> 12) as usize & 63];
        out[n + 2] = if chunk.len() > 1 { T[(v >> 6) as usize & 63] } else { b'=' };
        out[n + 3] = if chunk.len() > 2 { T[v as usize & 63] } else { b'=' };
        n += 4;
    }
    // Only ASCII from the table above was written.
    core::str::from_utf8(&out[..n]).unwrap_or("")
}

/// Iterates over `key=value` pairs of an `application/x-www-form-urlencoded`
/// string, decoded. Pairs without `=` get an empty value.
pub fn form_pairs(form: &str) -> impl Iterator<Item = (String, String)> + '_ {
    form.split('&').filter(|p| !p.is_empty()).map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (url_decode(k), url_decode(v))
    })
}

/// Decodes `+` and `%XX`. Invalid escapes are kept as they are; invalid
/// UTF-8 is replaced.
pub fn url_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = alloc::vec::Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                match (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Appends `v` URL-encoded (unreserved characters stay as they are).
pub fn url_encode_into(v: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in v.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
}


/// The web server's endpoints. [`Route::find`] is the one list of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    // Public: pages and live data.
    Index,
    ArtnetPage,
    Status,
    Leds,
    ArtnetData,
    // Need the login.
    ConfigPage,
    Config,
    Backup,
    SaveConfig,
    Update,
    Test,
    BootOther,
    Confirm,
    Rollback,
    DebugWifi,
    DebugPanic,
    Restart,
    Reset,
    MigrateLayout,
    InstallBootloader,
    UpdateInPlace,
    DualbootLog,
    DualbootReset,
}

const ROUTES: [(&str, &str, Route); 23] = [
    ("GET", "/", Route::Index),
    ("GET", "/artnet", Route::ArtnetPage),
    ("GET", "/api/status", Route::Status),
    ("GET", "/api/leds", Route::Leds),
    ("GET", "/api/artnet", Route::ArtnetData),
    ("GET", "/config", Route::ConfigPage),
    ("GET", "/api/config", Route::Config),
    ("GET", "/api/backup", Route::Backup),
    ("GET", "/api/dualboot-log", Route::DualbootLog),
    ("POST", "/config", Route::SaveConfig),
    ("POST", "/update", Route::Update),
    ("POST", "/test", Route::Test),
    ("POST", "/boot-other", Route::BootOther),
    ("POST", "/confirm", Route::Confirm),
    ("POST", "/rollback", Route::Rollback),
    ("POST", "/debug/wifi", Route::DebugWifi),
    ("POST", "/debug/panic", Route::DebugPanic),
    ("POST", "/restart", Route::Restart),
    ("POST", "/reset", Route::Reset),
    ("POST", "/migrate-layout", Route::MigrateLayout),
    ("POST", "/install-bootloader", Route::InstallBootloader),
    ("POST", "/update-in-place", Route::UpdateInPlace),
    ("POST", "/dualboot-reset", Route::DualbootReset),
];

impl Route {
    pub fn find(method: &str, path: &str) -> Option<Route> {
        ROUTES.iter().find(|(m, p, _)| *m == method && *p == path).map(|r| r.2)
    }

    /// Served without the login (read-only pages and status).
    pub fn public(self) -> bool {
        matches!(self, Route::Index | Route::ArtnetPage | Route::Status | Route::Leds | Route::ArtnetData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head() {
        let raw = b"POST /config?x=1 HTTP/1.1\r\nHost: node.local:80\r\ncontent-length: 12\r\nAuthorization: Basic YWJj\r\nX-Requested-With: x\r\nExpect: 100-continue\r\n\r\nbody";
        let n = head_len(raw).unwrap();
        assert_eq!(&raw[n..], b"body");
        let h = parse_head(&raw[..n]).unwrap();
        assert_eq!(
            h,
            Head {
                method: "POST",
                path: "/config",
                query: "x=1",
                content_length: Some(12),
                authorization: Some("Basic YWJj"),
                host: Some("node.local"),
                xrw: true,
                expect_continue: true,
            }
        );
    }

    #[test]
    fn head_minimal_and_bad() {
        let h = parse_head(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!((h.method, h.path, h.query, h.host, h.xrw), ("GET", "/", "", None, false));
        assert_eq!(head_len(b"GET / HTTP/1.1\r\n"), None, "incomplete");
        assert!(parse_head(b"GET /\r\n\r\n").is_none(), "no version");
        assert!(parse_head(b"GET / SPDY/3\r\n\r\n").is_none());
        assert!(parse_head(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n").is_none(), "bad length");
    }

    #[test]
    fn base64() {
        let mut b = [0u8; 16];
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg==")] {
            assert_eq!(base64_encode(i.as_bytes(), &mut b), o);
        }
        // The longest credentials: user "admin" and a 63-character password
        // fit the 128-byte buffer the web server uses.
        assert!(4 * (6 + 63usize).div_ceil(3) <= 128);
    }

    #[test]
    fn auth() {
        let mut b = [0u8; 32];
        let want = base64_encode(b"admin:secret", &mut b);
        assert!(basic_auth_ok(Some("Basic YWRtaW46c2VjcmV0"), want));
        assert!(basic_auth_ok(Some("basic  YWRtaW46c2VjcmV0 "), want));
        assert!(!basic_auth_ok(Some("Basic YWRtaW46c2VjcmV1"), want));
        assert!(!basic_auth_ok(Some("Basic YWRtaW46c2VjcmV0AA"), want));
        assert!(!basic_auth_ok(Some("Bearer YWRtaW46c2VjcmV0"), want));
        assert!(!basic_auth_ok(Some("Basic"), want));
        assert!(!basic_auth_ok(None, want));
    }

    #[test]
    fn routes() {
        for (i, a) in ROUTES.iter().enumerate() {
            assert_eq!(Route::find(a.0, a.1), Some(a.2));
            assert!(ROUTES[i + 1..].iter().all(|b| (b.0, b.1) != (a.0, a.1) && b.2 != a.2), "{a:?} twice");
            // Anything that changes state needs the login.
            assert!(a.0 == "GET" || !a.2.public(), "{a:?}");
        }
        assert_eq!(Route::find("POST", "/api/status"), None);
        assert_eq!(Route::find("GET", "/restart"), None);
        assert_eq!(Route::find("GET", "/nope"), None);
    }

    #[test]
    fn form() {
        let p: alloc::vec::Vec<_> = form_pairs("a=1&&b=x+y%21&c&d=%zz%4").collect();
        let s = |a: &str, b: &str| (String::from(a), String::from(b));
        assert_eq!(p, [s("a", "1"), s("b", "x y!"), s("c", ""), s("d", "%zz%4")]);
        let mut e = String::new();
        url_encode_into("a b&c=ä~", &mut e);
        assert_eq!(url_decode(&e), "a b&c=ä~");
        assert!(!e.contains(['&', '=', ' ']));
    }
}
