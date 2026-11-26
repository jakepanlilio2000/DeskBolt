use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_resolver::Resolver;
use hickory_resolver::proto::rr::RecordType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

pub const DEFAULT_SCAN_PORTS: &[u16] = &[4899, 22, 3389, 80, 443, 8291];
const MAX_CIDR_HOSTS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsQueryKind {
    A,
    Aaaa,
    Ptr,
    Mx,
    Txt,
}

impl DnsQueryKind {
    pub const ALL: [Self; 5] = [Self::A, Self::Aaaa, Self::Ptr, Self::Mx, Self::Txt];

    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Ptr => "PTR",
            Self::Mx => "MX",
            Self::Txt => "TXT",
        }
    }

    fn as_record_type(self) -> RecordType {
        match self {
            Self::A => RecordType::A,
            Self::Aaaa => RecordType::AAAA,
            Self::Ptr => RecordType::PTR,
            Self::Mx => RecordType::MX,
            Self::Txt => RecordType::TXT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanHit {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub service: String,
    pub banner: String,
}

#[derive(Debug, Clone)]
pub struct ArpEntry {
    pub ip: String,
    pub mac: String,
    pub state: String,
    pub interface: String,
}

#[derive(Debug, Clone)]
pub struct TracerouteHop {
    pub hop: u8,
    pub address: String,
    pub rtt_ms: Option<f64>,
    pub reached: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HostLatencyStats {
    pub online: bool,
    pub last_rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub sent: u32,
    pub lost: u32,
    pub used_tcp_fallback: bool,
}

impl HostLatencyStats {
    pub fn record(&mut self, rtt: Option<Duration>, tcp_fallback: bool) {
        self.sent = self.sent.saturating_add(1);
        self.used_tcp_fallback = tcp_fallback;
        match rtt {
            Some(d) => {
                let ms = d.as_secs_f64() * 1000.0;
                if let Some(prev) = self.last_rtt_ms {
                    let delta = (ms - prev).abs();
                    self.jitter_ms = Some(match self.jitter_ms {
                        Some(j) => j * 0.7 + delta * 0.3,
                        None => delta,
                    });
                }
                self.last_rtt_ms = Some(ms);
                self.online = true;
            }
            None => {
                self.lost = self.lost.saturating_add(1);
                self.online = false;
            }
        }
    }

    pub fn loss_pct(&self) -> f32 {
        if self.sent == 0 {
            0.0
        } else {
            (self.lost as f32 / self.sent as f32) * 100.0
        }
    }
}

/// Parses `AA:BB:CC:DD:EE:FF`, `AA-BB-CC-DD-EE-FF`, or `AABBCCDDEEFF` into 6 raw octets.
pub fn parse_mac_address(raw: &str) -> Result<[u8; 6], String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    if cleaned.len() != 12 {
        return Err("MAC address must contain 12 hexadecimal digits.".into());
    }
    let mut out = [0u8; 6];
    for (idx, slot) in out.iter_mut().enumerate() {
        let hex_pair = &cleaned[idx * 2..idx * 2 + 2];
        *slot = u8::from_str_radix(hex_pair, 16)
            .map_err(|_| format!("invalid hex byte '{hex_pair}' in MAC address"))?;
    }
    Ok(out)
}

/// Constructs a 102-byte Wake-on-LAN magic packet (`6x 0xFF` + `16x MAC`).
pub fn build_wol_magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut pkt = [0xFFu8; 102];
    for i in 0..16 {
        let start = 6 + i * 6;
        pkt[start..start + 6].copy_from_slice(&mac);
    }
    pkt
}

/// Broadcasts a Wake-on-LAN UDP magic packet on port `9` (or custom port).
pub fn send_wol_packet(mac_str: &str, broadcast_ip: &str, port: u16) -> Result<(), String> {
    let mac = parse_mac_address(mac_str)?;
    let packet = build_wol_magic_packet(mac);
    let target_ip = broadcast_ip.trim();
    let dst = if target_ip.is_empty() {
        format!("255.255.255.255:{}", if port == 0 { 9 } else { port })
    } else {
        format!("{target_ip}:{}", if port == 0 { 9 } else { port })
    };
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind WoL socket failed: {e}"))?;
    sock.set_broadcast(true)
        .map_err(|e| format!("enable UDP broadcast failed: {e}"))?;
    sock.send_to(&packet, &dst)
        .map_err(|e| format!("send WoL packet to {dst} failed: {e}"))?;
    Ok(())
}

/// Expands an IPv4 CIDR block (e.g. `192.168.1.0/24`) into scannable host IPv4 addresses.
pub fn expand_cidr_v4(cidr: &str) -> Result<Vec<Ipv4Addr>, String> {
    let trimmed = cidr.trim();
    let (ip_str, prefix_str) = trimmed
        .split_once('/')
        .ok_or_else(|| "CIDR must be formatted as a.b.c.d/prefix (e.g. 192.168.1.0/24)".to_string())?;
    let base_ip: Ipv4Addr = ip_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid IPv4 address '{ip_str}'"))?;
    let prefix: u32 = prefix_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid CIDR prefix '/{prefix_str}'"))?;
    if prefix > 32 {
        return Err("CIDR prefix must be between 0 and 32".into());
    }
    let host_bits = 32 - prefix;
    if host_bits > 16 {
        return Err("CIDR block too large; use /20 or smaller (<= 4096 hosts)".into());
    }
    let total_addrs = 1u32 << host_bits;
    let base_u32 = u32::from(base_ip);
    let mask = if prefix == 0 { 0 } else { !0u32 << host_bits };
    let net = base_u32 & mask;

    let (start_off, end_off) = if prefix >= 31 {
        (0, total_addrs)
    } else {
        (1, total_addrs.saturating_sub(1))
    };
    let count = end_off.saturating_sub(start_off) as usize;
    // shortcut: capped at 4096 addresses per subnet scan to prevent socket exhaustion.
    if count > MAX_CIDR_HOSTS {
        return Err(format!(
            "subnet has {count} hosts; maximum per scan is {MAX_CIDR_HOSTS} (/20)"
        ));
    }

    let mut out = Vec::with_capacity(count);
    for off in start_off..end_off {
        out.push(Ipv4Addr::from(net + off));
    }
    Ok(out)
}

/// Runs a bounded-concurrency async TCP port scan and service banner grab across `hosts x ports`.
pub async fn scan_subnet(hosts: Vec<Ipv4Addr>, ports: Vec<u16>, concurrency: usize) -> Vec<ScanHit> {
    let sem = Arc::new(Semaphore::new(concurrency.clamp(1, 128)));
    let mut handles = Vec::new();
    for ip in hosts {
        for &port in &ports {
            if port == 0 {
                continue;
            }
            let permit_sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let Ok(_permit) = permit_sem.acquire_owned().await else {
                    return None;
                };
                probe_port_and_banner(ip, port).await
            }));
        }
    }

    let mut hits = Vec::new();
    for h in handles {
        if let Ok(Some(hit)) = h.await {
            hits.push(hit);
        }
    }
    hits.sort_by_key(|h| (h.ip, h.port));
    hits
}

