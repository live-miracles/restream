//! Linux receiver-side TCP socket statistics for RTMP publishers.
//!
//! `ss -tinmH` exposes fields that are not consistently available through the
//! portable socket APIs used by Tokio, including receive-buffer occupancy,
//! receive RTT, last-receive age, and out-of-order packet counts.

use std::io;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TcpReceiverStats {
    pub tcp_rtt_ms: Option<f64>,
    pub tcp_rtt_var_ms: Option<f64>,
    pub tcp_bytes_received: Option<u64>,
    pub tcp_last_rcv_ms: Option<u64>,
    pub tcp_rcv_rtt_ms: Option<f64>,
    pub tcp_rcv_space: Option<u64>,
    pub tcp_rcv_ooopack: Option<u64>,
    pub tcp_skmem_rmem_alloc: Option<u64>,
    pub tcp_skmem_rmem_max: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct TcpSocketEntry {
    state: String,
    local_key: String,
    peer_key: String,
    stats: TcpReceiverStats,
}

fn normalize_socket_key(value: &str) -> Option<String> {
    let raw = value.trim();
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (host, port) = rest.rsplit_once("]:")?;
        (host, port)
    } else {
        raw.rsplit_once(':')?
    };

    let host = host
        .trim()
        .to_ascii_lowercase()
        .strip_prefix("::ffff:")
        .unwrap_or(host.trim())
        .to_string();
    if host.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    Some(format!("{}:{}", host, port))
}

fn parse_stats_line(line: &str) -> TcpReceiverStats {
    fn number<T: std::str::FromStr>(line: &str, label: &str) -> Option<T> {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(label))
            .and_then(|value| value.parse().ok())
    }

    fn decimal(line: &str, label: &str) -> Option<f64> {
        number(line, label)
    }

    let (tcp_rtt_ms, tcp_rtt_var_ms) = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("rtt:"))
        .and_then(|value| value.split_once('/'))
        .map(|(rtt, var)| (rtt.parse().ok(), var.parse().ok()))
        .unwrap_or((None, None));

    let (tcp_skmem_rmem_alloc, tcp_skmem_rmem_max) = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("skmem:(r"))
        .and_then(|value| value.split_once(",rb"))
        .map(|(used, rest)| {
            let max = rest.split(',').next().unwrap_or_default();
            (used.parse().ok(), max.parse().ok())
        })
        .unwrap_or((None, None));

    TcpReceiverStats {
        tcp_rtt_ms,
        tcp_rtt_var_ms,
        tcp_bytes_received: number(line, "bytes_received:"),
        tcp_last_rcv_ms: number(line, "lastrcv:"),
        tcp_rcv_rtt_ms: decimal(line, "rcv_rtt:"),
        tcp_rcv_space: number(line, "rcv_space:"),
        tcp_rcv_ooopack: number(line, "rcv_ooopack:"),
        tcp_skmem_rmem_alloc,
        tcp_skmem_rmem_max,
    }
}

fn parse_ss_tcp_socket_entries(stdout: &str) -> Vec<TcpSocketEntry> {
    let lines: Vec<&str> = stdout.lines().collect();
    let mut entries = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
            index += 1;
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            index += 1;
            continue;
        }

        let Some(local_key) = normalize_socket_key(parts[3]) else {
            index += 1;
            continue;
        };
        let Some(peer_key) = normalize_socket_key(parts[4]) else {
            index += 1;
            continue;
        };

        let mut stats_lines = Vec::new();
        index += 1;
        while index < lines.len() && lines[index].starts_with(char::is_whitespace) {
            stats_lines.push(lines[index].trim());
            index += 1;
        }

        entries.push(TcpSocketEntry {
            state: parts[0].to_string(),
            local_key,
            peer_key,
            stats: parse_stats_line(&stats_lines.join(" ")),
        });
    }

    entries
}

pub async fn collect_rtmp_receiver_stats(
    peer_addr: &str,
    local_port: u16,
) -> io::Result<Option<TcpReceiverStats>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (peer_addr, local_port);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "receiver TCP statistics require Linux",
        ));
    }

    #[cfg(target_os = "linux")]
    {
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::process::Command::new("ss").arg("-tinmH").output(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ss timed out"))??;

        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ss exited with status {}",
                output.status
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let peer_key = normalize_socket_key(peer_addr);
        Ok(parse_ss_tcp_socket_entries(&stdout)
            .into_iter()
            .find(|entry| {
                entry.state == "ESTAB"
                    && entry.local_key.ends_with(&format!(":{}", local_port))
                    && Some(entry.peer_key.as_str()) == peer_key.as_deref()
            })
            .map(|entry| entry.stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_receiver_side_metrics_and_skmem() {
        let output = "ESTAB 0 0 127.0.0.1:1935 10.0.0.5:55000\n\
            \t skmem:(r4096,rb2097152,t0,tb87040,f0,w0,o0,bl0,d0) cubic rtt:12.0/2.0 cwnd:10 bytes_received:1234567 lastrcv:42 rcv_rtt:8.5 rcv_space:65536 rcv_ooopack:15\n";

        let entries = parse_ss_tcp_socket_entries(output);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].stats,
            TcpReceiverStats {
                tcp_rtt_ms: Some(12.0),
                tcp_rtt_var_ms: Some(2.0),
                tcp_bytes_received: Some(1_234_567),
                tcp_last_rcv_ms: Some(42),
                tcp_rcv_rtt_ms: Some(8.5),
                tcp_rcv_space: Some(65_536),
                tcp_rcv_ooopack: Some(15),
                tcp_skmem_rmem_alloc: Some(4_096),
                tcp_skmem_rmem_max: Some(2_097_152),
            }
        );
    }

    #[test]
    fn normalizes_ipv4_mapped_addresses() {
        assert_eq!(
            normalize_socket_key("[::ffff:127.0.0.1]:55111"),
            Some("127.0.0.1:55111".to_string())
        );
    }
}
