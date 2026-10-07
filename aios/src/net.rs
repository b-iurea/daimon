//! IPv4 setup. Static from the kernel cmdline:
//!   aios.ip=10.0.2.15/24 aios.gw=10.0.2.2 aios.dns=10.0.2.3 [aios.if=eth0]
//! otherwise a DHCP client per interface, in its own thread so boot never waits on the network.

use crate::log;
use std::collections::HashMap;
use std::fs;
use std::mem;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

pub fn up(args: &HashMap<String, String>) {
    let s = sock();
    set_up(s, "lo");
    unsafe { libc::close(s) };

    let Some((ip, prefix)) = args.get("aios.ip").and_then(|v| v.split_once('/')) else {
        for iface in ifaces() {
            std::thread::spawn(move || dhcp_loop(&iface));
        }
        return;
    };
    let iface = args.get("aios.if").map_or("eth0", String::as_str);
    match (ip.parse(), prefix.parse::<u32>()) {
        (Ok(ip), Ok(p)) if p <= 32 => apply(&Lease {
            iface: iface.into(),
            ip,
            mask: Ipv4Addr::from(u32::MAX.checked_shl(32 - p).unwrap_or(0)),
            gw: args.get("aios.gw").and_then(|g| g.parse().ok()),
            dns: args.get("aios.dns").and_then(|g| g.parse().ok()),
            secs: 0,
        }),
        _ => log("net: bad aios.ip"),
    }
}

/// Machine name on the network: letters, digits and '-', 1..63 chars (RFC 1123). Applied to the kernel at once.
pub fn valid_hostname(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn set_hostname(name: &str) -> Result<String, String> {
    if !valid_hostname(name) {
        return Err("hostname: 1-63 letters, digits or '-', not starting with '-'".into());
    }
    if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } != 0 {
        return Err(format!("sethostname: {}", std::io::Error::last_os_error()));
    }
    Ok(format!("machine name {name} (sent to DHCP from the next lease)"))
}

fn ifaces() -> Vec<String> {
    let Ok(rd) = fs::read_dir("/sys/class/net") else {
        return vec![];
    };
    // ethernet only (type 1): skips lo, sit0 and other tunnels
    rd.flatten()
        .filter(|e| fs::read_to_string(e.path().join("type")).is_ok_and(|t| t.trim() == "1"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[derive(Debug, PartialEq)]
struct Lease {
    iface: String,
    ip: Ipv4Addr,
    mask: Ipv4Addr,
    gw: Option<Ipv4Addr>,
    dns: Option<Ipv4Addr>,
    secs: u32,
}

fn apply(l: &Lease) {
    let s = sock();
    set_addr(s, &l.iface, libc::SIOCSIFADDR as libc::Ioctl, l.ip);
    set_addr(s, &l.iface, libc::SIOCSIFNETMASK as libc::Ioctl, l.mask);
    set_up(s, &l.iface);
    if let Some(gw) = l.gw {
        default_route(s, gw);
    }
    unsafe { libc::close(s) };
    if let Some(dns) = l.dns {
        let _ = fs::write("/etc/resolv.conf", format!("nameserver {dns}\n"));
    }
    let prefix = u32::from(l.mask).leading_ones();
    let _ = fs::create_dir_all("/run/aios");
    let _ = fs::write(format!("/run/aios/ip.{}", l.iface), format!("{}/{prefix}\n", l.ip));
    log(&format!("net: {} {}/{prefix}", l.iface, l.ip));
}

// ---------------------------------------------------------------- DHCP (RFC 2131, minimal)

fn dhcp_loop(iface: &str) {
    let s = sock();
    set_up(s, iface); // link must be up to send DISCOVER
    unsafe { libc::close(s) };
    let mac = fs::read_to_string(format!("/sys/class/net/{iface}/address")).unwrap_or_default();
    let mac: Vec<u8> = mac.trim().split(':').filter_map(|h| u8::from_str_radix(h, 16).ok()).collect();
    if mac.len() != 6 {
        return log(&format!("dhcp {iface}: no mac"));
    }
    let mut wait = 2;
    loop {
        match dhcp_once(iface, &mac) {
            Ok(lease) => {
                apply(&lease);
                // ponytail: full DORA again at T1 instead of a unicast RENEW; fine for home LANs
                std::thread::sleep(Duration::from_secs(u64::from(lease.secs.max(60)) / 2));
                wait = 2;
            }
            Err(e) => {
                log(&format!("dhcp {iface}: {e}, retry in {wait}s"));
                std::thread::sleep(Duration::from_secs(wait));
                wait = (wait * 2).min(60);
            }
        }
    }
}

fn dhcp_once(iface: &str, mac: &[u8]) -> Result<Lease, String> {
    let sock = UdpSocket::bind("0.0.0.0:68").map_err(|e| e.to_string())?;
    sock.set_broadcast(true).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let r = unsafe { libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_BINDTODEVICE, iface.as_ptr().cast(), iface.len() as u32) };
    if r != 0 {
        return Err("bind to device failed".into());
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let xid = now.subsec_nanos() ^ now.as_secs() as u32;
    let xid = xid.wrapping_add(u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]));
    let send = |msg: &[u8]| sock.send_to(msg, "255.255.255.255:67").map_err(|e| e.to_string());
    let recv = |want: u8| -> Result<(Ipv4Addr, HashMap<u8, Vec<u8>>), String> {
        let mut buf = [0u8; 1500];
        for _ in 0..5 {
            let n = sock.recv(&mut buf).map_err(|e| e.to_string())?;
            if let Some((ip, opts)) = parse(&buf[..n], xid) {
                match opts.get(&53).and_then(|t| t.first()) {
                    Some(&t) if t == want => return Ok((ip, opts)),
                    Some(6) => return Err("NAK".into()),
                    _ => {}
                }
            }
        }
        Err("no reply".into())
    };

    send(&packet(xid, mac, 1, &[]))?;
    let (offered, opts) = recv(2)?;
    let server = opts.get(&54).cloned().unwrap_or_default();
    send(&packet(xid, mac, 3, &[(50, offered.octets().to_vec()), (54, server)]))?;
    let (ip, opts) = recv(5)?;
    let addr = |code| opts.get(&code).filter(|v| v.len() >= 4).map(|v| Ipv4Addr::new(v[0], v[1], v[2], v[3]));
    Ok(Lease {
        iface: iface.into(),
        ip,
        mask: addr(1).unwrap_or(Ipv4Addr::new(255, 255, 255, 0)),
        gw: addr(3),
        dns: addr(6),
        secs: opts.get(&51).filter(|v| v.len() == 4).map_or(3600, |v| u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
    })
}

fn packet(xid: u32, mac: &[u8], kind: u8, extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut p = vec![0u8; 240];
    p[..4].copy_from_slice(&[1, 1, 6, 0]); // BOOTREQUEST, ethernet, hlen 6
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10] = 0x80; // broadcast flag: we have no IP yet, so the server must broadcast back
    p[28..34].copy_from_slice(mac);
    p[236..240].copy_from_slice(&[99, 130, 83, 99]);
    p.extend([53, 1, kind]);
    p.extend([55, 4, 1, 3, 6, 51]); // ask for mask, router, dns, lease time
    let host = crate::config::get("hostname");
    p.extend([12, host.len() as u8]);
    p.extend(host.as_bytes());
    for (code, v) in extra {
        p.push(*code);
        p.push(v.len() as u8);
        p.extend(v);
    }
    p.push(255);
    p
}

fn parse(b: &[u8], xid: u32) -> Option<(Ipv4Addr, HashMap<u8, Vec<u8>>)> {
    if b.len() < 240 || b[0] != 2 || b[4..8] != xid.to_be_bytes() || b[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let mut opts = HashMap::new();
    let mut i = 240;
    while i < b.len() {
        match b[i] {
            0 => i += 1,
            255 => break,
            code => {
                let len = *b.get(i + 1)? as usize;
                opts.insert(code, b.get(i + 2..i + 2 + len)?.to_vec());
                i += 2 + len;
            }
        }
    }
    Some((Ipv4Addr::new(b[16], b[17], b[18], b[19]), opts))
}

// ---------------------------------------------------------------- ioctl plumbing

fn sock() -> i32 {
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) }
}

