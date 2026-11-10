//! Bounded loopback HTTP fixtures; never wait indefinitely for a missing request.
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub(crate) fn accept(listener: &TcpListener) -> io::Result<TcpStream> {
    accept_with_timeout(listener, Duration::from_secs(10))
}

fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "test server received no request",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut chunk = [0; 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "test request headers took too long",
                )
            })?;
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete test request",
            ));
        }
        data.extend_from_slice(&chunk[..n]);
        if data.len() > 65536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "test request headers too large",
            ));
        }
        if data.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return Ok(data);
        }
    }
}

#[test]
fn missing_connection_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let error = accept_with_timeout(&listener, Duration::from_millis(25)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn fragmented_headers_are_read_completely() {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || request(&mut accept(&listener).unwrap()).unwrap());
    let mut client = TcpStream::connect(address).unwrap();
    client.write_all(b"GET / HTTP/1.1\r\nHost: local").unwrap();
    std::thread::sleep(Duration::from_millis(10));
    client.write_all(b"host\r\n\r\n").unwrap();
    assert_eq!(
        server.join().unwrap(),
        b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
}
