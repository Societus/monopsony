//! STUN-based NAT detection
//!
//! Determines the server's external IP address and NAT type
//! by contacting public STUN servers. Used by the tracker
//! announce task to provide accurate endpoint information.

use std::net::{SocketAddr, UdpSocket};
use tracing::{debug, info, warn};

/// Well-known public STUN servers
#[allow(dead_code)] // Will be used when STUN NAT detection is exposed via CLI
const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

/// NAT type classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Will be used when STUN NAT detection is exposed via CLI
pub enum NatType {
    /// No NAT — public IP directly reachable
    Public,
    /// Full cone NAT — any external host can reach mapped port
    #[allow(dead_code)]
    FullCone,
    /// Restricted cone NAT
    #[allow(dead_code)]
    RestrictedCone,
    /// Port-restricted cone NAT
    #[allow(dead_code)]
    PortRestrictedCone,
    /// Symmetric NAT — different mapping per destination
    Symmetric,
    /// Could not determine
    Unknown,
}

impl std::fmt::Display for NatType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NatType::Public => write!(f, "public"),
            NatType::FullCone => write!(f, "full_cone"),
            NatType::RestrictedCone => write!(f, "restricted_cone"),
            NatType::PortRestrictedCone => write!(f, "port_restricted_cone"),
            NatType::Symmetric => write!(f, "symmetric"),
            NatType::Unknown => write!(f, "unknown"),
        }
    }
}

/// Result of NAT detection
#[derive(Debug, Clone)]
#[allow(dead_code)] // Will be used when STUN NAT detection is exposed via CLI
pub struct NatInfo {
    /// The server's external (public) IP address
    pub external_addr: Option<SocketAddr>,
    /// Detected NAT type
    pub nat_type: NatType,
    /// Local address used for the STUN binding
    pub local_addr: SocketAddr,
}

/// Encode a STUN Binding Request (RFC 5389)
#[allow(dead_code)] // STUN binding request encoder, will be used by CLI NAT detection
fn encode_binding_request() -> [u8; 20] {
    let mut buf = [0u8; 20];
    // Message Type: 0x0001 (Binding Request)
    buf[0] = 0x00;
    buf[1] = 0x01;
    // Message Length: 0 (no attributes)
    buf[2] = 0x00;
    buf[3] = 0x00;
    // Magic Cookie: 0x2112A442
    buf[4] = 0x21;
    buf[5] = 0x12;
    buf[6] = 0xA4;
    buf[7] = 0x42;
    // Transaction ID: 12 random bytes
    for byte in buf.iter_mut().skip(8) {
        *byte = rand_byte();
    }
    buf
}

#[allow(dead_code)] // Random byte for STUN transaction ID
fn rand_byte() -> u8 {
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (t as u8).wrapping_add(std::process::id() as u8)
}