fn ifreq(name: &str) -> libc::ifreq {
    let mut r: libc::ifreq = unsafe { mem::zeroed() };
    for (dst, b) in r.ifr_name.iter_mut().zip(name.bytes().take(libc::IFNAMSIZ - 1)) {
        *dst = b as libc::c_char;
    }
    r
}

fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(ip).to_be() },
        sin_zero: [0; 8],
    };
    unsafe { mem::transmute(sin) }
}

fn ioctl_ok(r: libc::c_int, what: &str) {
    let e = std::io::Error::last_os_error();
    if r < 0 && e.raw_os_error() != Some(libc::EEXIST) {
        log(&format!("net: {what}: {e}"));
    }
}

fn set_up(s: i32, iface: &str) {
    let mut r = ifreq(iface);
    unsafe {
        ioctl_ok(libc::ioctl(s, libc::SIOCGIFFLAGS as libc::Ioctl, &mut r), iface);
        r.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        ioctl_ok(libc::ioctl(s, libc::SIOCSIFFLAGS as libc::Ioctl, &r), iface);
    }
}

fn set_addr(s: i32, iface: &str, req: libc::Ioctl, ip: Ipv4Addr) {
    let mut r = ifreq(iface);
    r.ifr_ifru.ifru_addr = sockaddr(ip);
    ioctl_ok(unsafe { libc::ioctl(s, req, &r) }, iface);
}

fn default_route(s: i32, gw: Ipv4Addr) {
    let mut rt: libc::rtentry = unsafe { mem::zeroed() };
    rt.rt_dst = sockaddr(Ipv4Addr::UNSPECIFIED);
    rt.rt_genmask = sockaddr(Ipv4Addr::UNSPECIFIED);
    rt.rt_gateway = sockaddr(gw);
    rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    ioctl_ok(unsafe { libc::ioctl(s, libc::SIOCADDRT as libc::Ioctl, &rt) }, "default route");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ack() {
        let mut p = packet(42, &[2, 0, 0, 0, 0, 1], 5, &[(1, vec![255, 255, 255, 0]), (3, vec![10, 0, 2, 2])]);
        p[0] = 2;
        p[16..20].copy_from_slice(&[10, 0, 2, 15]);
        let (ip, o) = parse(&p, 42).unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 0, 2, 15));
        assert_eq!(o[&53], vec![5]);
        assert_eq!(o[&3], vec![10, 0, 2, 2]);
        assert!(parse(&p, 43).is_none());
    }
}
