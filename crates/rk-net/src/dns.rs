//! A tiny embedded DNS server: it answers A queries for cluster service names
//! (`<svc>.<ns>` and `<svc>.<ns>.svc.cluster.local`) from a map the agent
//! keeps current, and NXDOMAINs everything else. Pods point their
//! `/etc/resolv.conf` at their node's gateway, where this listens.
//!
//! Only the sliver of the DNS wire format we need is implemented — a single
//! question, A records, a compression pointer back to the question name — so
//! the parse/build is small and fully testable.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use crate::NetError;

/// Service name (lowercase, no trailing dot) → cluster IP. Both `<svc>.<ns>`
/// and the fully-qualified `…svc.cluster.local` forms are inserted.
pub type Records = Arc<Mutex<HashMap<String, Ipv4Addr>>>;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;
const TTL: u32 = 30;

/// A parsed query: transaction id, question name (dotted, lowercased, no
/// trailing dot), question type, and the byte offset just past the question.
#[derive(Debug, PartialEq, Eq)]
struct Query {
    id: u16,
    name: String,
    qtype: u16,
    question_end: usize,
}

fn parse_query(buf: &[u8]) -> Option<Query> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    if qdcount < 1 {
        return None;
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = *buf.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression in a question we don't expect
        }
        pos += 1;
        let label = buf.get(pos..pos + len)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        pos += len;
    }
    let qtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
    // skip qtype(2) + qclass(2)
    let question_end = pos + 4;
    if question_end > buf.len() {
        return None;
    }
    Some(Query {
        id,
        name: labels.join("."),
        qtype,
        question_end,
    })
}

/// Build the response for a parsed query given the lookup result.
fn build_response(query_bytes: &[u8], q: &Query, answer: Option<Ipv4Addr>) -> Vec<u8> {
    let mut resp = query_bytes[..q.question_end].to_vec();
    // An A record is appended only when the name resolved AND an A was asked.
    let a_record = answer.filter(|_| q.qtype == TYPE_A);

    // Flags byte 1: QR=1, AA=1, preserve the request's RD bit.
    resp[2] = 0x84 | (query_bytes[2] & 0x01);
    // Flags byte 2: RA=0, RCODE = 0 (NoError) or 3 (NXDOMAIN for an unknown name).
    resp[3] = if answer.is_none() { 3 } else { 0 };
    // Counts: QDCOUNT stays 1; ANCOUNT 1 on an A hit, else 0; NS/AR 0.
    let ancount: u16 = u16::from(a_record.is_some());
    resp[6..8].copy_from_slice(&ancount.to_be_bytes());
    resp[8..12].copy_from_slice(&[0, 0, 0, 0]);

    if let Some(ip) = a_record {
        resp.extend_from_slice(&[0xC0, 0x0C]); // name: pointer to the question
        resp.extend_from_slice(&TYPE_A.to_be_bytes());
        resp.extend_from_slice(&CLASS_IN.to_be_bytes());
        resp.extend_from_slice(&TTL.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(&ip.octets());
    }
    resp
}

fn lookup(records: &Records, name: &str) -> Option<Ipv4Addr> {
    records.lock().ok()?.get(name).copied()
}

/// Serve DNS on `bind` forever, answering from `records`.
pub async fn serve(bind: SocketAddr, records: Records) -> Result<(), NetError> {
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|err| NetError::Netlink {
            op: "binding DNS socket",
            detail: format!("{bind}: {err}"),
        })?;
    tracing::info!(%bind, "embedded DNS server listening");
    let mut buf = [0u8; 512];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let query = &buf[..len];
        let Some(parsed) = parse_query(query) else {
            continue;
        };
        let answer = lookup(&records, &parsed.name);
        let response = build_response(query, &parsed, answer);
        let _ = socket.send_to(&response, from).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query for `name` (A/IN), txid 0x1234.
    fn query_for(name: &str) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    fn records(entries: &[(&str, &str)]) -> Records {
        let map = entries
            .iter()
            .map(|(n, ip)| (n.to_string(), ip.parse().unwrap()))
            .collect();
        Arc::new(Mutex::new(map))
    }

    #[test]
    fn parses_a_question() {
        let q = parse_query(&query_for("web.default.svc.cluster.local")).unwrap();
        assert_eq!(q.id, 0x1234);
        assert_eq!(q.name, "web.default.svc.cluster.local");
        assert_eq!(q.qtype, TYPE_A);
    }

    #[test]
    fn answers_a_known_service() {
        let bytes = query_for("web.default");
        let q = parse_query(&bytes).unwrap();
        let resp = build_response(&bytes, &q, Some("10.96.0.10".parse().unwrap()));
        // Header: response, NoError, one answer.
        assert_eq!(resp[2] & 0x80, 0x80, "QR set");
        assert_eq!(resp[3] & 0x0f, 0, "NoError");
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1, "one answer");
        // Answer RDATA is the IP.
        let ip = &resp[resp.len() - 4..];
        assert_eq!(ip, [10, 96, 0, 10]);
        // Answer name is a compression pointer to the question.
        assert_eq!(&resp[q.question_end..q.question_end + 2], &[0xC0, 0x0C]);
    }

    #[test]
    fn nxdomain_for_unknown_names() {
        let bytes = query_for("nope.default");
        let q = parse_query(&bytes).unwrap();
        let resp = build_response(&bytes, &q, None);
        assert_eq!(resp[3] & 0x0f, 3, "NXDOMAIN");
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0, "no answers");
    }

    #[test]
    fn lookup_hits_both_short_and_fqdn_forms() {
        let recs = records(&[
            ("web.default", "10.96.0.10"),
            ("web.default.svc.cluster.local", "10.96.0.10"),
        ]);
        assert_eq!(
            lookup(&recs, "web.default").unwrap().to_string(),
            "10.96.0.10"
        );
        assert_eq!(
            lookup(&recs, "web.default.svc.cluster.local")
                .unwrap()
                .to_string(),
            "10.96.0.10"
        );
        assert!(lookup(&recs, "other.default").is_none());
    }
}
