//! Rewriting of the guest to upstream request stream.
//!
//! Substitution is scoped to the request head: the request line and every
//! header value, whatever it is named. Bodies stream through untouched.
//!
//! That scope is what makes this deterministic. At an arbitrary byte offset,
//! "no more bytes yet" and "the request ended" are indistinguishable without
//! a clock. A head that has not reached its terminating CRLF CRLF is
//! unambiguously an unfinished request, so holding it is correct rather than
//! a guess, and the upstream is waiting on those bytes anyway.
//!
//! Framing is read, never rewritten. It is needed only to find where a body
//! ends and the next head begins on a reused connection. An intermediary that
//! never re-frames cannot desynchronise its upstream by re-framing wrongly,
//! so the request smuggling exposure of a rewriting proxy does not arise.
//! Anything ambiguous degrades to a plain byte tunnel.
//!
//! A placeholder inside an `Authorization: Basic` token is swapped too, after
//! decoding it, under the same rule. A secret scoped to authorization is
//! swapped in the value of an `Authorization` header only.
//!
//! See docs/rfcs/0002-refreshable-secrets.md and 0013-live-secrets.md.

use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;

use crate::config::Scope;
use crate::secrets::Substitution;

type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Cap on a single request head. A head larger than this is tunnelled rather
/// than buffered without bound.
const MAX_HEAD: usize = 64 * 1024;
/// Longest chunk-size or trailer line kept while looking for its end.
const MAX_CHUNK_LINE: usize = 4096;

const HEAD_END: &[u8] = b"\r\n\r\n";

/// Replace all occurrences of `from` with `to` in a byte slice.
pub(crate) fn replace_bytes(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || data.len() < from.len() {
        return data.to_vec();
    }

    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i <= data.len() - from.len() {
        if &data[i..i + from.len()] == from {
            result.extend_from_slice(to);
            i += from.len();
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    result.extend_from_slice(&data[i..]);
    result
}

/// How the body of the request being read is delimited.
enum Body {
    /// A known number of bytes remain.
    Exact(u64),
    /// Chunked, tracked only closely enough to find where it ends.
    Chunked(Chunked),
}

enum Chunked {
    /// Reading a chunk size line, which may carry extensions after a `;`.
    Size(Vec<u8>),
    /// Passing through chunk data.
    Data(u64),
    /// Consuming the CRLF that follows chunk data.
    DataEnd,
    /// After the zero chunk, reading trailers until a blank line.
    Trailer(Vec<u8>),
}

enum State {
    /// Accumulating a request head.
    Head,
    /// Forwarding a body.
    Body(Body),
    /// Forwarding everything unchanged for the rest of the connection.
    Tunnel,
}

/// Framing derived from a head, per RFC 9112 section 6.3.
enum Framing {
    None,
    Exact(u64),
    Chunked,
    /// Unparseable, self contradictory, or not plain HTTP from here on.
    Ambiguous,
}

/// Substitutes secrets into request heads on a guest to upstream stream.
pub(crate) struct RequestStream {
    /// Placeholder to real value, both as raw bytes, swapped anywhere in a
    /// head.
    pairs: Pairs,
    /// The same, swapped only in the value of an `Authorization` header.
    authorization: Pairs,
    state: State,
    head: Vec<u8>,
}

impl RequestStream {
    pub(crate) fn new(substitutions: Vec<Substitution>) -> Self {
        let (pairs, authorization) = Self::split(substitutions);
        Self {
            pairs,
            authorization,
            state: State::Head,
            head: Vec::new(),
        }
    }

    fn split(substitutions: Vec<Substitution>) -> (Pairs, Pairs) {
        let mut head = Vec::new();
        let mut authorization = Vec::new();
        for s in substitutions {
            let pair = (s.placeholder.into_bytes(), s.value.into_bytes());
            match s.scope {
                Scope::Head => head.push(pair),
                Scope::Authorization => authorization.push(pair),
            }
        }
        (head, authorization)
    }

    /// Swap in freshly resolved values, applied from the next head onwards.
    pub(crate) fn update(&mut self, substitutions: Vec<Substitution>) {
        (self.pairs, self.authorization) = Self::split(substitutions);
    }

    #[cfg(test)]
    fn is_tunnel(&self) -> bool {
        matches!(self.state, State::Tunnel)
    }

    /// True between requests: no body under way and no head begun, so the
    /// next bytes start a new request.
    pub(crate) fn at_request_start(&self) -> bool {
        matches!(self.state, State::Head) && self.head.is_empty()
    }

    /// Consume a chunk of guest bytes, returning what to forward upstream.
    ///
    /// Returns fewer bytes than it was given only while a head is incomplete,
    /// which is precisely when the request is unfinished.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        let mut pos = 0;

        while pos < chunk.len() {
            match &mut self.state {
                State::Tunnel => {
                    out.extend_from_slice(&chunk[pos..]);
                    pos = chunk.len();
                }
                State::Body(body) => {
                    let (taken, finished) = body.consume(&chunk[pos..]);
                    out.extend_from_slice(&chunk[pos..pos + taken]);
                    pos += taken;
                    if finished {
                        self.state = State::Head;
                    } else if taken == 0 {
                        // Framing stopped making sense mid-body.
                        self.state = State::Tunnel;
                    }
                }
                State::Head => {
                    let rest = &chunk[pos..];
                    match head_end(&self.head, rest) {
                        Some(end) => {
                            self.head.extend_from_slice(&rest[..end]);
                            pos += end;
                            let head = std::mem::take(&mut self.head);
                            let rewritten = self.finish_head(head);
                            out.extend_from_slice(&rewritten);
                        }
                        None => {
                            self.head.extend_from_slice(rest);
                            pos = chunk.len();
                            if self.head.len() > MAX_HEAD {
                                // Never buffer without bound: give up on
                                // interpreting this connection.
                                out.extend_from_slice(&std::mem::take(&mut self.head));
                                self.state = State::Tunnel;
                            }
                        }
                    }
                }
            }
        }

        out
    }

    /// Anything still held when the guest stops sending. A partial head here
    /// means the request was abandoned, so forward it rather than eat it.
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.head)
    }

    /// Substitute into a complete head and set the state for its body.
    fn finish_head(&mut self, head: Vec<u8>) -> Vec<u8> {
        // Classify before substituting, so framing is read from what the
        // guest actually sent.
        self.state = match classify(&head) {
            Framing::None => State::Head,
            Framing::Exact(n) => State::Body(Body::Exact(n)),
            Framing::Chunked => State::Body(Body::Chunked(Chunked::Size(Vec::new()))),
            Framing::Ambiguous => State::Tunnel,
        };

        let mut out = head;
        for (from, to) in &self.pairs {
            out = replace_bytes(&out, from, to);
        }
        if self.pairs.is_empty() && self.authorization.is_empty() {
            return out;
        }
        substitute_authorization(&out, &self.authorization, &self.pairs).unwrap_or(out)
    }
}

