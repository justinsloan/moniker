//! Whether a host answers a ping from this machine, for the host status in the
//! profile lists (the GUI's `hosts`).
//!
//! An ICMP echo request over an unprivileged ICMP datagram socket
//! (`SOCK_DGRAM`, `IPPROTO_ICMP` or `IPPROTO_ICMPV6`), which Linux allows to
//! every user whose group is in `net.ipv4.ping_group_range`, as current Ubuntu
//! and Fedora set it for everyone. The kernel fills in the identifier and the
//! checksum, and hands the socket only the replies to its own requests. No
//! program is run and no privilege is needed; where the kernel refuses the
//! socket, the answer is [`Ping::NotAllowed`] and nothing is sent.
//!
//! [`ping`] sends up to [`ATTEMPTS`] requests, [`WAIT`] apart, and stops at
//! the first reply: one is enough. The requests carry nothing from Moniker:
//! the payload is zeros. Blocking: run it off the main thread.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketType, connect, ipproto, recv, send, socket,
    sockopt::{Timeout, set_socket_timeout},
};

/// How many echo requests a check sends at most.
pub const ATTEMPTS: u16 = 3;

/// How long each request waits for its reply.
pub const WAIT: Duration = Duration::from_secs(1);

/// The bytes of zeros after the echo header.
const PAYLOAD_LEN: usize = 16;

/// ICMP message types: echo request and echo reply, for IPv4 and IPv6.
const ECHO_REQUEST_V4: u8 = 8;
const ECHO_REPLY_V4: u8 = 0;
const ECHO_REQUEST_V6: u8 = 128;
const ECHO_REPLY_V6: u8 = 129;

/// What a ping found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ping {
    /// A reply came back.
    Answered,
    /// No reply to any request (or the name could not be resolved, or the
    /// network refused to send).
    NoReply,
    /// This system does not let this user open an ICMP datagram socket:
    /// nothing was sent.
    NotAllowed,
}

/// Ping `host` (a name or an address) from this machine (module docs).
/// Resolving the name is the system's own (`getaddrinfo`); its first address
/// is pinged.
pub fn ping(host: &str) -> Ping {
    let Some(address) = (host, 0)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
    else {
        return Ping::NoReply;
    };
    let v6 = address.is_ipv6();
    let socket = match open_socket(&address) {
        Ok(socket) => socket,
        Err(err) if refused(err) => return Ping::NotAllowed,
        Err(_) => return Ping::NoReply,
    };
    let mut echo = SocketEcho { socket };
    if exchange(&mut echo, v6, ATTEMPTS, WAIT) {
        Ping::Answered
    } else {
        Ping::NoReply
    }
}

/// An ICMP datagram socket connected to `address`.
fn open_socket(address: &SocketAddr) -> Result<OwnedFd, Errno> {
    let (family, protocol) = if address.is_ipv6() {
        (AddressFamily::INET6, ipproto::ICMPV6)
    } else {
        (AddressFamily::INET, ipproto::ICMP)
    };
    let socket = socket(family, SocketType::DGRAM, Some(protocol))?;
    connect(&socket, address)?;
    Ok(socket)
}

/// Whether `err` says the kernel does not let this user have an ICMP datagram
/// socket (outside `ping_group_range`, or no such socket at all).
fn refused(err: Errno) -> bool {
    [
        Errno::ACCESS,
        Errno::PERM,
        Errno::PROTONOSUPPORT,
        Errno::AFNOSUPPORT,
        Errno::SOCKTNOSUPPORT,
    ]
    .contains(&err)
}

/// Where echo requests go and replies come from: the socket, or a test's fake.
trait Echo {
    fn send(&mut self, packet: &[u8]) -> io::Result<()>;
    /// One packet received before `deadline` into `buf`, with its length, or
    /// `None` once `deadline` has passed.
    fn receive(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<Option<usize>>;
}

struct SocketEcho {
    socket: OwnedFd,
}

impl Echo for SocketEcho {
    fn send(&mut self, packet: &[u8]) -> io::Result<()> {
        send(&self.socket, packet, SendFlags::empty())?;
        Ok(())
    }