async fn probe_port_and_banner(ip: Ipv4Addr, port: u16) -> Option<ScanHit> {
    let addr = SocketAddr::new(IpAddr::V4(ip), port);
    let mut stream = tokio::time::timeout(Duration::from_millis(550), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    let _ = stream.set_nodelay(true);

    let mut buf = [0u8; 256];
    let mut banner = String::new();
    if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(220), stream.read(&mut buf)).await
        && n > 0
    {
        banner = sanitize_banner(&buf[..n]);
    } else if matches!(port, 80 | 8080) {
        let req = format!("HEAD / HTTP/1.0\r\nHost: {ip}\r\n\r\n");
        if stream.write_all(req.as_bytes()).await.is_ok()
            && let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf)).await
            && n > 0
        {
            banner = sanitize_banner(&buf[..n]);
        }
    }

    let service = infer_service_name(port, &banner);
    Some(ScanHit {
        ip,
        port,
        service,
        banner,
    })
}

fn sanitize_banner(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let first_line = s.lines().next().unwrap_or("");
    first_line
        .chars()
        .filter(|c| !c.is_control())
        .take(96)
        .collect::<String>()
        .trim()
        .to_string()
}

fn infer_service_name(port: u16, banner: &str) -> String {
    let low = banner.to_ascii_lowercase();
    if low.starts_with("ssh-") {
        return "SSH".into();
    }
    if low.starts_with("http/") {
        return "HTTP".into();
    }
    match port {
        4899 => "DeskBolt / Remote".into(),
        22 => "SSH".into(),
        3389 => "RDP".into(),
        80 | 8080 => "HTTP".into(),
        443 => "HTTPS / TLS".into(),
        8291 => "Winbox".into(),
        _ => "TCP".into(),
    }
}