/// Base64 as Basic credentials carry it, with or without padding.
const BASIC: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The head with the authorization scoped pairs swapped in every
/// `Authorization` value, and every pair inside each `Basic` token, or None
/// when nothing changed. Git sends `user:token` as Basic, encoded, so a
/// placeholder there never matches as it stands.
fn substitute_authorization(head: &[u8], authorization: &Pairs, pairs: &Pairs) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(head.len());
    let mut changed = false;
    let mut rest = head;
    while !rest.is_empty() {
        let end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .map_or(rest.len(), |i| i + 2);
        let (line, next) = rest.split_at(end);
        match authorization_line(line, authorization, pairs) {
            Some(rewritten) => {
                out.extend_from_slice(&rewritten);
                changed = true;
            }
            None => out.extend_from_slice(line),
        }
        rest = next;
    }
    changed.then_some(out)
}

/// One header line rewritten, when it is `Authorization` and a placeholder
/// in its value, or in its decoded Basic token, was swapped.
fn authorization_line(line: &[u8], authorization: &Pairs, pairs: &Pairs) -> Option<Vec<u8>> {
    let colon = line.iter().position(|b| *b == b':')?;
    if !line[..colon].eq_ignore_ascii_case(b"authorization") {
        return None;
    }
    let given = &line[colon + 1..];
    let mut value = given.to_vec();
    for (from, to) in authorization {
        value = replace_bytes(&value, from, to);
    }
    if let Some(swapped) = basic(&value, authorization.iter().chain(pairs)) {
        value = swapped;
    }
    if value == given {
        return None;
    }
    let mut out = line[..colon + 1].to_vec();
    out.extend_from_slice(&value);
    Some(out)
}