/// Parse STUN Binding Response to extract XOR-MAPPED-ADDRESS
#[allow(dead_code)] // STUN XOR-MAPPED-ADDRESS parser, used by NAT detection
fn parse_xor_mapped_address(data: &[u8], transaction_id: &[u8]) -> Option<SocketAddr> {
    if data.len() < 20 {
        return None;
    }

    // Verify message type is Binding Response (0x0101)
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != 0x0101 {
        return None;
    }

    // Verify magic cookie
    if data[4..8] != [0x21, 0x12, 0xA4, 0x42] {
        return None;
    }

    // Verify transaction ID matches
    if data[8..20] != transaction_id[..] {
        return None;
    }

    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < 20 + msg_len {
        return None;
    }

    // Parse attributes
    let mut offset = 20;
    while offset + 4 <= 20 + msg_len {
        let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;

        // XOR-MAPPED-ADDRESS: 0x0020 (RFC 5389)
        // MAPPED-ADDRESS: 0x0001 (RFC 3489, fallback)
        if attr_type == 0x0020 || attr_type == 0x0001 {
            if offset + 4 + attr_len > data.len() {
                return None;
            }
            let attr_data = &data[offset + 4..offset + 4 + attr_len];
            if attr_data.len() < 4 {
                return None;
            }

            let family = attr_data[1];
            let (port, ip) = if family == 0x01 {
                // IPv4
                if attr_data.len() < 8 {
                    return None;
                }
                let raw_port = u16::from_be_bytes([attr_data[2], attr_data[3]]);
                let raw_ip =
                    u32::from_be_bytes([attr_data[4], attr_data[5], attr_data[6], attr_data[7]]);

                let port = if attr_type == 0x0020 {
                    raw_port ^ 0x2112
                } else {
                    raw_port
                };
                let ip = if attr_type == 0x0020 {
                    raw_ip ^ 0x2112A442
                } else {
                    raw_ip
                };

                let ip_addr = std::net::Ipv4Addr::from(ip);
                (port, std::net::IpAddr::V4(ip_addr))
            } else if family == 0x02 {
                // IPv6
                if attr_data.len() < 20 {
                    return None;
                }
                let raw_port = u16::from_be_bytes([attr_data[2], attr_data[3]]);
                let port = if attr_type == 0x0020 {
                    raw_port ^ 0x2112
                } else {
                    raw_port
                };

                let mut ip_bytes = [0u8; 16];
                let cookie = [0x21, 0x12, 0xA4, 0x42];
                for i in 0..16 {
                    let xor = if i < 4 {
                        cookie[i]
                    } else {
                        transaction_id.get(i - 4).copied().unwrap_or(0)
                    };
                    ip_bytes[i] = attr_data[4 + i] ^ xor;
                }
                let ip_addr = std::net::Ipv6Addr::from(ip_bytes);
                (port, std::net::IpAddr::V6(ip_addr))
            } else {
                return None;
            };

            return Some(SocketAddr::new(ip, port));
        }

        // Attributes are padded to 4-byte boundary
        let padded_len = (attr_len + 3) & !3;
        offset += 4 + padded_len;
    }

    None
}

/// Resolve STUN server hostname to socket address
#[allow(dead_code)] // STUN server DNS resolver, used by NAT detection
fn resolve_stun_server(server: &str) -> Option<SocketAddr> {
    let parts: Vec<&str> = server.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let port = parts[0].parse::<u16>().ok()?;
    let host = parts[1];

    // Use std::net for DNS resolution
    let addr = format!("{}:{}", host, port);
    std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .ok()?
        .find(|a| a.is_ipv4()) // Prefer IPv4 for STUN
        .or_else(|| std::net::ToSocketAddrs::to_socket_addrs(&addr).ok()?.next())
}

/// Detect external IP address via STUN binding request
#[allow(dead_code)] // Will be wired to CLI `server detect-nat` subcommand
pub fn detect_external_addr() -> Option<SocketAddr> {
    for server in STUN_SERVERS {
        if let Some(addr) = probe_stun_server(server) {
            debug!("STUN: external addr {} via {}", addr, server);
            return Some(addr);
        }
    }
    warn!("STUN: all servers failed, cannot determine external address");
    None
}

/// Probe a single STUN server
#[allow(dead_code)] // STUN probe helper, used by detect_external_addr
fn probe_stun_server(server: &str) -> Option<SocketAddr> {
    let stun_addr = resolve_stun_server(server)?;
    debug!("STUN: resolved {} -> {}", server, stun_addr);

    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    let _local_addr = socket.local_addr().ok()?;

    let request = encode_binding_request();
    let transaction_id = &request[8..20];

    socket.send_to(&request, stun_addr).ok()?;

    let mut buf = [0u8; 576];
    let n = socket.recv(&mut buf).ok()?;

    parse_xor_mapped_address(&buf[..n], transaction_id)
}

/// Full NAT detection — determines external IP and NAT type
#[allow(dead_code)] // Will be wired to CLI `server detect-nat` subcommand
pub fn detect_nat() -> NatInfo {
    let local_addr = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| s.local_addr().ok())
        .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());

    // Try to get external address
    let external_addr = detect_external_addr();

    let nat_type = match &external_addr {
        Some(ext) => {
            // If external IP == local IP, we're public
            if ext.ip().is_unspecified() {
                NatType::Unknown
            } else if !local_addr.ip().is_unspecified()
                && !local_addr.ip().is_loopback()
                && local_addr.ip() == ext.ip()
            {
                NatType::Public
            } else {
                // Simplified: we detect symmetric NAT by comparing
                // mapped addresses from two different STUN servers
                detect_nat_type()
            }
        }
        None => NatType::Unknown,
    };

    info!(
        "NAT detection: external={:?}, type={}",
        external_addr, nat_type
    );

    NatInfo {
        external_addr,
        nat_type,
        local_addr,
    }
}

