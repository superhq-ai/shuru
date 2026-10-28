use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

// Kernel boot messages include the command line, even before the agent starts.
pub fn output(token: String, console: bool) -> io::Result<UnixStream> {
    let (mut reader, writer) = UnixStream::pair()?;
    std::thread::Builder::new()
        .name("shuru-console".into())
        .spawn(move || {
            let mut output: Box<dyn Write> = if console {
                Box::new(io::stdout())
            } else {
                Box::new(io::stderr())
            };
            let _ = redact(&mut reader, &mut output, token.as_bytes());
        })?;
    Ok(writer)
}

fn redact(reader: &mut impl Read, writer: &mut impl Write, secret: &[u8]) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            writer.write_all(&pending)?;
            return writer.flush();
        }
        let mut output = Vec::with_capacity(n);
        for byte in &buf[..n] {
            pending.push(*byte);
            while !secret.starts_with(&pending) {
                output.push(pending[0]);
                pending.remove(0);
            }
            if pending == secret {
                output.extend_from_slice(b"[redacted]");
                pending.clear();
            }
        }
        writer.write_all(&output)?;
        writer.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_across_reads() {
        let input = b"cmdline shuru.token=abcdabcd next abcdabcd end";
        for chunk in 1..=input.len() {
            struct Chunks {
                bytes: Vec<u8>,
                chunk: usize,
            }
            impl Read for Chunks {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    let n = self.chunk.min(self.bytes.len()).min(buf.len());
                    buf[..n].copy_from_slice(&self.bytes[..n]);
                    self.bytes.drain(..n);
                    Ok(n)
                }
            }
            let mut output = Vec::new();
            redact(
                &mut Chunks {
                    bytes: input.to_vec(),
                    chunk,
                },
                &mut output,
                b"abcdabcd",
            )
            .unwrap();
            assert_eq!(
                output,
                b"cmdline shuru.token=[redacted] next [redacted] end"
            );
        }
    }
}