/// A `Basic` value with every pair swapped inside its decoded token, or None
/// when it is not Basic or held no placeholder.
fn basic<'a>(value: &[u8], pairs: impl Iterator<Item = &'a (Vec<u8>, Vec<u8>)>) -> Option<Vec<u8>> {
    let start = value.iter().position(|b| !matches!(b, b' ' | b'\t'))?;
    let scheme = value[start..].get(..6)?;
    if !scheme[..5].eq_ignore_ascii_case(b"basic") || !matches!(scheme[5], b' ' | b'\t') {
        return None;
    }
    let token_start = start
        + 5
        + value[start + 5..]
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t'))?;
    let token_end = value
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))?
        + 1;
    if token_end <= token_start {
        return None;
    }
    let decoded = BASIC.decode(&value[token_start..token_end]).ok()?;
    let mut swapped = decoded.clone();
    for (from, to) in pairs {
        swapped = replace_bytes(&swapped, from, to);
    }
    if swapped == decoded {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() + 64);
    out.extend_from_slice(&value[..token_start]);
    out.extend_from_slice(BASIC.encode(&swapped).as_bytes());
    out.extend_from_slice(&value[token_end..]);
    Some(out)
}

impl Body {
    /// Returns how many leading bytes of `data` belong to this body, and
    /// whether the body ended within them.
    fn consume(&mut self, data: &[u8]) -> (usize, bool) {
        match self {
            Body::Exact(remaining) => {
                let take = (*remaining).min(data.len() as u64) as usize;
                *remaining -= take as u64;
                (take, *remaining == 0)
            }
            Body::Chunked(chunked) => chunked.consume(data),
        }
    }
}

impl Chunked {
    fn consume(&mut self, data: &[u8]) -> (usize, bool) {
        let mut pos = 0;
        while pos < data.len() {
            match self {
                Chunked::Size(line) => {
                    let rest = &data[pos..];
                    match rest.iter().position(|b| *b == b'\n') {
                        Some(i) => {
                            line.extend_from_slice(&rest[..=i]);
                            pos += i + 1;
                            let size = parse_chunk_size(line);
                            line.clear();
                            match size {
                                Some(0) => *self = Chunked::Trailer(Vec::new()),
                                Some(n) => *self = Chunked::Data(n),
                                // An unreadable size line means the framing is
                                // no longer trustworthy.
                                None => return (pos, false),
                            }
                        }
                        None => {
                            // Not consuming makes the caller tunnel the rest.
                            if line.len() + rest.len() > MAX_CHUNK_LINE {
                                return (pos, false);
                            }
                            line.extend_from_slice(rest);
                            pos = data.len();
                        }
                    }
                }
                Chunked::Data(remaining) => {
                    let take = (*remaining).min((data.len() - pos) as u64) as usize;
                    *remaining -= take as u64;
                    pos += take;
                    if *remaining == 0 {
                        *self = Chunked::DataEnd;
                    }
                }
                Chunked::DataEnd => {
                    // Skip the CRLF terminating chunk data.
                    if data[pos] == b'\n' {
                        *self = Chunked::Size(Vec::new());
                    }
                    pos += 1;
                }
                Chunked::Trailer(line) => {
                    let rest = &data[pos..];
                    match rest.iter().position(|b| *b == b'\n') {
                        Some(i) => {
                            line.extend_from_slice(&rest[..=i]);
                            pos += i + 1;
                            let blank = line
                                .iter()
                                .all(|b| matches!(b, b'\r' | b'\n' | b' ' | b'\t'));
                            line.clear();
                            if blank {
                                return (pos, true);
                            }
                        }
                        None => {
                            if line.len() + rest.len() > MAX_CHUNK_LINE {
                                return (pos, false);
                            }
                            line.extend_from_slice(rest);
                            pos = data.len();
                        }
                    }
                }
            }
        }
        (pos, false)
    }
}

/// Chunk size is hex, optionally followed by `;` extensions.
fn parse_chunk_size(line: &[u8]) -> Option<u64> {
    let digits: Vec<u8> = line
        .iter()
        .copied()
        .skip_while(|b| matches!(b, b' ' | b'\t'))
        .take_while(|b| b.is_ascii_hexdigit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(&digits).ok()?, 16).ok()
}

