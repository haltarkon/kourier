//! The TRAP listeners of the real-library tests (TG-1b B-5, critic H1): in an offline child (an
//! `unshare -rn` namespace whose every address is routed to `lo`, [`crate::spawn::run_self_offline`])
//! a connection or a DNS query that TDLib makes PAST the gate lands here and is counted: TCP 443,
//! 80, 5222 and 53 and UDP 53 on every address, the resolvers of `resolv.conf` included. [`Traps::arm`]
//! proves every kind catches a probe of its own before the counts start. Test support only
//! (`#[doc(hidden)]`): binds listeners on fixed ports, so it is only ever armed inside the offline
//! child (it refuses elsewhere). Counts only: nothing that reaches a trap is read or kept.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The trap listeners: every direct attempt past the gate, counted.
pub struct Traps {
    tcp: Arc<AtomicUsize>,
    dns: Arc<AtomicUsize>,
    v6: bool,
    /// The namespace's counters after the probes (the deltas are measured from them).
    base: NetCounters,
}

/// The network namespace's own counters (`/proc/self/net/snmp`, `snmp6`; they belong to the
/// namespace of the reading process, which is the offline child's): what leaves it on ANY port, not
/// only the four the trap listeners hold (TG-1b fix G3). Every address is routed to `lo`, so a
/// connection or a datagram to a port nobody listens on is refused or answered by the namespace
/// itself and no trap sees it, but these counters rise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetCounters {
    /// `Tcp ActiveOpens`: TCP connections started (IPv4 and IPv6 together).
    pub tcp_active_opens: u64,
    /// `Tcp PassiveOpens`: TCP connections a listener of the namespace took. Every address is
    /// local, so a connection to a port nobody listens on rises `tcp_active_opens` alone.
    pub tcp_passive_opens: u64,
    /// `Udp OutDatagrams`.
    pub udp_out: u64,
    /// `Icmp OutMsgs`.
    pub icmp_out: u64,
    /// `Udp6OutDatagrams`.
    pub udp6_out: u64,
    /// `Icmp6OutMsgs`.
    pub icmp6_out: u64,
}

impl NetCounters {
    /// The counters of this process's network namespace now.
    /// # Panics
    /// When `/proc/self/net/snmp` or `snmp6` cannot be read or lacks a field (a kernel without
    /// IPv6 has no `snmp6`: its IPv6 counters are then zero).
    pub fn read() -> Self {
        let snmp = std::fs::read_to_string("/proc/self/net/snmp").expect("read snmp");
        let snmp6 = std::fs::read_to_string("/proc/self/net/snmp6").unwrap_or_default();
        Self::parse(&snmp, &snmp6)
    }

    fn parse(snmp: &str, snmp6: &str) -> Self {
        let v4 = |group: &str, name: &str| {
            snmp_field(snmp, group, name)
                .unwrap_or_else(|| panic!("no {group} {name} in /proc/self/net/snmp"))
        };
        let v6 = |name: &str| {
            snmp6
                .lines()
                .find_map(|l| {
                    let mut words = l.split_whitespace();
                    (words.next()? == name).then(|| words.next()?.parse().ok())?
                })
                .unwrap_or(0)
        };
        Self {
            tcp_active_opens: v4("Tcp:", "ActiveOpens"),
            tcp_passive_opens: v4("Tcp:", "PassiveOpens"),
            udp_out: v4("Udp:", "OutDatagrams"),
            icmp_out: v4("Icmp:", "OutMsgs"),
            udp6_out: v6("Udp6OutDatagrams"),
            icmp6_out: v6("Icmp6OutMsgs"),
        }
    }

    /// The rise since `earlier` (saturating).
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            tcp_active_opens: self
                .tcp_active_opens
                .saturating_sub(earlier.tcp_active_opens),
            tcp_passive_opens: self
                .tcp_passive_opens
                .saturating_sub(earlier.tcp_passive_opens),
            udp_out: self.udp_out.saturating_sub(earlier.udp_out),
            icmp_out: self.icmp_out.saturating_sub(earlier.icmp_out),
            udp6_out: self.udp6_out.saturating_sub(earlier.udp6_out),
            icmp6_out: self.icmp6_out.saturating_sub(earlier.icmp6_out),
        }
    }

    /// UDP and ICMP datagrams or messages sent, IPv4 and IPv6 together.
    pub fn datagrams(self) -> u64 {
        self.udp_out + self.icmp_out + self.udp6_out + self.icmp6_out
    }
}

/// A value of `/proc/net/snmp`'s two-line groups: `Tcp: <names>` then `Tcp: <values>`.
fn snmp_field(snmp: &str, group: &str, name: &str) -> Option<u64> {
    let mut lines = snmp.lines();
    while let Some(names) = lines.next() {
        let values = lines.next()?;
        if !names.starts_with(group) {
            continue;
        }
        let at = names.split_whitespace().position(|n| n == name)?;
        return values.split_whitespace().nth(at)?.parse().ok();
    }
    None
}

/// The ports TDLib reaches Telegram on, and DNS.
const TRAP_PORTS: [u16; 3] = [443, 80, 5222];

/// A listener on every address of `port`: dual-stack `[::]` where IPv6 exists, else `0.0.0.0`.
fn any_tcp(port: u16) -> (TcpListener, bool) {
    match TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)) {
        Ok(l) => (l, true),
        Err(_) => (
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).expect("bind a trap"),
            false,
        ),
    }
}

fn any_udp(port: u16) -> UdpSocket {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
        .expect("bind the DNS trap")
}

fn count_accepts(listener: TcpListener, hits: Arc<AtomicUsize>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                hits.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
}

