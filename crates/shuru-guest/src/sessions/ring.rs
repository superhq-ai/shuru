//! The output of a session: its newest bytes of stdout and stderr as one
//! sequence, each byte at an offset counted from the start of the session,
//! with the stream of each run kept apart from the bytes.

use std::collections::VecDeque;

use shuru_proto::frame;

pub const BUFFER: u64 = 1 << 20;

/// Output that switches stream on every byte would otherwise take one
/// entry per byte; past this many runs the oldest are dropped.
const MAX_RUNS: usize = 16 * 1024;

pub struct Ring {
    bytes: VecDeque<u8>,
    runs: VecDeque<(u8, usize)>,
    start: u64,
    end: u64,
}

impl Ring {
    pub fn new() -> Self {
        Ring {
            bytes: VecDeque::new(),
            runs: VecDeque::new(),
            start: 0,
            end: 0,
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn push(&mut self, stream: u8, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.bytes.extend(data);
        self.end += data.len() as u64;
        match self.runs.back_mut() {
            Some((s, len)) if *s == stream => *len += data.len(),
            _ => self.runs.push_back((stream, data.len())),
        }
        let over = (self.end - self.start).saturating_sub(BUFFER) as usize;
        self.drop_front(over);
        while self.runs.len() > MAX_RUNS {
            let oldest = self.runs[0].1;
            self.drop_front(oldest);
        }
    }

    fn drop_front(&mut self, n: usize) {
        self.bytes.drain(..n);
        self.start += n as u64;
        let mut left = n;
        while left > 0 {
            let Some(front) = self.runs.front_mut() else {
                break;
            };
            if front.1 <= left {
                left -= front.1;
                self.runs.pop_front();
            } else {
                front.1 -= left;
                left = 0;
            }
        }
    }

    /// How many bytes of `stream` fit without dropping any at or after
    /// `keep`.
    pub fn room(&self, stream: u8, keep: u64) -> usize {
        let room = (keep + BUFFER).saturating_sub(self.end) as usize;
        let new_run = self.runs.back().map(|r| r.0) != Some(stream);
        if new_run && self.runs.len() >= MAX_RUNS && self.start + self.runs[0].1 as u64 > keep {
            return 0;
        }
        room
    }

    /// Appends the output from `from` as STDOUT and STDERR frames, at most
    /// `max` bytes of it, and returns the offset reached. `from` is at or
    /// after the start.
    pub fn frames(&self, from: u64, max: usize, out: &mut Vec<u8>) -> u64 {
        let mut at = from.max(self.start);
        let mut left = max;
        let mut run_start = self.start;
        for &(stream, len) in &self.runs {
            let run_end = run_start + len as u64;
            if left == 0 {
                break;
            }
            while run_end > at && left > 0 {
                let skip = (at - run_start) as usize;
                let take = (len - skip).min(left).min(frame::MAX_FRAME as usize - 1);
                out.extend_from_slice(&(take as u32 + 1).to_be_bytes());
                out.push(stream);
                self.copy((run_start - self.start) as usize + skip, take, out);
                at += take as u64;
                left -= take;
            }
            run_start = run_end;
        }
        at
    }

    fn copy(&self, from: usize, len: usize, out: &mut Vec<u8>) {
        let (a, b) = self.bytes.as_slices();
        let end = from + len;
        if from < a.len() {
            out.extend_from_slice(&a[from..end.min(a.len())]);
        }
        if end > a.len() {
            out.extend_from_slice(&b[from.saturating_sub(a.len())..end - a.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(parts: &[(u8, &[u8])]) -> Ring {
        let mut r = Ring::new();
        for (stream, data) in parts {
            r.push(*stream, data);
        }
        r
    }

    fn frames(r: &Ring, from: u64, max: usize) -> (Vec<(u8, Vec<u8>)>, u64) {
        let mut out = Vec::new();
        let at = r.frames(from, max, &mut out);
        let mut cursor = std::io::Cursor::new(out);
        let mut got = Vec::new();
        while let Some(f) = frame::read_frame(&mut cursor).unwrap() {
            got.push(f);
        }
        (got, at)
    }

    #[test]
    fn frames_from_an_offset_inside_a_run() {
        let r = ring(&[(frame::STDOUT, b"hello "), (frame::STDERR, b"world")]);
        let (got, at) = frames(&r, 3, 100);
        assert_eq!(
            got,
            vec![
                (frame::STDOUT, b"lo ".to_vec()),
                (frame::STDERR, b"world".to_vec())
            ]
        );
        assert_eq!(at, 11);
        assert_eq!(frames(&r, 11, 100), (vec![], 11));
        let (got, at) = frames(&r, 2, 5);
        assert_eq!(
            got,
            vec![
                (frame::STDOUT, b"llo ".to_vec()),
                (frame::STDERR, b"w".to_vec())
            ]
        );
        assert_eq!(at, 7);
    }

    #[test]
    fn runs_of_one_stream_merge() {
        let r = ring(&[(frame::STDOUT, b"ab"), (frame::STDOUT, b"cd")]);
        assert_eq!(r.runs.len(), 1);
        assert_eq!(
            frames(&r, 0, 100).0,
            vec![(frame::STDOUT, b"abcd".to_vec())]
        );
    }

    #[test]
    fn keeps_the_newest_buffer() {
        let chunk = vec![b'x'; 64 * 1024];
        let mut r = Ring::new();
        for _ in 0..(BUFFER as usize / chunk.len() + 3) {
            r.push(frame::STDOUT, &chunk);
        }
        assert_eq!(r.end() - r.start(), BUFFER);
        assert_eq!(r.start(), 3 * chunk.len() as u64);
        assert_eq!(r.bytes.len() as u64, BUFFER);
        let (got, _) = frames(&r, 0, 10);
        assert_eq!(got[0].1.len(), 10);
    }

    #[test]
    fn copies_across_the_wrap() {
        let mut r = Ring::new();
        let mut want = Vec::new();
        for i in 0..40u32 {
            let part: Vec<u8> = (0..50_000u32).map(|j| (i * 7 + j) as u8).collect();
            r.push(frame::STDOUT, &part);
            want.extend(part);
        }
        let tail = &want[want.len() - BUFFER as usize..];
        let (got, at) = frames(&r, r.start(), BUFFER as usize);
        let bytes: Vec<u8> = got.into_iter().flat_map(|(_, d)| d).collect();
        assert_eq!(bytes, tail);
        assert_eq!(at, r.end());
    }

    #[test]
    fn interleaved_streams_are_bounded_by_runs() {
        let mut r = Ring::new();
        for i in 0..(MAX_RUNS + 10) {
            let stream = if i % 2 == 0 {
                frame::STDOUT
            } else {
                frame::STDERR
            };
            r.push(stream, b"x");
        }
        assert_eq!(r.runs.len(), MAX_RUNS);
        assert_eq!(r.start(), 10);
        assert_eq!(r.bytes.len(), MAX_RUNS);
    }

    #[test]
    fn room_keeps_unsent_bytes() {
        let mut r = Ring::new();
        r.push(frame::STDOUT, &vec![0; BUFFER as usize - 100]);
        assert_eq!(r.room(frame::STDOUT, 0), 100);
        assert_eq!(r.room(frame::STDOUT, 50), 150);
        r.push(frame::STDOUT, &[0; 100]);
        assert_eq!(r.room(frame::STDOUT, 0), 0);
        assert_eq!(r.start(), 0);
    }

    #[test]
    fn room_counts_a_new_run_past_the_limit() {
        let mut r = Ring::new();
        for i in 0..MAX_RUNS {
            let stream = if i % 2 == 0 {
                frame::STDOUT
            } else {
                frame::STDERR
            };
            r.push(stream, b"x");
        }
        // MAX_RUNS is even, so the last run is stderr.
        assert!(r.room(frame::STDERR, 0) > 0);
        assert_eq!(r.room(frame::STDOUT, 0), 0);
        assert!(r.room(frame::STDOUT, 1) > 0);
    }
}