/// Index in `hay` just past a CRLF CRLF, which may have started in `prev`.
fn head_end(prev: &[u8], hay: &[u8]) -> Option<usize> {
    let back = prev.len().min(HEAD_END.len() - 1);
    if back > 0 {
        let mut probe = Vec::with_capacity(back + HEAD_END.len() - 1);
        probe.extend_from_slice(&prev[prev.len() - back..]);
        probe.extend_from_slice(&hay[..hay.len().min(HEAD_END.len() - 1)]);
        for start in 0..back {
            if probe.len() >= start + HEAD_END.len()
                && &probe[start..start + HEAD_END.len()] == HEAD_END
            {
                return Some(start + HEAD_END.len() - back);
            }
        }
    }
    hay.windows(HEAD_END.len())
        .position(|w| w == HEAD_END)
        .map(|i| i + HEAD_END.len())
}

/// Derive body framing from a head, following RFC 9112 section 6.3.
///
/// Anything the specification calls out as a possible smuggling attempt is
/// reported ambiguous rather than resolved, since this proxy has no need to
/// forward such a message as HTTP.
fn classify(head: &[u8]) -> Framing {
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(head), Ok(httparse::Status::Complete(_))) {
        return Framing::Ambiguous;
    }

    // CONNECT stops being HTTP after this exchange.
    if req.method.map(|m| m.eq_ignore_ascii_case("CONNECT")) == Some(true) {
        return Framing::Ambiguous;
    }

    let mut content_length: Option<u64> = None;
    let mut conflicting_length = false;
    let mut chunked = false;
    let mut has_transfer_encoding = false;
    let mut upgrade = false;

    for header in req.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            has_transfer_encoding = true;
            let value = String::from_utf8_lossy(header.value).to_ascii_lowercase();
            // Only chunked as the final coding gives a length we can follow.
            chunked = value.rsplit(',').next().map(str::trim) == Some("chunked");
        } else if header.name.eq_ignore_ascii_case("content-length") {
            let parsed = std::str::from_utf8(header.value)
                .ok()
                .map(str::trim)
                .and_then(|v| v.parse::<u64>().ok());
            match (parsed, content_length) {
                (None, _) => conflicting_length = true,
                (Some(n), Some(seen)) if n != seen => conflicting_length = true,
                (Some(n), _) => content_length = Some(n),
            }
        } else if header.name.eq_ignore_ascii_case("upgrade") {
            upgrade = true;
        }
    }

    if upgrade || conflicting_length {
        return Framing::Ambiguous;
    }
    if has_transfer_encoding {
        // Both fields present is called out as a possible smuggling attempt,
        // and a non-chunked coding leaves no length to follow.
        if content_length.is_some() || !chunked {
            return Framing::Ambiguous;
        }
        return Framing::Chunked;
    }
    match content_length {
        Some(0) | None => Framing::None,
        Some(n) => Framing::Exact(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLACEHOLDER: &str = "shuru_tok_abc123";
    const SECRET: &str = "sk-a-much-longer-real-secret-value";

    fn substitution(placeholder: &str, value: &str, scope: Scope) -> Substitution {
        Substitution {
            placeholder: placeholder.to_string(),
            value: value.to_string(),
            scope,
        }
    }

    fn stream() -> RequestStream {
        RequestStream::new(vec![substitution(PLACEHOLDER, SECRET, Scope::Head)])
    }

    fn scoped() -> RequestStream {
        RequestStream::new(vec![substitution(
            PLACEHOLDER,
            SECRET,
            Scope::Authorization,
        )])
    }

    /// Feed `input` in slices of `size`, returning everything forwarded.
    fn feed(s: &mut RequestStream, input: &[u8], size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for piece in input.chunks(size.max(1)) {
            out.extend(s.push(piece));
        }
        out
    }

    fn get_request() -> String {
        format!("GET /v1/models HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer {PLACEHOLDER}\r\n\r\n")
    }

    #[test]
    fn substitutes_in_any_header_not_a_fixed_list() {
        for name in [
            "Authorization",
            "X-Api-Key",
            "x-goog-api-key",
            "X-Some-Vendor-Nobody-Has-Heard-Of",
        ] {
            let req = format!("GET / HTTP/1.1\r\nHost: h\r\n{name}: {PLACEHOLDER}\r\n\r\n");
            let out = stream().push(req.as_bytes());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                req.replace(PLACEHOLDER, SECRET),
                "header {name} was not substituted"
            );
        }
    }

    #[test]
    fn substitutes_in_the_request_line() {
        let req = format!(
            "GET /v1?key={PLACEHOLDER}&x=1 HTTP/1.1\r\nHost: generativelanguage.googleapis.com\r\n\r\n"
        );
        let out = stream().push(req.as_bytes());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            req.replace(PLACEHOLDER, SECRET)
        );
    }

    #[test]
    fn substitutes_at_every_split_point() {
        let req = get_request();
        let expected = req.replace(PLACEHOLDER, SECRET);
        for size in 1..=req.len() {
            let mut s = stream();
            let mut out = feed(&mut s, req.as_bytes(), size);
            out.extend(s.take_pending());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                expected,
                "lost substitution at chunk size {size}"
            );
        }
    }

    #[test]
    fn a_body_is_forwarded_untouched_even_if_it_holds_a_placeholder() {
        let body = format!("tok={PLACEHOLDER}");
        let req = format!(
            "POST /p HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut s = stream();
        let out = String::from_utf8(feed(&mut s, req.as_bytes(), 7)).unwrap();

        // Header substituted, body byte for byte identical, so the declared
        // Content-Length stays true.
        assert!(out.contains(&format!("Authorization: Bearer {SECRET}")));
        assert!(out.ends_with(&body));
        assert_eq!(out.len(), req.len() - PLACEHOLDER.len() + SECRET.len());
    }

    #[test]
    fn second_request_on_a_reused_connection_is_substituted() {
        let body = "0123456789";
        let first = format!(
            "POST /p HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\nContent-Length: 10\r\n\r\n{body}"
        );
        let both = format!("{first}{}", get_request());

        for size in [1, 3, 17, 64, 4096] {
            let mut s = stream();
            let out = String::from_utf8(feed(&mut s, both.as_bytes(), size)).unwrap();
            assert_eq!(
                out.matches(SECRET).count(),
                2,
                "both heads should be substituted at chunk size {size}"
            );
            assert!(!out.contains(PLACEHOLDER));
        }
    }

    #[test]
    fn chunked_body_is_tracked_so_the_next_head_is_found() {
        let first = format!(
            "POST /p HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n3\r\nbye\r\n0\r\n\r\n"
        );
        let both = format!("{first}{}", get_request());

        for size in [1, 2, 9, 128] {
            let mut s = stream();
            let out = String::from_utf8(feed(&mut s, both.as_bytes(), size)).unwrap();
            assert_eq!(
                out.matches(SECRET).count(),
                2,
                "chunked body mistracked at chunk size {size}"
            );
            assert!(out.contains("5\r\nhello\r\n"), "chunk data was altered");
        }
    }

    #[test]
    fn chunk_extensions_and_trailers_are_handled() {
        let first = format!(
            "POST /p HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n0\r\nX-Trailer: v\r\n\r\n"
        );
        let both = format!("{first}{}", get_request());
        let mut s = stream();
        let out = String::from_utf8(feed(&mut s, both.as_bytes(), 3)).unwrap();
        assert_eq!(out.matches(SECRET).count(), 2);
    }

    #[test]
    fn ambiguous_framing_degrades_to_a_tunnel() {
        // Both Content-Length and Transfer-Encoding: RFC 9112 6.3 calls this
        // out as a possible smuggling attempt.
        let smuggle = format!(
            "POST /p HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n{}",
            get_request()
        );
        let mut s = stream();
        let out = String::from_utf8(feed(&mut s, smuggle.as_bytes(), 11)).unwrap();

        assert!(s.is_tunnel());
        // The offending head is still substituted, but nothing after it is
        // interpreted, so a smuggled second request is passed through as data.
        assert_eq!(out.matches(SECRET).count(), 1);
        assert!(
            out.contains(PLACEHOLDER),
            "tunnelled bytes must be verbatim"
        );
    }

    #[test]
    fn conflicting_content_lengths_degrade_to_a_tunnel() {
        let req =
            "POST /p HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\nContent-Length: 9\r\n\r\nhello";
        let mut s = stream();
        let out = feed(&mut s, req.as_bytes(), 5);
        assert!(s.is_tunnel());
        assert_eq!(out, req.as_bytes());
    }

    #[test]
    fn upgrade_and_connect_degrade_to_a_tunnel() {
        let ws = "GET /s HTTP/1.1\r\nHost: h\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        let mut s = stream();
        let out = feed(&mut s, ws.as_bytes(), 6);
        assert!(s.is_tunnel());
        assert_eq!(out, ws.as_bytes());

        let connect = "CONNECT h:443 HTTP/1.1\r\nHost: h\r\n\r\n";
        let mut s = stream();
        s.push(connect.as_bytes());
        assert!(s.is_tunnel());
    }

    #[test]
    fn an_oversized_head_is_tunnelled_not_buffered_forever() {
        let mut s = stream();
        let filler = format!("GET / HTTP/1.1\r\nX-Pad: {}\r\n", "A".repeat(MAX_HEAD));
        let out = s.push(filler.as_bytes());
        assert!(s.is_tunnel());
        assert_eq!(out, filler.as_bytes(), "held bytes must be released intact");
    }

    #[test]
    fn an_incomplete_head_is_held_and_released_on_close() {
        let mut s = stream();
        let partial = "GET / HTTP/1.1\r\nHost: h\r\n";
        assert!(
            s.push(partial.as_bytes()).is_empty(),
            "an unfinished request must not be forwarded"
        );
        assert_eq!(s.take_pending(), partial.as_bytes());
    }

    #[test]
    fn refreshed_values_apply_from_the_next_head() {
        let mut s = stream();
        let out = String::from_utf8(s.push(get_request().as_bytes())).unwrap();
        assert!(out.contains(SECRET));

        s.update(vec![substitution(
            PLACEHOLDER,
            "rotated-value",
            Scope::Head,
        )]);
        let out = String::from_utf8(s.push(get_request().as_bytes())).unwrap();
        assert!(out.contains("rotated-value"));
    }

    #[test]
    fn no_substitutions_passes_bytes_through() {
        let mut s = RequestStream::new(Vec::new());
        let req = get_request();
        assert_eq!(feed(&mut s, req.as_bytes(), 5), req.as_bytes());
    }

    fn basic(credentials: &str) -> String {
        BASIC.encode(credentials)
    }

    #[test]
    fn substitutes_inside_basic_credentials() {
        let req = format!(
            "GET /o/r.git/info/refs HTTP/1.1\r\nHost: github.com\r\nAuthorization: Basic {}\r\nAccept: */*\r\n\r\n",
            basic(&format!("x-access-token:{PLACEHOLDER}"))
        );
        let expected = req.replace(
            &basic(&format!("x-access-token:{PLACEHOLDER}")),
            &basic(&format!("x-access-token:{SECRET}")),
        );
        for size in [1, 5, 64, 4096] {
            let mut s = stream();
            let mut out = feed(&mut s, req.as_bytes(), size);
            out.extend(s.take_pending());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                expected,
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn basic_scheme_and_header_name_are_read_without_case() {
        let token = basic(&format!("u:{PLACEHOLDER}"));
        let req = format!("GET / HTTP/1.1\r\nHost: h\r\nauthorization:  bAsIc   {token}  \r\n\r\n");
        let out = String::from_utf8(stream().push(req.as_bytes())).unwrap();
        assert_eq!(
            out,
            format!(
                "GET / HTTP/1.1\r\nHost: h\r\nauthorization:  bAsIc   {}  \r\n\r\n",
                basic(&format!("u:{SECRET}"))
            )
        );
    }

    #[test]
    fn basic_without_padding_is_decoded() {
        let token = basic(&format!("ab:{PLACEHOLDER}"));
        let unpadded = token.trim_end_matches('=');
        assert_ne!(token, unpadded, "the test wants a token that pads");
        let req = format!("GET / HTTP/1.1\r\nHost: h\r\nAuthorization: Basic {unpadded}\r\n\r\n");
        let out = String::from_utf8(stream().push(req.as_bytes())).unwrap();
        assert!(out.contains(&basic(&format!("ab:{SECRET}"))));
    }

    #[test]
    fn basic_without_a_placeholder_or_not_base64_is_left_alone() {
        for value in [
            format!("Basic {}", basic("user:password")),
            "Basic not*base64!".to_string(),
            "Basic".to_string(),
            "Basic ".to_string(),
            format!("Bearer {}", basic(&format!("u:{PLACEHOLDER}"))),
            format!("Basicx {}", basic(&format!("u:{PLACEHOLDER}"))),
        ] {
            let req = format!("GET / HTTP/1.1\r\nHost: h\r\nAuthorization: {value}\r\n\r\n");
            let out = stream().push(req.as_bytes());
            assert_eq!(out, req.as_bytes(), "{value} was changed");
        }
    }

    #[test]
    fn basic_only_in_authorization() {
        let token = basic(&format!("u:{PLACEHOLDER}"));
        let req = format!("GET / HTTP/1.1\r\nHost: h\r\nX-Note: Basic {token}\r\nProxy-Authorization: Basic {token}\r\n\r\n");
        let out = stream().push(req.as_bytes());
        assert_eq!(out, req.as_bytes());
    }

    #[test]
    fn basic_is_not_substituted_without_a_secret_for_the_host() {
        let token = basic(&format!("u:{PLACEHOLDER}"));
        let req = format!("GET / HTTP/1.1\r\nHost: h\r\nAuthorization: Basic {token}\r\n\r\n");
        let out = RequestStream::new(Vec::new()).push(req.as_bytes());
        assert_eq!(out, req.as_bytes());
    }

    #[test]
    fn basic_in_a_body_is_left_alone() {
        let body = format!(
            "Authorization: Basic {}",
            basic(&format!("u:{PLACEHOLDER}"))
        );
        let req = format!(
            "POST / HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let out = feed(&mut stream(), req.as_bytes(), 9);
        assert_eq!(out, req.as_bytes());
    }

    #[test]
    fn scoped_secrets_are_swapped_in_authorization_only() {
        let token = basic(&format!("x-access-token:{PLACEHOLDER}"));
        for (given, wanted) in [
            (
                format!("Authorization: token {PLACEHOLDER}"),
                format!("Authorization: token {SECRET}"),
            ),
            (
                format!("authorization: Bearer {PLACEHOLDER}"),
                format!("authorization: Bearer {SECRET}"),
            ),
            (
                format!("Authorization: Basic {token}"),
                format!(
                    "Authorization: Basic {}",
                    basic(&format!("x-access-token:{SECRET}"))
                ),
            ),
        ] {
            let req = format!("GET / HTTP/1.1\r\nHost: h\r\n{given}\r\n\r\n");
            let out = String::from_utf8(scoped().push(req.as_bytes())).unwrap();
            assert_eq!(
                out,
                format!("GET / HTTP/1.1\r\nHost: h\r\n{wanted}\r\n\r\n")
            );
        }
    }

    #[test]
    fn scoped_secrets_are_not_swapped_where_a_host_would_keep_them() {
        let req = format!(
            "PUT /repos/o/r/contents/{PLACEHOLDER} HTTP/1.1\r\nHost: api.github.com\r\nX-Title: {PLACEHOLDER}\r\nX-Authorization: {PLACEHOLDER}\r\nContent-Length: 0\r\n\r\n"
        );
        let out = scoped().push(req.as_bytes());
        assert_eq!(out, req.as_bytes());
    }

    #[test]
    fn both_scopes_on_one_connection() {
        let other = "shuru_tok_other";
        let mut s = RequestStream::new(vec![
            substitution(PLACEHOLDER, SECRET, Scope::Authorization),
            substitution(other, "head-value", Scope::Head),
        ]);
        let req = format!(
            "GET /x?k={other}&t={PLACEHOLDER} HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {PLACEHOLDER}\r\n\r\n"
        );
        let out = String::from_utf8(s.push(req.as_bytes())).unwrap();
        assert_eq!(
            out,
            format!("GET /x?k=head-value&t={PLACEHOLDER} HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer {SECRET}\r\n\r\n")
        );
    }

    #[test]
    fn a_request_starts_only_between_requests() {
        let mut s = stream();
        assert!(s.at_request_start());
        s.push(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\nab");
        assert!(!s.at_request_start(), "inside a body");
        s.push(b"cd");
        assert!(s.at_request_start());
        s.push(b"GET / HT");
        assert!(!s.at_request_start(), "inside a head");
    }

    #[test]
    fn replace_bytes_basics() {
        assert_eq!(
            replace_bytes(b"hello world", b"world", b"rust"),
            b"hello rust"
        );
        assert_eq!(replace_bytes(b"no match", b"xyz", b"abc"), b"no match");
        assert_eq!(replace_bytes(b"", b"x", b"y"), b"");
    }

    #[test]
    fn head_end_finds_a_terminator_split_across_reads() {
        assert_eq!(head_end(b"", b"abc\r\n\r\ndef"), Some(7));
        assert_eq!(head_end(b"abc\r\n", b"\r\ndef"), Some(2));
        assert_eq!(head_end(b"abc\r\n\r", b"\ndef"), Some(1));
        assert_eq!(head_end(b"abc", b"defg"), None);
    }
}