/// Probes RTT to `host`, preferring ICMP echo (`surge-ping`) and falling back to TCP connect RTT on permission errors.
pub async fn probe_host_latency(
    icmp_client: Option<&surge_ping::Client>,
    host: &str,
    fallback_port: u16,
    seq: u16,
) -> (Option<Duration>, bool) {
    let resolved_ip = match host.trim().parse::<IpAddr>() {
        Ok(ip) => Some(ip),
        Err(_) => tokio::net::lookup_host((host.trim(), fallback_port.max(1)))
            .await
            .ok()
            .and_then(|mut it| it.next().map(|sa| sa.ip())),
    };

    if let (Some(client), Some(ip)) = (icmp_client, resolved_ip) {
        let mut pinger = client
            .pinger(ip, surge_ping::PingIdentifier((std::process::id() as u16).max(1)))
            .await;
        pinger.timeout(Duration::from_millis(1200));
        match pinger.ping(surge_ping::PingSequence(seq), &[0x44; 16]).await {
            Ok((_pkt, rtt)) => return (Some(rtt), false),
            Err(surge_ping::SurgeError::Timeout { .. }) => return (None, false),
            Err(surge_ping::SurgeError::IOError(e))
                if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(_) => {}
        }
    }

    // Automatic unprivileged TCP handshake latency fallback
    let port = if fallback_port == 0 { 4899 } else { fallback_port };
    let started = Instant::now();
    match tokio::time::timeout(
        Duration::from_millis(1200),
        TcpStream::connect((host.trim(), port)),
    )
    .await
    {
        Ok(Ok(_)) => (Some(started.elapsed()), true),
        _ => (None, true),
    }
}

/// Creates a reusable `surge-ping` client if permitted on the current OS/user context.
pub fn try_create_icmp_client() -> Option<surge_ping::Client> {
    surge_ping::Client::new(&surge_ping::Config::default()).ok()
}

/// Performs pure-Rust DNS resolution (`A`, `AAAA`, `PTR`, `MX`, `TXT`) using `hickory-resolver`.
pub async fn dns_lookup(input: &str, kind: DnsQueryKind) -> Result<Vec<String>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a domain name or IP address.".into());
    }
    let resolver = Resolver::builder_tokio()
        .map_err(|e| format!("init DNS resolver failed: {e}"))?
        .build();

    let query_name = if kind == DnsQueryKind::Ptr
        && let Ok(ip) = trimmed.parse::<IpAddr>()
    {
        format_reverse_arpa(ip)
    } else {
        trimmed.to_string()
    };

    let lookup = resolver
        .lookup(query_name, kind.as_record_type())
        .await
        .map_err(|e| format!("DNS query failed: {e}"))?;

    let mut results = Vec::new();
    for rec in lookup.iter() {
        results.push(rec.to_string());
    }
    if results.is_empty() {
        results.push("(no records returned)".into());
    }
    Ok(results)
}

