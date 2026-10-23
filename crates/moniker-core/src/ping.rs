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
//! [`ping`] sends up to [`ATTEMPTS`] requests to an address, [`WAIT`] apart,
//! and stops at the first reply: one is enough. It asks its caller before each
//! request whether to go on, so a check called off (the person turned the
//! pings off) sends nothing more. The requests carry nothing from Moniker: the
//! payload is zeros.
//!
//! Which addresses: only those ssh's `AddressFamily` allows ([`Family`]), and
//! of those the first the system gives for the name; if it gets no reply, the
//! first of the other kind (IPv4 after IPv6, or the other way round) when both
//! are allowed. `ssh` tries a name's addresses in turn too, so a host whose
//! IPv6 address does not answer but whose IPv4 one does counts as answering.
//! A name with no such address is [`Ping::NameNotFound`]. Blocking: run it off
//! the main thread.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, connect, ipproto, recv, send,
    socket_with,
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
    /// No reply to any request (or the address cannot be pinged, or the check
    /// was called off).
    NoReply,
    /// The name could not be resolved, or has no address of the kind asked
    /// for: nothing was sent.
    NameNotFound,
    /// This system does not let this user open an ICMP datagram socket:
    /// nothing was sent.
    NotAllowed,
}

/// Which of a name's addresses may be pinged: ssh's `AddressFamily`, as
/// `ssh -G` reports it, so that a ping goes where `ssh` would connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// IPv4 or IPv6 (`any`, ssh's default).
    Any,
    /// IPv4 only (`inet`).
    Inet,
    /// IPv6 only (`inet6`).
    Inet6,
}

/// Ping `host` (a name or an address) from this machine at the addresses
/// `family` allows (module docs), asking `keep_going` before each request.
/// Resolving the name is the system's own (`getaddrinfo`).
pub fn ping(host: &str, family: Family, keep_going: &dyn Fn() -> bool) -> Ping {
    if !keep_going() {
        return Ping::NoReply;
    }
    let addresses = match (host, 0).to_socket_addrs() {
        Ok(resolved) => to_try(resolved, family),
        Err(_) => Vec::new(),
    };
    in_turn(&addresses, keep_going, &mut |address| {
        ping_address(address, keep_going)
    })
}

/// The addresses to ping, from those the system gave for a name (`resolved`,
/// in its order of preference): the first one `family` allows, then the first
/// of the other kind if `family` allows both.
fn to_try(resolved: impl IntoIterator<Item = SocketAddr>, family: Family) -> Vec<SocketAddr> {
    let mut chosen: Vec<SocketAddr> = Vec::new();
    for address in resolved {
        let allowed = match family {
            Family::Any => true,
            Family::Inet => address.is_ipv4(),
            Family::Inet6 => address.is_ipv6(),
        };
        // At most one of each kind.
        if allowed && chosen.iter().all(|c| c.is_ipv6() != address.is_ipv6()) {
            chosen.push(address);
        }
    }
    chosen
}

/// Ping `addresses` in turn with `ping_address` until one answers, asking
/// `keep_going` before each: [`Ping::NameNotFound`] if there is none,
/// [`Ping::NotAllowed`] only if every one was refused, else [`Ping::NoReply`].
fn in_turn(
    addresses: &[SocketAddr],
    keep_going: &dyn Fn() -> bool,
    ping_address: &mut dyn FnMut(&SocketAddr) -> Ping,
) -> Ping {
    if addresses.is_empty() {
        return Ping::NameNotFound;
    }
    let mut refused = 0;
    for address in addresses {
        if !keep_going() {
            return Ping::NoReply;
        }
        match ping_address(address) {
            Ping::Answered => return Ping::Answered,
            Ping::NotAllowed => refused += 1,
            Ping::NoReply | Ping::NameNotFound => {}
        }
    }
    if refused == addresses.len() {
        Ping::NotAllowed
    } else {
        Ping::NoReply
    }
}

/// Ping one address: up to [`ATTEMPTS`] requests, asking `keep_going` before
/// each.
fn ping_address(address: &SocketAddr, keep_going: &dyn Fn() -> bool) -> Ping {
    let v6 = address.is_ipv6();
    let socket = match icmp_socket(v6) {
        Ok(socket) => socket,
        Err(err) if refused(err) => return Ping::NotAllowed,
        Err(_) => return Ping::NoReply,
    };
    // A refusal here is about the address (a broadcast one, say), not about
    // pings being allowed; nothing has been sent.
    if connect(&socket, address).is_err() {
        return Ping::NoReply;
    }
    let mut echo = SocketEcho { socket };
    if exchange(&mut echo, v6, ATTEMPTS, WAIT, keep_going) {
        Ping::Answered
    } else {
        Ping::NoReply
    }
}

/// Whether this system lets this user ping at all: an IPv4 ICMP datagram
/// socket can be opened (and is closed again; nothing is sent).
pub fn allowed() -> bool {
    match icmp_socket(false) {
        Ok(_) => true,
        Err(err) => !refused(err),
    }
}