/// The resolvers of this namespace's `resolv.conf` (the child shares the host's file).
pub fn resolvers() -> Vec<IpAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|a| a.trim().parse().ok())
        .collect()
}

impl Traps {
    /// Binds every trap, proves each kind catches a probe (an IPv4 and an IPv6 documentation
    /// address on 443, a datagram to every resolver and to a documentation address on 53), and
    /// starts the counts from zero.
    /// # Panics
    /// Outside the offline child, or when a trap cannot be bound or misses its probe.
    pub fn arm() -> Self {
        assert!(
            crate::spawn::in_offline_child(),
            "the traps are armed only in the offline child"
        );
        let (tcp, dns) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut v6 = true;
        for port in TRAP_PORTS {
            let (l, dual) = any_tcp(port);
            v6 &= dual;
            count_accepts(l, tcp.clone());
        }
        let (l, dual) = any_tcp(53);
        v6 &= dual;
        count_accepts(l, dns.clone());
        let udp = any_udp(53);
        let udp_hits = dns.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while udp.recv_from(&mut buf).is_ok() {
                udp_hits.fetch_add(1, Ordering::SeqCst);
            }
        });
        let before = NetCounters::read();
        let mut tcp_probes = vec![SocketAddr::from(([192, 0, 2, 7], 443))];
        if v6 {
            tcp_probes.push(SocketAddr::from((
                Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7),
                443,
            )));
        }
        for to in &tcp_probes {
            let _ = TcpStream::connect_timeout(to, Duration::from_secs(2)).expect("a trap probe");
        }
        let mut dns_probes: Vec<IpAddr> = resolvers();
        dns_probes.push(IpAddr::from([192, 0, 2, 53]));
        let sender = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("probe socket");
        let sender6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok();
        for to in &dns_probes {
            let to = SocketAddr::new(*to, 53);
            match (to.is_ipv6(), &sender6) {
                (true, Some(s)) => s.send_to(b"probe", to).expect("probe"),
                (true, None) => 0,
                (false, _) => sender.send_to(b"probe", to).expect("probe"),
            };
        }
        let want_dns = dns_probes
            .iter()
            .filter(|a| a.is_ipv4() || sender6.is_some())
            .count();
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while (tcp.load(Ordering::SeqCst) < tcp_probes.len()
            || dns.load(Ordering::SeqCst) < want_dns)
            && std::time::Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            tcp.load(Ordering::SeqCst),
            tcp_probes.len(),
            "a TCP trap missed its probe"
        );
        assert_eq!(
            dns.load(Ordering::SeqCst),
            want_dns,
            "the DNS trap missed its probe"
        );
        tcp.store(0, Ordering::SeqCst);
        dns.store(0, Ordering::SeqCst);
        // After the probes: they are in the namespace's counters too, which proves the counters
        // are live (a parse that reads zeros for ever would make "no rise" meaningless).
        let base = NetCounters::read();
        let rose = base.since(before);
        assert!(
            rose.tcp_active_opens >= tcp_probes.len() as u64,
            "the namespace's TCP counter missed the probes"
        );
        assert!(
            rose.udp_out + rose.udp6_out >= want_dns as u64,
            "the namespace's UDP counters missed the probes"
        );
        Self { tcp, dns, v6, base }
    }

    /// The direct attempts so far: (TCP to 443/80/5222, DNS over UDP or TCP 53).
    pub fn hits(&self) -> (usize, usize) {
        (
            self.tcp.load(Ordering::SeqCst),
            self.dns.load(Ordering::SeqCst),
        )
    }
}

impl Traps {
    /// What the namespace's own counters rose by since the traps were armed (the probes excluded):
    /// every TCP connection started, every UDP datagram and ICMP message sent, on any port.
    pub fn opens(&self) -> NetCounters {
        NetCounters::read().since(self.base)
    }

    /// Whether the traps listen on IPv6 too (dual-stack `[::]`).
    pub fn v6(&self) -> bool {
        self.v6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNMP: &str = "Ip: Forwarding DefaultTTL\nIp: 1 64\n\
Icmp: InMsgs InErrors OutMsgs OutErrors\nIcmp: 1 2 3 4\n\
Tcp: RtoAlgorithm ActiveOpens PassiveOpens\nTcp: 1 7 9\n\
Udp: InDatagrams NoPorts InErrors OutDatagrams\nUdp: 10 11 12 13\n";
    const SNMP6: &str = "Ip6InReceives 1\nIcmp6OutMsgs 5\nUdp6OutDatagrams 6\n";

    /// The two-line format is read by name, not by column.
    #[test]
    fn the_namespaces_counters_are_read_by_name() {
        let c = NetCounters::parse(SNMP, SNMP6);
        assert_eq!(
            c,
            NetCounters {
                tcp_active_opens: 7,
                tcp_passive_opens: 9,
                udp_out: 13,
                icmp_out: 3,
                udp6_out: 6,
                icmp6_out: 5,
            }
        );
        assert_eq!(c.datagrams(), 13 + 3 + 6 + 5);
        // No snmp6 (a kernel without IPv6): zero.
        let no6 = NetCounters::parse(SNMP, "");
        assert_eq!((no6.udp6_out, no6.icmp6_out), (0, 0));
        let later = NetCounters {
            tcp_active_opens: 9,
            ..c
        };
        assert_eq!(
            later.since(c),
            NetCounters {
                tcp_active_opens: 2,
                ..NetCounters::default()
            }
        );
        assert_eq!(c.since(later).tcp_active_opens, 0, "the rise saturates");
    }

    /// The counters of a real namespace read (this machine's): the fields exist.
    #[test]
    fn this_machines_counters_read() {
        let c = NetCounters::read();
        assert_eq!(c.since(c), NetCounters::default());
    }
}
