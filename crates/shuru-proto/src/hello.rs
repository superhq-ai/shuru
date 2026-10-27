use std::io::{self, Read, Write};

use subtle::ConstantTimeEq;

use crate::frame;

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy)]
pub struct Identity {
    pub token: [u8; 32],
    pub instance: [u8; 16],
}

fn rejected() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "guest identity handshake failed",
    )
}

// Fixed length reads keep unauthenticated peers from allocating frame buffers.
fn read_hello<const N: usize>(stream: &mut impl Read, kind: u8) -> io::Result<[u8; N]> {
    let mut header = [0; 5];
    stream.read_exact(&mut header)?;
    if u32::from_be_bytes(header[..4].try_into().unwrap()) != N as u32 + 1 || header[4] != kind {
        return Err(rejected());
    }
    let mut payload = [0; N];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

impl Identity {
    pub fn accept(&self, stream: &mut (impl Read + Write)) -> io::Result<()> {
        let token = read_hello::<32>(stream, frame::HELLO)?;
        if !bool::from(self.token.ct_eq(&token)) {
            return Err(rejected());
        }
        frame::write_frame(stream, frame::HELLO_OK, &self.instance)
    }

    pub fn connect(&self, stream: &mut (impl Read + Write)) -> io::Result<()> {
        frame::write_frame(stream, frame::HELLO, &self.token)?;
        let instance = read_hello::<16>(stream, frame::HELLO_OK)?;
        if instance != self.instance {
            return Err(rejected());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Stream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn stream(kind: u8, payload: &[u8]) -> Stream {
        let mut input = Vec::new();
        frame::write_frame(&mut input, kind, payload).unwrap();
        Stream {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }
    const ID: Identity = Identity {
        token: [42; 32],
        instance: [7; 16],
    };

    #[test]
    fn hello_roundtrip() {
        let mut guest = stream(frame::HELLO, &ID.token);
        ID.accept(&mut guest).unwrap();
        let mut host = Stream {
            input: Cursor::new(guest.output),
            output: Vec::new(),
        };
        ID.connect(&mut host).unwrap();
        assert_eq!(
            frame::read_frame(&mut host.output.as_slice()).unwrap(),
            Some((frame::HELLO, ID.token.to_vec()))
        );
    }

    #[test]
    fn reject_wrong_token_type_length_and_truncation() {
        for index in 0..32 {
            let mut token = ID.token;
            token[index] ^= 1;
            let mut peer = stream(frame::HELLO, &token);
            assert!(ID.accept(&mut peer).is_err());
            assert!(peer.output.is_empty());
        }
        for (kind, payload) in [
            (frame::EXEC_REQ, ID.token.as_slice()),
            (frame::HELLO, &[0][..]),
        ] {
            let mut peer = stream(kind, payload);
            assert!(ID.accept(&mut peer).is_err());
            assert!(peer.output.is_empty());
        }
        let mut peer = stream(frame::HELLO, &ID.token);
        peer.input.get_mut().pop();
        assert!(ID.accept(&mut peer).is_err());
    }

    #[test]
    fn reject_wrong_instance_and_reply_type() {
        assert!(ID.connect(&mut stream(frame::HELLO_OK, &[0; 16])).is_err());
        assert!(ID
            .connect(&mut stream(frame::STDOUT, &ID.instance))
            .is_err());
    }
}
