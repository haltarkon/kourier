//! The gate's side of a SOCKS5 handshake (RFC 1928 with the username/password method of RFC 1929),
//! as much of it as TDLib speaks (`tdnet/td/net/Socks5.cpp` of the pinned commit, BSL-1.0, read
//! for behaviour): TDLib offers methods `00` and `02` when it has a login (`:21-31`), so the gate
//! SELECTS `02`; a greeting without `02` is refused (`FF`). Only CONNECT, only IPv4 and IPv6
//! literals (TDLib sends nothing else, `:101-115`).
//!
//! Nothing here logs: the caller counts the outcome.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The SOCKS protocol version.
const VER: u8 = 5;
/// The username/password sub-negotiation's version (RFC 1929).
const AUTH_VER: u8 = 1;
/// "Username/password".
const METHOD_LOGIN: u8 = 0x02;
/// "No acceptable methods".
const METHOD_REFUSED: u8 = 0xFF;
const CMD_CONNECT: u8 = 1;
const ATYP_V4: u8 = 1;
const ATYP_V6: u8 = 4;

/// The reply codes the gate writes (RFC 1928 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Rep {
    Succeeded = 0x00,
    GeneralFailure = 0x01,
    NotAllowed = 0x02,
    Refused = 0x05,
    CommandNotSupported = 0x07,
    AddressTypeNotSupported = 0x08,
}

/// Why a handshake ended without a CONNECT to forward. Each is one count of the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not SOCKS5, a bad sub-negotiation version, or the peer closed half way.
    Protocol,
    /// The greeting did not offer username/password.
    NoLogin,
    /// A wrong username or password.
    Auth,
    /// BIND or UDP ASSOCIATE.
    Command,
    /// A domain name or an unknown address type.
    AddressType,
}

/// The gate's login, compared in constant time.
pub(crate) struct Login<'a> {
    pub(crate) user: &'a [u8],
    pub(crate) pass: &'a [u8],
}

/// Equal in time that depends only on the lengths (both are public: 22 bytes each).
fn same(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0xFF);
    }
    diff == 0
}

/// Reads the greeting, the login and the request; writes the method choice, the login status and,
/// for a refused request, its reply. Returns the destination of an accepted CONNECT (the caller
/// still checks it against the list and writes the final reply with [`reply`]).
pub(crate) async fn handshake<S>(s: &mut S, login: &Login<'_>) -> Result<SocketAddr, Refusal>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let io = |_| Refusal::Protocol;
    // Greeting: VER NMETHODS METHODS.
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await.map_err(io)?;
    if head[0] != VER || head[1] == 0 {
        return Err(Refusal::Protocol);
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    s.read_exact(&mut methods).await.map_err(io)?;
    if !methods.contains(&METHOD_LOGIN) {
        // "00 only" and anything else without a login: no acceptable method.
        let _ = s.write_all(&[VER, METHOD_REFUSED]).await;
        return Err(Refusal::NoLogin);
    }
    s.write_all(&[VER, METHOD_LOGIN]).await.map_err(io)?;

    // RFC 1929: VER ULEN UNAME PLEN PASSWD.
    let mut ver_len = [0u8; 2];
    s.read_exact(&mut ver_len).await.map_err(io)?;
    if ver_len[0] != AUTH_VER {
        return Err(Refusal::Protocol);
    }
    let mut user = vec![0u8; usize::from(ver_len[1])];
    s.read_exact(&mut user).await.map_err(io)?;
    let mut plen = [0u8; 1];
    s.read_exact(&mut plen).await.map_err(io)?;
    let mut pass = vec![0u8; usize::from(plen[0])];
    s.read_exact(&mut pass).await.map_err(io)?;
    // Both comparisons always run.
    let user_ok = same(&user, login.user);
    let pass_ok = same(&pass, login.pass);
    if !(user_ok & pass_ok) {
        let _ = s.write_all(&[AUTH_VER, 1]).await;
        return Err(Refusal::Auth);
    }
    s.write_all(&[AUTH_VER, 0]).await.map_err(io)?;

    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT.
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await.map_err(io)?;
    if req[0] != VER {
        return Err(Refusal::Protocol);
    }
    if req[1] != CMD_CONNECT {
        let _ = reply(s, Rep::CommandNotSupported).await;
        return Err(Refusal::Command);
    }
    let ip = match req[3] {
        ATYP_V4 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await.map_err(io)?;
            IpAddr::from(Ipv4Addr::from(a))
        }
        ATYP_V6 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await.map_err(io)?;
            IpAddr::from(Ipv6Addr::from(a))
        }
        // A domain name (type 3) or an unknown type.
        _ => {
            let _ = reply(s, Rep::AddressTypeNotSupported).await;
            return Err(Refusal::AddressType);
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await.map_err(io)?;
    Ok(SocketAddr::new(ip, u16::from_be_bytes(port)))
}

/// Writes a reply with an empty bound address (`0.0.0.0:0`: TDLib reads and ignores it).
pub(crate) async fn reply<S>(s: &mut S, rep: Rep) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    s.write_all(&[VER, rep as u8, 0, ATYP_V4, 0, 0, 0, 0, 0, 0])
        .await?;
    s.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_comparison_needs_equal_bytes_and_lengths() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
        assert!(!same(b"", b"a"));
        // A shorter value padded with what the filler would be is still different.
        assert!(!same(b"ab\xFF", b"ab"));
        assert!(same(b"", b""));
    }
}
