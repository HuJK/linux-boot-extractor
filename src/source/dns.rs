//! Hostname resolution that works where the system resolver doesn't.
//!
//! A statically-linked (musl) binary on Android has no `/etc/resolv.conf`
//! and no access to bionic's resolver, so `getaddrinfo` fails with "Try
//! again" — which broke `lbx <https-url>` on the device. So: try the system
//! resolver first (fine on a normal host), then fall back to a tiny
//! DNS-over-UDP client that queries, in order, the servers named in
//! `$LBX_DNS` (the host app can pass the device's real resolvers there) and
//! a few well-known public resolvers.

use crate::{Error, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const PUBLIC_DNS: &[&str] = &["8.8.8.8", "1.1.1.1", "9.9.9.9"];
const TIMEOUT: Duration = Duration::from_secs(5);

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;

/// Resolve `host` to one or more IP addresses.
pub fn resolve(host: &str) -> Result<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    // System resolver — works off-Android; ignore its failures and fall back.
    if let Ok(addrs) = (host, 0u16).to_socket_addrs() {
        let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
        if !ips.is_empty() {
            return Ok(ips);
        }
    }
    let mut last_err = None;
    for server in servers() {
        for qtype in [TYPE_A, TYPE_AAAA] {
            match query(&server, host, qtype) {
                Ok(ips) if !ips.is_empty() => return Ok(ips),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
    }
    Err(Error::Http(format!(
        "could not resolve {host}{}",
        last_err.map(|e| format!(" ({e})")).unwrap_or_default()
    )))
}

/// `$LBX_DNS` resolvers (comma/space/semicolon-separated) first, then the
/// public fallbacks.
fn servers() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(env) = std::env::var("LBX_DNS") {
        for s in env.split([',', ' ', ';', '\n']) {
            if let Ok(ip) = s.trim().parse::<IpAddr>() {
                out.push(ip);
            }
        }
    }
    for s in PUBLIC_DNS {
        if let Ok(ip) = s.parse::<IpAddr>() {
            out.push(ip);
        }
    }
    out
}

/// A single-shot UDP query to one resolver.
fn query(server: &IpAddr, host: &str, qtype: u16) -> std::io::Result<Vec<IpAddr>> {
    let bind: SocketAddr = if server.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(TIMEOUT))?;
    sock.set_write_timeout(Some(TIMEOUT))?;
    sock.connect(SocketAddr::new(*server, 53))?;
    sock.send(&build_query(host, qtype))?;
    let mut buf = [0u8; 1232]; // EDNS-typical UDP payload ceiling
    let n = sock.recv(&mut buf)?;
    Ok(parse_answers(&buf[..n]))
}

fn build_query(host: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(host.len() + 18);
    q.extend_from_slice(&0x4242u16.to_be_bytes()); // id (fixed; connected socket)
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: recursion desired
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // ANCOUNT/NSCOUNT/ARCOUNT
    for label in host.split('.') {
        if label.is_empty() {
            continue;
        }
        // Labels are <=63 bytes; over-long names just won't resolve.
        q.push(label.len().min(63) as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0); // root label
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS IN
    q
}

fn parse_answers(buf: &[u8]) -> Vec<IpAddr> {
    let mut out = Vec::new();
    if buf.len() < 12 {
        return out;
    }
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut pos = skip_name(buf, 12); // the single question's name
    pos += 4; // QTYPE + QCLASS
    for _ in 0..ancount {
        pos = skip_name(buf, pos);
        if pos + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlen = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > buf.len() {
            break;
        }
        if rtype == TYPE_A && rdlen == 4 {
            out.push(IpAddr::V4(Ipv4Addr::new(buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3])));
        } else if rtype == TYPE_AAAA && rdlen == 16 {
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[pos..pos + 16]);
            out.push(IpAddr::V6(Ipv6Addr::from(o)));
        }
        pos += rdlen;
    }
    out
}

/// Advance past a DNS name (label sequence ending in 0, or a compression
/// pointer), returning the offset just after it.
fn skip_name(buf: &[u8], mut pos: usize) -> usize {
    while pos < buf.len() {
        let len = buf[pos];
        if len == 0 {
            return pos + 1;
        }
        if len & 0xc0 == 0xc0 {
            return pos + 2; // pointer terminates the name
        }
        pos += 1 + len as usize;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literal_passthrough() {
        assert_eq!(resolve("1.2.3.4").unwrap(), vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn parses_an_a_record_response() {
        // Response to a query for "a.b" with one A record 93.184.216.34.
        let mut r = Vec::new();
        r.extend_from_slice(&0x4242u16.to_be_bytes()); // id
        r.extend_from_slice(&0x8180u16.to_be_bytes()); // flags
        r.extend_from_slice(&1u16.to_be_bytes()); // QD
        r.extend_from_slice(&1u16.to_be_bytes()); // AN
        r.extend_from_slice(&[0, 0, 0, 0]); // NS, AR
        // question: 1 'a' 1 'b' 0, A, IN
        r.extend_from_slice(&[1, b'a', 1, b'b', 0]);
        r.extend_from_slice(&TYPE_A.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        // answer: name pointer to 0x0c, A, IN, ttl, rdlen 4, ip
        r.extend_from_slice(&[0xc0, 0x0c]);
        r.extend_from_slice(&TYPE_A.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&60u32.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&[93, 184, 216, 34]);
        assert_eq!(parse_answers(&r), vec!["93.184.216.34".parse::<IpAddr>().unwrap()]);
    }
}