fn format_reverse_arpa(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa.")
        }
        IpAddr::V6(v6) => {
            let mut s = String::with_capacity(74);
            for byte in v6.octets().iter().rev() {
                s.push_str(&format!("{:x}.{:x}.", byte & 0x0f, byte >> 4));
            }
            s.push_str("ip6.arpa.");
            s
        }
    }
}

/// Reads the OS ARP / neighbor cache natively without spawning `arp`.
pub fn read_arp_table() -> Result<Vec<ArpEntry>, String> {
    read_arp_table_platform()
}

#[cfg(target_os = "windows")]
fn read_arp_table_platform() -> Result<Vec<ArpEntry>, String> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIpNetTable2, MIB_IPNET_TABLE2};
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};

    let mut table_ptr: *mut MIB_IPNET_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIpNetTable2 allocates table_ptr freed unconditionally via FreeMibTable below.
    let err = unsafe { GetIpNetTable2(AF_UNSPEC, &mut table_ptr) };
    if err.0 != 0 || table_ptr.is_null() {
        return Err(format!("GetIpNetTable2 failed (Win32 {})", err.0));
    }

    let mut entries = Vec::new();
    // SAFETY: table_ptr is valid with NumEntries contiguous MIB_IPNET_ROW2 elements.
    unsafe {
        let count = (*table_ptr).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table_ptr).Table.as_ptr(), count);
        for row in rows {
            let ip = match row.Address.si_family {
                f if f == AF_INET => {
                    let b = row.Address.Ipv4.sin_addr.S_un.S_un_b;
                    IpAddr::V4(Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4)).to_string()
                }
                f if f == AF_INET6 => {
                    let bytes = row.Address.Ipv6.sin6_addr.u.Byte;
                    IpAddr::V6(std::net::Ipv6Addr::from(bytes)).to_string()
                }
                _ => continue,
            };

            let mac_len = (row.PhysicalAddressLength as usize).min(row.PhysicalAddress.len());
            if mac_len == 0 {
                continue;
            }
            let mac = row.PhysicalAddress[..mac_len]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":");
            if mac == "00:00:00:00:00:00" {
                continue;
            }
            let state = match row.State.0 {
                1 => "Incomplete",
                2 => "Probe",
                3 => "Delay",
                4 => "Stale",
                5 => "Reachable",
                6 => "Permanent",
                _ => "Unreachable",
            }
            .to_string();
            entries.push(ArpEntry {
                ip,
                mac,
                state,
                interface: format!("if#{}", row.InterfaceIndex),
            });
        }
        FreeMibTable(table_ptr.cast());
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
fn read_arp_table_platform() -> Result<Vec<ArpEntry>, String> {
    let text = std::fs::read_to_string("/proc/net/arp")
        .map_err(|e| format!("read /proc/net/arp failed: {e}"))?;
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 6 {
            let state = match cols[2] {
                "0x2" => "Reachable",
                "0x6" => "Permanent",
                "0x0" => "Incomplete",
                other => other,
            }
            .to_string();
            out.push(ArpEntry {
                ip: cols[0].to_string(),
                mac: cols[3].to_ascii_uppercase(),
                state,
                interface: cols[5].to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn read_arp_table_platform() -> Result<Vec<ArpEntry>, String> {
    // Query PF_ROUTE ARP entries directly via libc::sysctl (zero external arp invocation).
    let mut mib = [
        libc::CTL_NET,
        libc::PF_ROUTE,
        0,
        libc::AF_INET,
        libc::NET_RT_FLAGS,
        libc::RTF_LLINFO,
    ];
    let mut len: libc::size_t = 0;
    // SAFETY: Standard two-pass sysctl query for routing table buffer sizing.
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Err("sysctl ARP size query failed".into());
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; len];
        if libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return Err("sysctl ARP read failed".into());
        }
        Ok(parse_macos_arp_buffer(&buf[..len]))
    }
}

#[cfg(target_os = "macos")]
fn parse_macos_arp_buffer(buf: &[u8]) -> Vec<ArpEntry> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let rtm_hdr_len = std::mem::size_of::<libc::rt_msghdr>();
    while offset + rtm_hdr_len <= buf.len() {
        // SAFETY: Alignment & bounds checked before reading rt_msghdr header.
        let rtm = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast::<libc::rt_msghdr>()) };
        let msglen = rtm.rtm_msglen as usize;
        if msglen < rtm_hdr_len || offset + msglen > buf.len() {
            break;
        }
        let mut sa_off = offset + rtm_hdr_len;
        let end = offset + msglen;
        let mut ip_str = None;
        let mut mac_str = None;
        for bit in 0..8 {
            if rtm.rtm_addrs & (1 << bit) == 0 || sa_off + 4 > end {
                continue;
            }
            let sa_len = buf[sa_off] as usize;
            let sa_family = buf[sa_off + 1] as i32;
            let step = if sa_len == 0 { 4 } else { (sa_len + 3) & !3 };
            if sa_family == libc::AF_INET && sa_off + 8 <= end {
                ip_str = Some(
                    Ipv4Addr::new(buf[sa_off + 4], buf[sa_off + 5], buf[sa_off + 6], buf[sa_off + 7])
                        .to_string(),
                );
            } else if sa_family == libc::AF_LINK && sa_off + 8 <= end {
                let nlen = buf[sa_off + 5] as usize;
                let alen = buf[sa_off + 6] as usize;
                let addr_start = sa_off + 8 + nlen;
                if alen == 6 && addr_start + 6 <= end {
                    mac_str = Some(
                        buf[addr_start..addr_start + 6]
                            .iter()
                            .map(|b| format!("{b:02X}"))
                            .collect::<Vec<_>>()
                            .join(":"),
                    );
                }
            }
            sa_off += step;
        }
        if let (Some(ip), Some(mac)) = (ip_str, mac_str) {
            out.push(ArpEntry {
                ip,
                mac,
                state: "ARP".into(),
                interface: format!("if#{}", rtm.rtm_index),
            });
        }
        offset += msglen;
    }
    out
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn read_arp_table_platform() -> Result<Vec<ArpEntry>, String> {
    Ok(Vec::new())
}