    fn receive(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<Option<usize>> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        set_socket_timeout(&self.socket, Timeout::Recv, Some(left))?;
        match recv(&self.socket, buf, RecvFlags::empty()) {
            Ok((len, _)) => Ok(Some(len)),
            Err(Errno::AGAIN) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

/// Send up to `attempts` echo requests over `echo`, each waiting `wait` for a
/// reply, and say whether one came: a reply to any request sent so far counts,
/// so a late reply to the first is not missed while waiting for the second. A
/// failure to send or receive ends that request's wait, not the check.
fn exchange(echo: &mut dyn Echo, v6: bool, attempts: u16, wait: Duration) -> bool {
    let mut buf = [0u8; 512];
    for seq in 1..=attempts {
        if echo.send(&echo_request(v6, seq)).is_err() {
            continue;
        }
        let deadline = Instant::now() + wait;
        while let Ok(Some(len)) = echo.receive(&mut buf, deadline) {
            if is_echo_reply(v6, seq, &buf[..len.min(buf.len())]) {
                return true;
            }
        }
    }
    false
}

/// An echo request with sequence number `seq`: type, code 0, a checksum and an
/// identifier of zero (the kernel fills both in), `seq`, and a payload of
/// zeros.
fn echo_request(v6: bool, seq: u16) -> [u8; 8 + PAYLOAD_LEN] {
    let mut packet = [0u8; 8 + PAYLOAD_LEN];
    packet[0] = if v6 { ECHO_REQUEST_V6 } else { ECHO_REQUEST_V4 };
    packet[6..8].copy_from_slice(&seq.to_be_bytes());
    packet
}

/// Whether `packet` is an echo reply to one of the requests sent so far
/// (sequence numbers 1 to `last_seq`).
fn is_echo_reply(v6: bool, last_seq: u16, packet: &[u8]) -> bool {
    let reply = if v6 { ECHO_REPLY_V6 } else { ECHO_REPLY_V4 };
    if packet.len() < 8 || packet[0] != reply || packet[1] != 0 {
        return false;
    }
    let seq = u16::from_be_bytes([packet[6], packet[7]]);
    (1..=last_seq).contains(&seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// What the fake network does for one `receive` call.
    enum Then {
        Packet(Vec<u8>),
        Nothing,
        Fail,
    }

    /// A network that answers from a script and counts what is sent.
    struct Scripted {
        sent: Vec<Vec<u8>>,
        send_fails: bool,
        script: VecDeque<Then>,
    }

    impl Scripted {
        fn new(script: Vec<Then>) -> Scripted {
            Scripted {
                sent: Vec::new(),
                send_fails: false,
                script: script.into(),
            }
        }
    }

    impl Echo for Scripted {
        fn send(&mut self, packet: &[u8]) -> io::Result<()> {
            self.sent.push(packet.to_vec());
            if self.send_fails {
                Err(io::Error::from(io::ErrorKind::NetworkUnreachable))
            } else {
                Ok(())
            }
        }

        fn receive(&mut self, buf: &mut [u8], _deadline: Instant) -> io::Result<Option<usize>> {
            match self.script.pop_front().unwrap_or(Then::Nothing) {
                Then::Packet(packet) => {
                    buf[..packet.len()].copy_from_slice(&packet);
                    Ok(Some(packet.len()))
                }
                Then::Nothing => Ok(None),
                Then::Fail => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            }
        }
    }

    /// An echo reply as a host sends it back, for IPv4 or IPv6.
    fn reply(v6: bool, seq: u16) -> Vec<u8> {
        let mut packet = vec![if v6 { 129 } else { 0 }, 0, 0xab, 0xcd, 0x12, 0x34];
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&[0; PAYLOAD_LEN]);
        packet
    }

    #[test]
    fn a_request_is_a_bare_echo_request_with_its_sequence_number() {
        let v4 = echo_request(false, 2);
        assert_eq!(&v4[..8], &[8, 0, 0, 0, 0, 0, 0, 2]);
        assert!(v4[8..].iter().all(|&b| b == 0), "nothing from Moniker");
        let v6 = echo_request(true, 258);
        assert_eq!(&v6[..8], &[128, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(v6.len(), 24);
    }

    #[test]
    fn only_an_echo_reply_to_a_request_sent_counts() {
        assert!(is_echo_reply(false, 1, &reply(false, 1)));
        assert!(is_echo_reply(true, 3, &reply(true, 2)), "a late reply");
        assert!(!is_echo_reply(false, 1, &reply(false, 2)), "not sent yet");
        assert!(!is_echo_reply(false, 3, &reply(false, 0)));
        assert!(!is_echo_reply(false, 1, &reply(true, 1)), "the IPv6 type");
        assert!(!is_echo_reply(true, 1, &reply(false, 1)), "the IPv4 type");
        let mut other_code = reply(false, 1);
        other_code[1] = 1;
        assert!(!is_echo_reply(false, 1, &other_code));
        assert!(
            !is_echo_reply(false, 1, &[0, 0, 0, 0, 0, 0, 0]),
            "too short"
        );
        // An echo request (our own, looped back) is not a reply.
        assert!(!is_echo_reply(false, 1, &echo_request(false, 1)));
    }

    #[test]
    fn the_first_reply_ends_the_check() {
        let mut net = Scripted::new(vec![Then::Packet(reply(false, 1))]);
        assert!(exchange(&mut net, false, 3, WAIT));
        assert_eq!(net.sent.len(), 1, "one request was enough");
    }

    #[test]
    fn three_requests_without_a_reply_are_no_reply() {
        let mut net = Scripted::new(vec![]);
        assert!(!exchange(&mut net, true, 3, WAIT));
        assert_eq!(net.sent.len(), 3);
        let seqs: Vec<[u8; 2]> = net.sent.iter().map(|p| [p[6], p[7]]).collect();
        assert_eq!(seqs, [[0, 1], [0, 2], [0, 3]]);
    }

    #[test]
    fn a_reply_to_a_later_request_counts() {
        let mut net = Scripted::new(vec![
            Then::Nothing,
            // Something else, then the answer, during the second wait.
            Then::Packet(vec![3, 1, 0, 0, 0, 0, 0, 0]),
            Then::Packet(reply(false, 1)),
        ]);
        assert!(exchange(&mut net, false, 3, WAIT));
        assert_eq!(net.sent.len(), 2);
    }

    #[test]
    fn packets_that_are_not_replies_are_no_answer() {
        let mut net = Scripted::new(vec![
            // Destination unreachable, our own request looped back, a reply
            // to a request not sent: none of them is an answer.
            Then::Packet(vec![3, 1, 0, 0, 0, 0, 0, 0]),
            Then::Packet(echo_request(false, 1).to_vec()),
            Then::Packet(reply(false, 9)),
        ]);
        assert!(!exchange(&mut net, false, 3, WAIT));
        assert_eq!(net.sent.len(), 3);
    }

    #[test]
    fn errors_end_one_wait_not_the_check() {
        let mut net = Scripted::new(vec![Then::Fail, Then::Fail, Then::Packet(reply(true, 3))]);
        assert!(exchange(&mut net, true, 3, WAIT));
        assert_eq!(net.sent.len(), 3);
        let mut down = Scripted::new(vec![]);
        down.send_fails = true;
        assert!(!exchange(&mut down, false, 3, WAIT));
        assert_eq!(down.sent.len(), 3, "every request was tried");
    }

    #[test]
    fn this_machine_answers_its_own_ping() {
        // Loopback only: nothing leaves the machine.
        match ping("127.0.0.1") {
            Ping::Answered => {}
            Ping::NotAllowed => {
                eprintln!("skipped: this system does not allow ICMP datagram sockets here");
            }
            Ping::NoReply => panic!("127.0.0.1 did not answer"),
        }
    }

    #[test]
    fn a_name_that_does_not_resolve_is_no_reply() {
        // Refused by the resolver at once: nothing is sent, not even a query.
        assert_eq!(ping(""), Ping::NoReply);
    }
}
