//! A stand-in HTTP/1.1 server's end of a connection, such as a stand-in model provider's.
//!
//! A stand-in serves only a documented network API that Pumpkin speaks as a client, as cargo's
//! tests serve a registry over crates.io's documented protocol:
//! <https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-test-support/src/registry.rs#L790-L834>.

use std::io::{BufRead, Write as _};
use std::net::TcpStream;

/// Reads a request's line, its headers, and the body of the length its `content-length` gives.
pub fn read_request(reader: &mut impl BufRead) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let Some((name, value)) = header.trim_end().split_once(": ") else {
            break;
        };
        if name == "content-length" {
            length = value.parse().unwrap();
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (line.trim_end().to_owned(), headers, body)
}

/// Writes `text` as one chunk of a chunked body: the last, if it is empty.
pub fn write_chunk(connection: &mut TcpStream, text: &str) {
    write!(connection, "{:x}\r\n{text}\r\n", text.len()).unwrap();
}
