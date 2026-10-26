//! Redaction of secrets out of the upstream to guest response stream.
//!
//! Substitution on the way out is only half of the promise that the real
//! value never enters the VM. An upstream that reflects the request, which a
//! debug endpoint or a verbose error page will do, would hand the credential
//! straight back. This replaces every real value with its placeholder on the
//! way back, so the guest sees the same token it sent.
//!
//! Responses are treated as bytes, not HTTP. A replacement that only ever
//! touches an exact match of the value needs no framing to stay safe, and a
//! response that is not HTTP at all still gets scrubbed. Two things the scan
//! cannot see: a value split across chunk boundaries, and a value that comes
//! back transformed. Compression, the one transformation an honest upstream
//! applies on its own, is closed off on the request side by asking for an
//! identity response whenever a request carried a secret. The rest need the
//! upstream to go out of its way.
//!
//! See docs/rfcs/0002-refreshable-secrets.md, appendix B.

/// Values shorter than this are not redacted. A tiny value would match
/// ordinary response text and corrupt it, and is not a credential anyway.
const MIN_VALUE_LEN: usize = 8;

pub(crate) struct Redactor {
    /// Real value to placeholder, longest value first so that a value which
    /// is a prefix of another never wins over the longer one.
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// Trailing bytes of the last push that could still turn into a value.
    held: Vec<u8>,
}

impl Redactor {
    pub(crate) fn new(substitutions: Vec<(String, String)>) -> Self {
        let mut redactor = Self {
            pairs: Vec::new(),
            held: Vec::new(),
        };
        redactor.add(substitutions);
        redactor
    }

    /// Learn more values. Old ones stay: a rotated credential echoed back is
    /// still a credential.
    pub(crate) fn add(&mut self, substitutions: Vec<(String, String)>) {
        for (placeholder, value) in substitutions {
            let value = value.into_bytes();
            if value.len() < MIN_VALUE_LEN || self.pairs.iter().any(|(v, _)| *v == value) {
                continue;
            }
            self.pairs.push((value, placeholder.into_bytes()));
        }
        self.pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// Consume upstream bytes, returning what to forward to the guest.
    ///
    /// Holds back only the shortest suffix that is still a prefix of some
    /// value, which for a credential-shaped value is almost always nothing,
    /// so a streaming response is not noticeably delayed.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.pairs.is_empty() {
            return chunk.to_vec();
        }

        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(chunk);

        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            let rest = &data[i..];
            if let Some((value, placeholder)) = self.pairs.iter().find(|(v, _)| rest.starts_with(v))
            {
                out.extend_from_slice(placeholder);
                i += value.len();
                continue;
            }
            if self
                .pairs
                .iter()
                .any(|(v, _)| v.len() > rest.len() && v.starts_with(rest))
            {
                self.held.extend_from_slice(rest);
                break;
            }
            out.push(data[i]);
            i += 1;
        }
        out
    }

    /// Everything still held, for when the upstream closes.
    pub(crate) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLACEHOLDER: &str = "shuru_tok_abc123";
    const SECRET: &str = "sk-a-much-longer-real-secret-value";

    fn redactor() -> Redactor {
        Redactor::new(vec![(PLACEHOLDER.to_string(), SECRET.to_string())])
    }

    /// Feed `input` in slices of `size`, then flush, returning everything forwarded.
    fn feed(r: &mut Redactor, input: &[u8], size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for piece in input.chunks(size.max(1)) {
            out.extend(r.push(piece));
        }
        out.extend(r.flush());
        out
    }

    fn echo_response() -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{{\"headers\":{{\"authorization\":\"Bearer {SECRET}\"}},\"again\":\"{SECRET}\"}}"
        )
    }

    #[test]
    fn a_reflected_value_comes_back_as_the_placeholder() {
        let out = feed(&mut redactor(), echo_response().as_bytes(), usize::MAX);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            echo_response().replace(SECRET, PLACEHOLDER)
        );
    }

    #[test]
    fn redacts_at_every_split_point() {
        let input = echo_response();
        for size in 1..input.len() {
            let out = feed(&mut redactor(), input.as_bytes(), size);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                input.replace(SECRET, PLACEHOLDER),
                "leaked at chunk size {size}"
            );
        }
    }

    #[test]
    fn holds_only_a_suffix_that_could_become_the_value() {
        let mut r = redactor();
        let first = r.push(b"data: hello sk-a");
        assert_eq!(first, b"data: hello ");
        let second = r.push(b"-much-longer-real-secret-value\n\n");
        assert_eq!(second, format!("{PLACEHOLDER}\n\n").as_bytes());
    }

    #[test]
    fn a_false_start_is_released_on_the_next_push() {
        let mut r = redactor();
        assert_eq!(r.push(b"event: sk-a"), b"event: ");
        assert_eq!(r.push(b"ok\n"), b"sk-aok\n");
    }

    #[test]
    fn a_false_start_is_released_on_close() {
        let mut r = redactor();
        assert_eq!(r.push(b"trailing sk-a-much"), b"trailing ");
        assert_eq!(r.flush(), b"sk-a-much");
    }

    #[test]
    fn nothing_is_held_when_the_tail_cannot_start_a_value() {
        let mut r = redactor();
        let out = r.push(b"data: {\"ok\":true}\n\n");
        assert_eq!(out, b"data: {\"ok\":true}\n\n");
        assert!(r.flush().is_empty());
    }

    #[test]
    fn rotated_values_are_still_redacted_after_a_refresh() {
        let mut r = redactor();
        r.add(vec![(
            PLACEHOLDER.to_string(),
            "sk-rotated-value-number-two".to_string(),
        )]);
        let out = r.push(format!("old={SECRET} new=sk-rotated-value-number-two").as_bytes());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("old={PLACEHOLDER} new={PLACEHOLDER}")
        );
    }

    #[test]
    fn the_longer_of_two_overlapping_values_wins() {
        let mut r = Redactor::new(vec![
            ("tok_short".to_string(), "secret-prefix".to_string()),
            ("tok_long".to_string(), "secret-prefix-and-more".to_string()),
        ]);
        let out = feed(
            &mut r,
            b"a secret-prefix-and-more b secret-prefix c",
            usize::MAX,
        );
        assert_eq!(out, b"a tok_long b tok_short c");
    }

    #[test]
    fn short_values_are_left_alone() {
        let mut r = Redactor::new(vec![("tok".to_string(), "abc".to_string())]);
        assert_eq!(r.push(b"abc abc"), b"abc abc");
    }

    #[test]
    fn no_values_passes_bytes_through() {
        let mut r = Redactor::new(vec![]);
        assert_eq!(r.push(b"anything at all"), b"anything at all");
        assert!(r.flush().is_empty());
    }
}