/// Performs hop-by-hop TTL traceroute natively (OS `IcmpSendEcho` on Windows, TTL-stepped probes elsewhere).
pub async fn run_traceroute(target: &str, max_hops: u8) -> Result<Vec<TracerouteHop>, String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("Target host cannot be empty.".into());
    }
    let ip: IpAddr = match trimmed.parse() {
        Ok(ip) => ip,
        Err(_) => {
            let resolver = Resolver::builder_tokio()
                .map_err(|e| format!("DNS resolver init failed: {e}"))?
                .build();
            let lookup = resolver
                .lookup_ip(trimmed)
                .await
                .map_err(|e| format!("cannot resolve '{trimmed}': {e}"))?;
            lookup
                .iter()
                .next()
                .ok_or_else(|| format!("no IP addresses found for '{trimmed}'"))?
        }
    };

    let limit = max_hops.clamp(1, 30);
    #[cfg(target_os = "windows")]
    if let IpAddr::V4(v4) = ip {
        return tokio::task::spawn_blocking(move || windows_traceroute_v4(v4, limit))
            .await
            .map_err(|e| format!("traceroute task failed: {e}"))?;
    }

    let mut hops = Vec::new();
    for hop in 1..=limit {
        let cfg = surge_ping::Config::builder()
            .kind(if ip.is_ipv6() {
                surge_ping::ICMP::V6
            } else {
                surge_ping::ICMP::V4
            })
            .ttl(hop as u32)
            .build();
        let Ok(client) = surge_ping::Client::new(&cfg) else {
            hops.push(TracerouteHop {
                hop,
                address: "* (ICMP requires elevated privileges)".into(),
                rtt_ms: None,
                reached: false,
            });
            break;
        };
        let mut pinger = client
            .pinger(ip, surge_ping::PingIdentifier((std::process::id() as u16).max(1)))
            .await;
        pinger.timeout(Duration::from_millis(900));
        match pinger
            .ping(surge_ping::PingSequence(hop as u16), &[0x54; 16])
            .await
        {
            Ok((_pkt, rtt)) => {
                hops.push(TracerouteHop {
                    hop,
                    address: ip.to_string(),
                    rtt_ms: Some(rtt.as_secs_f64() * 1000.0),
                    reached: true,
                });
                break;
            }
            Err(_) => {
                hops.push(TracerouteHop {
                    hop,
                    address: "*".into(),
                    rtt_ms: None,
                    reached: false,
                });
            }
        }
    }
    Ok(hops)
}

