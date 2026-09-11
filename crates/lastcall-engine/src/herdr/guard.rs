//! The protocol guard (docs/spec/00-spec.md §4.6): `ping`, compare `pong.protocol`, degrade
//! politely. Imitates herdr's own `src/cli/protocol_guard.rs`: the server never rejects a
//! mismatched client, so enforcement is entirely ours. A mismatch is a notice and standalone
//! mode, never a panic.

use super::transport::{Transport, TransportError};
use super::wire::{self, Pong};

/// The wire protocol the consumed surface (§5) was hand-checked against, source line by source
/// line: herdr `master` @ `5158ada` (2026-08-31), whose embedded schema says `"protocol": 21`.
/// It is deliberately **not** the pinned release's protocol (v0.9.0 answers 22): later
/// protocols are admitted by the fixture diff (`herdr_real_schema_consumed_surface_unchanged`),
/// which is a check of the surface we consume, not a re-read of §5. Moving this constant means
/// redoing that hand check.
pub const SUPPORTED_PROTOCOL: u32 = 21;

/// Every protocol the client accepts. **Construction finding (Phase 1):** the published
/// v0.8.2 release asset (`just herdr-fetch`) answers `ping` with `protocol: 20`, while the
/// spec's §5.2 "v0.8.2 = protocol 21" was verified against a post-release `master` whose
/// `Cargo.toml` still said 0.8.2. The real-herdr integration test proves the consumed surface
/// works on 20, so both are accepted. **Phase 9b:** the v0.9.0 release asset answers 22, and
/// the consumed-surface projection regenerated from its embedded schema differs from v0.8.2's
/// only by additions we do not read (`WorktreeListParams.trust_repository`,
/// `ServerCapabilities.{endpoint_protocol_generation, health_check, surface_interest}`), so 22
/// joins the set and the real-herdr subset is green on both assets. Additive to §5.2 (proposed
/// v1.10 item 5); anything outside this set degrades to standalone with a notice.
pub const SUPPORTED_PROTOCOLS: &[u32] = &[20, SUPPORTED_PROTOCOL, 22];

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
    fn guard_release_protocol_22_is_ok_too() {
        // The published v0.9.0 asset answers 22 (Phase 9b deliverable 14).
        assert_eq!(
            compare(&pong(22)),
            Compat::Ok {
                version: "0.8.2".into(),
                protocol: 22
            }
        );
        assert_eq!(compare(&pong(22)).notice(), None);
    }

    #[test]
    fn guard_accepts_exactly_20_21_22() {
        assert_eq!(SUPPORTED_PROTOCOLS, &[20, 21, 22]);
        for protocol in [20, 21, 22] {
            assert!(
                compare(&pong(protocol)).is_ok(),
                "protocol {protocol} should be accepted"
            );
        }
        for protocol in [19, 23] {
            assert!(
                !compare(&pong(protocol)).is_ok(),
                "protocol {protocol} should be refused"
            );
        }
    }

    #[test]
    fn guard_newer_server_mismatch_has_notice_and_no_panic() {
        let c = compare(&pong(23));
        match &c {
            Compat::Mismatch {
                server_protocol,
                notice,
                ..
            } => {
                assert_eq!(*server_protocol, 23);
                assert!(notice.contains("newer than"), "{notice}");
                assert!(notice.contains("standalone"), "{notice}");
                assert!(notice.contains("20/21/22"), "{notice}");
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
        assert!(notice.contains("20/21/22"), "{notice}");
    }

    #[test]
    fn guard_transport_failure_is_absent() {
        let c = verdict(Err(TransportError::ClosedBeforeResponse));
        assert!(matches!(c, Compat::Absent { .. }));
        assert!(c.notice().unwrap().contains("not reachable"));
    }
}
