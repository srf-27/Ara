//! Minimal RFC 5389 STUN binding (request/response, XOR-MAPPED-ADDRESS only).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub fn encode_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    buf[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    buf[8..20].copy_from_slice(txid);
    buf
}

pub fn parse_request(buf: &[u8]) -> Option<[u8; 12]> {
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_REQUEST {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC {
        return None;
    }
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&buf[8..20]);
    Some(txid)
}

pub fn encode_response(txid: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
    let mut attr = Vec::with_capacity(24);
    attr.push(0); // reserved
    let xor_port = addr.port() ^ (MAGIC >> 16) as u16;
    match addr.ip() {
        IpAddr::V4(v4) => {
            attr.push(0x01);
            attr.extend_from_slice(&xor_port.to_be_bytes());
            let x: u32 = u32::from(v4) ^ MAGIC;
            attr.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(v6) => {
            attr.push(0x02);
            attr.extend_from_slice(&xor_port.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[0..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..16].copy_from_slice(txid);
            let o = v6.octets();
            for i in 0..16 {
                attr.push(o[i] ^ mask[i]);
            }
        }
    }
    let mut buf = Vec::with_capacity(20 + 4 + attr.len());
    buf.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
    buf.extend_from_slice(&(4 + attr.len() as u16).to_be_bytes());
    buf.extend_from_slice(&MAGIC.to_be_bytes());
    buf.extend_from_slice(txid);
    buf.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
    buf.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    buf.extend_from_slice(&attr);
    buf
}

pub fn parse_response(buf: &[u8]) -> Option<SocketAddr> {
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_RESPONSE {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC {
        return None;
    }
    let txid = &buf[8..20];
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < 20 + len {
        return None;
    }
    let mut pos = 20;
    while pos + 4 <= 20 + len {
        let ty = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let alen = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        let val = &buf[pos + 4..pos + 4 + alen];
        if ty == XOR_MAPPED_ADDRESS && alen >= 4 {
            let port = u16::from_be_bytes([val[2], val[3]]) ^ (MAGIC >> 16) as u16;
            match val[1] {
                0x01 if alen >= 8 => {
                    let x = u32::from_be_bytes([val[4], val[5], val[6], val[7]]) ^ MAGIC;
                    return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(x)), port));
                }
                0x02 if alen >= 20 => {
                    let mut mask = [0u8; 16];
                    mask[0..4].copy_from_slice(&MAGIC.to_be_bytes());
                    mask[4..16].copy_from_slice(txid);
                    let mut o = [0u8; 16];
                    for i in 0..16 {
                        o[i] = val[4 + i] ^ mask[i];
                    }
                    return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port));
                }
                _ => {}
            }
        }
        pos += 4 + alen + ((4 - alen % 4) % 4); // attributes are 32-bit padded
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let txid = [7u8; 12];
        let req = encode_request(&txid);
        assert_eq!(parse_request(&req), Some(txid));
        assert_eq!(parse_request(&[0u8; 20]), None);
    }

    #[test]
    fn response_roundtrip_v4() {
        let txid = [9u8; 12];
        let addr: SocketAddr = "203.0.113.7:41641".parse().unwrap();
        let resp = encode_response(&txid, addr);
        assert_eq!(parse_response(&resp), Some(addr));
    }

    #[test]
    fn response_roundtrip_v6() {
        let txid = [3u8; 12];
        let addr: SocketAddr = "[2001:db8::1]:23947".parse().unwrap();
        let resp = encode_response(&txid, addr);
        assert_eq!(parse_response(&resp), Some(addr));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_response(&[0xff; 40]), None);
        assert_eq!(parse_response(&[]), None);
    }
}