//! The protocol guard (docs/spec/00-spec.md §4.6): `ping`, compare `pong.protocol`, degrade
//! politely. Imitates herdr's own `src/cli/protocol_guard.rs`: the server never rejects a
//! mismatched client, so enforcement is entirely ours. A mismatch is a notice and standalone
//! mode, never a panic.

use super::transport::{Transport, TransportError};
use super::wire::{self, Pong};

/// The one wire protocol we speak in v1 (herdr v0.8.2).
pub const SUPPORTED_PROTOCOL: u32 = 21;

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

/// Pure comparison of a pong against [`SUPPORTED_PROTOCOL`].
pub fn compare(pong: &Pong) -> Compat {
    if pong.protocol == SUPPORTED_PROTOCOL {
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
    let notice = format!(
        "herdr {} speaks protocol {}, {relation} the supported protocol {SUPPORTED_PROTOCOL}; \
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
        let c = compare(&pong(20));
        assert!(c.notice().unwrap().contains("older than"));
    }

    #[test]
    fn guard_transport_failure_is_absent() {
        let c = verdict(Err(TransportError::ClosedBeforeResponse));
        assert!(matches!(c, Compat::Absent { .. }));
        assert!(c.notice().unwrap().contains("not reachable"));
    }
}