#[cfg(target_os = "windows")]
fn windows_traceroute_v4(dest: Ipv4Addr, max_hops: u8) -> Result<Vec<TracerouteHop>, String> {
    use windows::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, IP_OPTION_INFORMATION, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
    };

    // SAFETY: Standard Win32 IcmpCreateFile + IcmpSendEcho loop closed cleanly via IcmpCloseHandle.
    let handle = unsafe { IcmpCreateFile() }.map_err(|e| format!("IcmpCreateFile: {e}"))?;
    let mut hops = Vec::new();
    let payload = [0x44u8; 32];
    let mut reply_buf = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + payload.len() + 64];
    let dest_addr = u32::from_ne_bytes(dest.octets());

    for hop in 1..=max_hops {
        let opts = IP_OPTION_INFORMATION {
            Ttl: hop,
            ..Default::default()
        };
        let n_replies = unsafe {
            IcmpSendEcho(
                handle,
                dest_addr,
                payload.as_ptr().cast(),
                payload.len() as u16,
                Some(&opts),
                reply_buf.as_mut_ptr().cast(),
                reply_buf.len() as u32,
                1000,
            )
        };
        if n_replies > 0 {
            let reply = unsafe { std::ptr::read_unaligned(reply_buf.as_ptr().cast::<ICMP_ECHO_REPLY>()) };
            let hop_ip = Ipv4Addr::from(reply.Address.to_ne_bytes());
            let rtt_ms = Some(reply.RoundTripTime as f64);
            let reached = reply.Status == 0 || hop_ip == dest;
            hops.push(TracerouteHop {
                hop,
                address: hop_ip.to_string(),
                rtt_ms,
                reached,
            });
            if reached {
                break;
            }
        } else {
            hops.push(TracerouteHop {
                hop,
                address: "*".into(),
                rtt_ms: None,
                reached: false,
            });
        }
    }

    let _ = unsafe { IcmpCloseHandle(handle) };
    Ok(hops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wol_magic_packet_layout_and_mac_parsing() {
        let mac = parse_mac_address("aa:bb-cc.dd:ee:ff").unwrap();
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert!(parse_mac_address("00:11:22").is_err());

        let pkt = build_wol_magic_packet(mac);
        assert_eq!(pkt.len(), 102);
        assert_eq!(&pkt[..6], &[0xFF; 6]);
        for i in 0..16 {
            let s = 6 + i * 6;
            assert_eq!(&pkt[s..s + 6], &mac);
        }
    }

    #[test]
    fn cidr_expansion_and_bounds_enforced() {
        let hosts24 = expand_cidr_v4("192.168.1.42/24").unwrap();
        assert_eq!(hosts24.len(), 254);
        assert_eq!(hosts24[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(*hosts24.last().unwrap(), Ipv4Addr::new(192, 168, 1, 254));

        let single = expand_cidr_v4("10.0.0.5/32").unwrap();
        assert_eq!(single, vec![Ipv4Addr::new(10, 0, 0, 5)]);

        assert!(expand_cidr_v4("10.0.0.0/8").is_err());
        assert!(expand_cidr_v4("not-a-cidr").is_err());
    }
}