/// An ICMP datagram socket for IPv6 or IPv4, closed on exec so that no
/// program started meanwhile inherits it.
fn icmp_socket(v6: bool) -> Result<OwnedFd, Errno> {
    let (family, protocol) = if v6 {
        (AddressFamily::INET6, ipproto::ICMPV6)
    } else {
        (AddressFamily::INET, ipproto::ICMP)
    };
    socket_with(
        family,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(protocol),
    )
}

/// Whether `err`, from opening the socket, says the kernel does not let this
/// user have an ICMP datagram socket (outside `ping_group_range`, or no such
/// socket at all).
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
    /// Wait until `deadline`, after an error that ended a wait early.
    fn wait_out(&mut self, deadline: Instant);
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

    fn wait_out(&mut self, deadline: Instant) {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Send up to `attempts` echo requests over `echo`, each waiting `wait` for a
/// reply, and say whether one came: a reply to any request sent so far counts,
/// so a late reply to the first is not missed while waiting for the second.
/// `keep_going` is asked before each request; once it says no, nothing more is
/// sent. An interrupted wait goes on; any other failure to send or receive
/// ends that request's wait (its rest waited out, so the requests stay `wait`
/// apart), not the check.
fn exchange(
    echo: &mut dyn Echo,
    v6: bool,
    attempts: u16,
    wait: Duration,
    keep_going: &dyn Fn() -> bool,
) -> bool {
    let mut buf = [0u8; 512];
    for seq in 1..=attempts {
        if !keep_going() {
            return false;
        }
        let deadline = Instant::now() + wait;
        if echo.send(&echo_request(v6, seq)).is_err() {
            echo.wait_out(deadline);
            continue;
        }
        loop {
            match echo.receive(&mut buf, deadline) {
                Ok(Some(len)) => {
                    if is_echo_reply(v6, seq, &buf[..len.min(buf.len())]) {
                        return true;
                    }
                }
                Ok(None) => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    echo.wait_out(deadline);
                    break;
                }
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
        Interrupted,
    }

    /// A network that answers from a script and counts what is sent, and how
    /// often the rest of a wait was waited out.
    struct Scripted {
        sent: Vec<Vec<u8>>,
        send_fails: bool,
        script: VecDeque<Then>,
        waited_out: usize,
    }

    impl Scripted {
        fn new(script: Vec<Then>) -> Scripted {
            Scripted {
                sent: Vec::new(),
                send_fails: false,
                script: script.into(),
                waited_out: 0,
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
                Then::Interrupted => Err(io::Error::from(io::ErrorKind::Interrupted)),
            }
        }

        fn wait_out(&mut self, _deadline: Instant) {
            self.waited_out += 1;
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
        assert!(exchange(&mut net, false, 3, WAIT, &|| true));
        assert_eq!(net.sent.len(), 1, "one request was enough");
    }

    #[test]
    fn three_requests_without_a_reply_are_no_reply() {
        let mut net = Scripted::new(vec![]);
        assert!(!exchange(&mut net, true, 3, WAIT, &|| true));
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
        assert!(exchange(&mut net, false, 3, WAIT, &|| true));
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
        assert!(!exchange(&mut net, false, 3, WAIT, &|| true));
        assert_eq!(net.sent.len(), 3);
    }

    #[test]
    fn errors_end_one_wait_not_the_check_nor_its_pace() {
        // An ICMP error (unreachable, prohibited) comes back at once: the rest
        // of that wait is waited out, so the requests stay a second apart.
        let mut net = Scripted::new(vec![Then::Fail, Then::Fail, Then::Packet(reply(true, 3))]);
        assert!(exchange(&mut net, true, 3, WAIT, &|| true));
        assert_eq!(net.sent.len(), 3);
        assert_eq!(net.waited_out, 2);
        let mut down = Scripted::new(vec![]);
        down.send_fails = true;
        assert!(!exchange(&mut down, false, 3, WAIT, &|| true));
        assert_eq!(down.sent.len(), 3, "every request was tried");
        assert_eq!(down.waited_out, 3, "a second apart");
    }

    #[test]
    fn an_interrupted_wait_goes_on() {
        let mut net = Scripted::new(vec![Then::Interrupted, Then::Packet(reply(false, 1))]);
        assert!(exchange(&mut net, false, 3, WAIT, &|| true));
        assert_eq!(net.sent.len(), 1, "the same wait went on");
        assert_eq!(net.waited_out, 0);
    }

    #[test]
    fn a_check_called_off_sends_nothing_more() {
        let asked = std::cell::Cell::new(0);
        let keep_going = || {
            asked.set(asked.get() + 1);
            asked.get() == 1
        };
        let mut net = Scripted::new(vec![]);
        assert!(!exchange(&mut net, false, 3, WAIT, &keep_going));
        assert_eq!(net.sent.len(), 1, "only the request sent before");
        assert_eq!(
            ping("127.0.0.1", Family::Any, &|| false),
            Ping::NoReply,
            "nothing is sent at all"
        );
    }

    #[test]
    fn this_machine_answers_its_own_ping() {
        // Loopback only: nothing leaves the machine.
        match ping("127.0.0.1", Family::Any, &|| true) {
            Ping::Answered => {}
            Ping::NotAllowed => {
                eprintln!("skipped: this system does not allow ICMP datagram sockets here");
            }
            other => panic!("127.0.0.1 did not answer: {other:?}"),
        }
    }

    #[test]
    fn an_address_that_cannot_be_pinged_is_no_reply_not_a_refusal() {
        // connect() refuses a broadcast address at once (nothing is sent):
        // that is about the host, not about whether pings are allowed here.
        let expected = if allowed() {
            Ping::NoReply
        } else {
            Ping::NotAllowed
        };
        assert_eq!(ping("255.255.255.255", Family::Any, &|| true), expected);
    }

    #[test]
    fn a_name_that_does_not_resolve_is_name_not_found() {
        // Refused by the resolver at once: nothing is sent, not even a query.
        assert_eq!(ping("", Family::Any, &|| true), Ping::NameNotFound);
    }

    #[test]
    fn an_address_of_a_kind_ssh_would_not_use_is_not_pinged() {
        // Literal addresses: nothing is looked up and nothing is sent.
        assert_eq!(
            ping("127.0.0.1", Family::Inet6, &|| true),
            Ping::NameNotFound
        );
        assert_eq!(ping("::1", Family::Inet, &|| true), Ping::NameNotFound);
    }

    // ----- which addresses, in turn -----

    fn v4(last: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last], 0))
    }

    fn v6(last: u16) -> SocketAddr {
        SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, last], 0))
    }

    #[test]
    fn the_first_address_and_the_first_of_the_other_kind_are_tried() {
        assert_eq!(
            to_try([v6(1), v6(2), v4(1), v4(2)], Family::Any),
            [v6(1), v4(1)]
        );
        assert_eq!(to_try([v4(1), v6(1), v4(2)], Family::Any), [v4(1), v6(1)]);
        // One kind only: its first address.
        assert_eq!(to_try([v4(1), v4(2)], Family::Any), [v4(1)]);
        assert_eq!(to_try([], Family::Any), []);
    }

    #[test]
    fn only_the_kind_ssh_would_use_is_tried() {
        let both = [v6(1), v4(1), v6(2), v4(2)];
        assert_eq!(to_try(both, Family::Inet), [v4(1)]);
        assert_eq!(to_try(both, Family::Inet6), [v6(1)]);
        assert_eq!(to_try([v4(1)], Family::Inet6), []);
    }

    /// `in_turn` over `addresses`, the n-th address pinged finding
    /// `found[n]`: what it says, and the addresses it pinged.
    fn in_turn_finding(
        addresses: &[SocketAddr],
        found: &[Ping],
        keep_going: &dyn Fn() -> bool,
    ) -> (Ping, Vec<SocketAddr>) {
        let mut pinged = Vec::new();
        let ping = in_turn(addresses, keep_going, &mut |address| {
            pinged.push(*address);
            found[pinged.len() - 1]
        });
        (ping, pinged)
    }

    #[test]
    fn the_next_address_is_tried_only_after_no_reply() {
        let both = [v6(1), v4(1)];
        assert_eq!(
            in_turn_finding(&both, &[Ping::Answered], &|| true),
            (Ping::Answered, vec![v6(1)])
        );
        assert_eq!(
            in_turn_finding(&both, &[Ping::NoReply, Ping::Answered], &|| true),
            (Ping::Answered, both.to_vec())
        );
        assert_eq!(
            in_turn_finding(&both, &[Ping::NoReply, Ping::NoReply], &|| true),
            (Ping::NoReply, both.to_vec())
        );
    }

    #[test]
    fn not_allowed_only_when_every_address_was_refused() {
        // IPv6 sockets refused (IPv6 off in this kernel, say), IPv4 fine.
        let both = [v6(1), v4(1)];
        let found = |found: &[Ping]| in_turn_finding(&both, found, &|| true).0;
        assert_eq!(found(&[Ping::NotAllowed, Ping::Answered]), Ping::Answered);
        assert_eq!(found(&[Ping::NotAllowed, Ping::NoReply]), Ping::NoReply);
        assert_eq!(found(&[Ping::NoReply, Ping::NotAllowed]), Ping::NoReply);
        assert_eq!(
            found(&[Ping::NotAllowed, Ping::NotAllowed]),
            Ping::NotAllowed
        );
    }

    #[test]
    fn no_address_is_name_not_found() {
        assert_eq!(
            in_turn_finding(&[], &[], &|| true),
            (Ping::NameNotFound, vec![])
        );
    }

    #[test]
    fn a_check_called_off_tries_no_further_address() {
        let asked = std::cell::Cell::new(0);
        let keep_going = || {
            asked.set(asked.get() + 1);
            asked.get() == 1
        };
        assert_eq!(
            in_turn_finding(
                &[v6(1), v4(1)],
                &[Ping::NoReply, Ping::Answered],
                &keep_going
            ),
            (Ping::NoReply, vec![v6(1)])
        );
    }
}
