//! Just enough STUN (RFC 8489) to route packets to WebRTC sessions and to
//! learn this machine's public address from a STUN server.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub type TransactionId = [u8; 12];

/// Whether `packet` looks like a STUN message.
pub fn is_stun(packet: &[u8]) -> bool {
    packet.len() >= 20 && packet[0] & 0xc0 == 0 && packet[4..8] == MAGIC_COOKIE
}

fn message_type(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[0], packet[1]])
}

fn transaction_id(packet: &[u8]) -> &[u8] {
    &packet[8..20]
}

/// The attributes of a STUN message as (type, value) pairs.
fn attributes(packet: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let mut rest = packet.get(20..20 + len).unwrap_or(&[]);
    std::iter::from_fn(move || {
        if rest.len() < 4 {
            return None;
        }
        let kind = u16::from_be_bytes([rest[0], rest[1]]);
        let len = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let value = rest.get(4..4 + len)?;
        // Attributes are padded to a multiple of four bytes.
        rest = rest.get((4 + len).next_multiple_of(4)..).unwrap_or(&[]);
        Some((kind, value))
    })
}

/// The ICE username fragment a binding request is addressed to: the part of
/// USERNAME before the colon, which is the receiver's own ufrag.
pub fn request_ufrag(packet: &[u8]) -> Option<&str> {
    if !is_stun(packet) || message_type(packet) != BINDING_REQUEST {
        return None;
    }
    let (_, username) = attributes(packet).find(|(kind, _)| *kind == ATTR_USERNAME)?;
    let username = std::str::from_utf8(username).ok()?;
    Some(
        username
            .split_once(':')
            .map_or(username, |(local, _)| local),
    )
}

/// A binding request with no attributes, as sent to a STUN server.
pub fn binding_request(id: &TransactionId) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20);
    packet.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&MAGIC_COOKIE);
    packet.extend_from_slice(id);
    packet
}

/// The address in a binding success response to the request with `id`.
pub fn mapped_address(packet: &[u8], id: &TransactionId) -> Option<SocketAddr> {
    if !is_stun(packet) || message_type(packet) != BINDING_SUCCESS || transaction_id(packet) != id {
        return None;
    }
    let (_, value) = attributes(packet).find(|(kind, _)| *kind == ATTR_XOR_MAPPED_ADDRESS)?;
    if value.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([value[2], value[3]]) ^ 0x2112;
    let ip = match value[1] {
        0x01 => {
            let mut octets: [u8; 4] = value[4..8].try_into().ok()?;
            for (o, m) in octets.iter_mut().zip(MAGIC_COOKIE) {
                *o ^= m;
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        0x02 if value.len() >= 20 => {
            let mut octets: [u8; 16] = value[4..20].try_into().ok()?;
            let mask = MAGIC_COOKIE.iter().chain(id.iter());
            for (o, m) in octets.iter_mut().zip(mask) {
                *o ^= m;
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u16, id: &TransactionId, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, v) in attrs {
            body.extend_from_slice(&t.to_be_bytes());
            body.extend_from_slice(&(v.len() as u16).to_be_bytes());
            body.extend_from_slice(v);
            body.resize(body.len().next_multiple_of(4), 0);
        }
        let mut packet = Vec::new();
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&(body.len() as u16).to_be_bytes());
        packet.extend_from_slice(&MAGIC_COOKIE);
        packet.extend_from_slice(id);
        packet.extend_from_slice(&body);
        packet
    }

    #[test]
    fn finds_the_receivers_ufrag() {
        let id = [7; 12];
        let packet = message(
            BINDING_REQUEST,
            &id,
            &[(0x0024, &[0, 0, 0, 1]), (ATTR_USERNAME, b"srv1:brw22")],
        );
        assert!(is_stun(&packet));
        assert_eq!(request_ufrag(&packet), Some("srv1"));
        let response = message(BINDING_SUCCESS, &id, &[(ATTR_USERNAME, b"srv1:brw22")]);
        assert_eq!(request_ufrag(&response), None);
        assert!(!is_stun(&[0x80; 40]), "RTP is not STUN");
    }

    #[test]
    fn decodes_xor_mapped_addresses() {
        let id = [3; 12];
        // 192.0.2.1:32853 XORed with the magic cookie.
        let v4 = [0, 1, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43];
        let packet = message(BINDING_SUCCESS, &id, &[(ATTR_XOR_MAPPED_ADDRESS, &v4)]);
        assert_eq!(
            mapped_address(&packet, &id),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert_eq!(mapped_address(&packet, &[4; 12]), None, "wrong transaction");
        assert_eq!(binding_request(&id).len(), 20);
        assert!(is_stun(&binding_request(&id)));
    }
}
