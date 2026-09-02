//! The protocol guard (docs/spec/00-spec.md §4.6): `ping`, compare `pong.protocol`, degrade
//! politely. Imitates herdr's own `src/cli/protocol_guard.rs`: the server never rejects a
//! mismatched client, so enforcement is entirely ours. A mismatch is a notice and standalone
//! mode, never a panic.

use super::transport::{Transport, TransportError};
use super::wire::{self, Pong};

/// The wire protocol the consumed surface (§5) was hand-checked against: herdr `master` @
/// `5158ada` (2026-08-31), whose embedded schema says `"protocol": 21`.
pub const SUPPORTED_PROTOCOL: u32 = 21;

/// Every protocol the client accepts. **Construction finding (Phase 1):** the published
/// v0.8.2 release asset (`just herdr-fetch`) answers `ping` with `protocol: 20`, while the
/// spec's §5.2 "v0.8.2 = protocol 21" was verified against a post-release `master` whose
/// `Cargo.toml` still said 0.8.2. The real-herdr integration test proves the consumed surface
/// works on 20, so both are accepted. Additive to §5.2 (proposed v1.1); anything outside this
/// set degrades to standalone with a notice.
pub const SUPPORTED_PROTOCOLS: &[u32] = &[20, SUPPORTED_PROTOCOL];

/// The verdict of a ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    /// Same protocol; carry on.
    Ok { version: String, protocol: u32 },
    /// Different protocol; `notice` is the one-line standalone reason.
    Mismatch {
        version: String,
        server_protocol: u32,
        notice: String,
    },
    /// No server answered (socket missing, refused, timed out, garbage).
    Absent { reason: String },
}

impl Compat {
    pub fn is_ok(&self) -> bool {
        matches!(self, Compat::Ok { .. })
    }

    /// The standalone notice for anything but `Ok`.
    pub fn notice(&self) -> Option<&str> {
        match self {
            Compat::Ok { .. } => None,
            Compat::Mismatch { notice, .. } => Some(notice),
            Compat::Absent { reason } => Some(reason),
        }
    }
}

/// Pure comparison of a pong against [`SUPPORTED_PROTOCOLS`].
pub fn compare(pong: &Pong) -> Compat {
    if SUPPORTED_PROTOCOLS.contains(&pong.protocol) {
        return Compat::Ok {
            version: pong.version.clone(),
            protocol: pong.protocol,
        };
    }
    let relation = if pong.protocol > SUPPORTED_PROTOCOL {
        "newer than"
    } else {
        "older than"
    };
    let supported = SUPPORTED_PROTOCOLS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("/");
    let notice = format!(
        "herdr {} speaks protocol {}, {relation} the supported protocol {supported}; \
         running standalone (upgrade lastcall or herdr so they match)",
        pong.version, pong.protocol
    );
    Compat::Mismatch {
        version: pong.version.clone(),
        server_protocol: pong.protocol,
        notice,
    }
}

/// Turn a ping outcome into a verdict.
pub fn verdict(ping: Result<Pong, TransportError>) -> Compat {
    match ping {
        Ok(pong) => compare(&pong),
        Err(err) => Compat::Absent {
            reason: format!("herdr not reachable: {err}"),
        },
    }
}

/// Send `ping` (params `{}`) and parse the pong.
pub async fn ping<T: Transport + ?Sized>(transport: &T) -> Result<Pong, TransportError> {
    let result = transport
        .request(wire::method::PING, serde_json::json!({}))
        .await?;
    let pong: Pong = serde_json::from_value(result).map_err(wire::WireError::from)?;
    Ok(pong)
}

/// `ping` then compare.
pub async fn probe<T: Transport + ?Sized>(transport: &T) -> Compat {
    verdict(ping(transport).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pong(protocol: u32) -> Pong {
        Pong {
            version: "0.8.2".into(),
            protocol,
            capabilities: None,
        }
    }

    #[test]
    fn guard_matching_protocol_is_ok() {
        assert_eq!(
            compare(&pong(21)),
            Compat::Ok {
                version: "0.8.2".into(),
                protocol: 21
            }
        );
        assert!(compare(&pong(SUPPORTED_PROTOCOL)).is_ok());
        assert_eq!(compare(&pong(21)).notice(), None);
    }

    #[test]
    fn guard_release_protocol_20_is_ok_too() {
        // The published v0.8.2 asset answers 20 (see SUPPORTED_PROTOCOLS).
        assert_eq!(
            compare(&pong(20)),
            Compat::Ok {
                version: "0.8.2".into(),
                protocol: 20
            }
        );
        assert!(SUPPORTED_PROTOCOLS.contains(&SUPPORTED_PROTOCOL));
    }

    #[test]
    fn guard_newer_server_mismatch_has_notice_and_no_panic() {
        let c = compare(&pong(22));
        match &c {
            Compat::Mismatch {
                server_protocol,
                notice,
                ..
            } => {
                assert_eq!(*server_protocol, 22);
                assert!(notice.contains("newer than"), "{notice}");
                assert!(notice.contains("standalone"), "{notice}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!c.is_ok());
        assert!(c.notice().is_some());
    }

    #[test]
    fn guard_older_server_mismatch_says_older() {
        let c = compare(&pong(19));
        assert!(matches!(
            c,
            Compat::Mismatch {
                server_protocol: 19,
                ..
            }
        ));
        let notice = c.notice().unwrap();
        assert!(notice.contains("older than"), "{notice}");
        assert!(notice.contains("20/21"), "{notice}");
    }

    #[test]
    fn guard_transport_failure_is_absent() {
        let c = verdict(Err(TransportError::ClosedBeforeResponse));
        assert!(matches!(c, Compat::Absent { .. }));
        assert!(c.notice().unwrap().contains("not reachable"));
    }
}