/// Detect NAT type by comparing mapped addresses from multiple STUN servers
#[allow(dead_code)] // NAT type classifier, used by detect_nat
fn detect_nat_type() -> NatType {
    let mut mapped_addrs: Vec<SocketAddr> = Vec::new();

    for server in STUN_SERVERS.iter().take(2) {
        if let Some(addr) = probe_stun_server(server) {
            mapped_addrs.push(addr);
        }
    }

    match mapped_addrs.len() {
        0 => NatType::Unknown,
        1 => NatType::Unknown, // Can't determine with only one response
        _ => {
            // If the two STUN servers report different mapped ports,
            // it's a symmetric NAT
            if mapped_addrs[0].port() != mapped_addrs[1].port() {
                NatType::Symmetric
            } else {
                // Same port from both — likely cone NAT
                // Full classification requires Test II/III (RFC 3489)
                // which needs the STUN server to send back to us.
                // Default to port-restricted (safest assumption)
                NatType::PortRestrictedCone
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stun_request_encoding() {
        let req = encode_binding_request();
        // Message type: Binding Request
        assert_eq!(req[0], 0x00);
        assert_eq!(req[1], 0x01);
        // Message length: 0
        assert_eq!(req[2], 0x00);
        assert_eq!(req[3], 0x00);
        // Magic cookie
        assert_eq!(&req[4..8], &[0x21, 0x12, 0xA4, 0x42]);
    }

    #[test]
    fn test_parse_xor_mapped_address_ipv4() {
        // Construct a minimal STUN Binding Response with XOR-MAPPED-ADDRESS
        let transaction_id: [u8; 12] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
        ];

        // XOR-MAPPED-ADDRESS attribute: 203.0.113.1:12345
        // Port XOR: 12345 ^ 0x2112 = 0x309D ^ 0x2112 = 0x118F = 4495
        // Actually: 12345 = 0x3039, 0x3039 ^ 0x2112 = 0x112B
        let port = 12345u16;
        let ip: u32 = std::net::Ipv4Addr::new(203, 0, 113, 1).into();

        let xor_port = port.to_be_bytes();
        let xor_port_val = u16::from_be_bytes(xor_port) ^ 0x2112;
        let xor_ip = u32::from_be_bytes(ip.to_be_bytes()) ^ 0x2112A442;

        let mut attr_data = vec![0u8; 8];
        attr_data[0] = 0x00; // reserved
        attr_data[1] = 0x01; // IPv4 family
        attr_data[2..4].copy_from_slice(&xor_port_val.to_be_bytes());
        attr_data[4..8].copy_from_slice(&xor_ip.to_be_bytes());

        let mut response = Vec::new();
        // Header
        response.extend_from_slice(&[0x01, 0x01]); // Binding Response
        response.extend_from_slice(&(attr_data.len() as u16).to_be_bytes()); // Message length
        response.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]); // Magic cookie
        response.extend_from_slice(&transaction_id); // Transaction ID
                                                     // Attribute
        response.extend_from_slice(&[0x00, 0x20]); // XOR-MAPPED-ADDRESS type
        response.extend_from_slice(&(attr_data.len() as u16).to_be_bytes()); // Attribute length
        response.extend_from_slice(&attr_data);

        let result = parse_xor_mapped_address(&response, &transaction_id);
        assert!(result.is_some());
        let addr = result.unwrap();
        assert_eq!(addr.port(), 12345);
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1))
        );
    }

    #[test]
    fn test_nat_type_display() {
        assert_eq!(NatType::Public.to_string(), "public");
        assert_eq!(NatType::Symmetric.to_string(), "symmetric");
        assert_eq!(NatType::Unknown.to_string(), "unknown");
    }

    #[test]
    fn test_parse_invalid_response() {
        let result = parse_xor_mapped_address(&[0u8; 10], &[0u8; 12]);
        assert!(result.is_none());
    }
}
