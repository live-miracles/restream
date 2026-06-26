//! Native Linux TCP statistics for RTMP publishers and egress targets.
//!
//! The RTMP server/egress task owns the socket, so it can read `TCP_INFO` and
//! `SO_MEMINFO` directly without spawning `ss` or matching address strings.

use std::io;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TcpReceiverStats {
    pub tcp_congestion_algorithm: Option<String>,
    pub tcp_rtt_ms: Option<f64>,
    pub tcp_rtt_var_ms: Option<f64>,
    pub tcp_bytes_received: Option<u64>,
    pub tcp_bytes_sent: Option<u64>,
    pub tcp_bytes_acked: Option<u64>,
    pub tcp_bytes_retrans: Option<u64>,
    pub tcp_last_rcv_ms: Option<u64>,
    pub tcp_last_snd_ms: Option<u64>,
    pub tcp_rcv_rtt_ms: Option<f64>,
    pub tcp_rcv_space: Option<u64>,
    pub tcp_rcv_ooopack: Option<u64>,
    pub tcp_snd_mss: Option<u64>,
    pub tcp_pmtu: Option<u64>,
    pub tcp_unacked: Option<u64>,
    pub tcp_sacked: Option<u64>,
    pub tcp_lost: Option<u64>,
    pub tcp_retrans: Option<u64>,
    pub tcp_snd_cwnd: Option<u64>,
    pub tcp_snd_ssthresh: Option<u64>,
    pub tcp_advmss: Option<u64>,
    pub tcp_reordering: Option<u64>,
    pub tcp_notsent_bytes: Option<u64>,
    pub tcp_total_retrans: Option<u64>,
    pub tcp_pacing_rate_bps: Option<u64>,
    pub tcp_max_pacing_rate_bps: Option<u64>,
    pub tcp_delivery_rate_bps: Option<u64>,
    pub tcp_segs_out: Option<u64>,
    pub tcp_data_segs_out: Option<u64>,
    pub tcp_delivered: Option<u64>,
    pub tcp_delivered_ce: Option<u64>,
    pub tcp_busy_time_ms: Option<u64>,
    pub tcp_rwnd_limited_ms: Option<u64>,
    pub tcp_sndbuf_limited_ms: Option<u64>,
    pub tcp_dsack_dups: Option<u64>,
    pub tcp_reord_seen: Option<u64>,
    pub tcp_snd_wnd: Option<u64>,
    pub tcp_total_rto: Option<u64>,
    pub tcp_total_rto_recoveries: Option<u64>,
    pub tcp_total_rto_time_ms: Option<u64>,
    pub tcp_skmem_rmem_alloc: Option<u64>,
    pub tcp_skmem_rmem_max: Option<u64>,
    pub tcp_skmem_wmem_alloc: Option<u64>,
    pub tcp_skmem_wmem_max: Option<u64>,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct LinuxTcpInfo {
    tcpi_state: u8,
    tcpi_ca_state: u8,
    tcpi_retransmits: u8,
    tcpi_probes: u8,
    tcpi_backoff: u8,
    tcpi_options: u8,
    tcpi_snd_rcv_wscale: u8,
    tcpi_delivery_rate_flags: u8,
    tcpi_rto: u32,
    tcpi_ato: u32,
    tcpi_snd_mss: u32,
    tcpi_rcv_mss: u32,
    tcpi_unacked: u32,
    tcpi_sacked: u32,
    tcpi_lost: u32,
    tcpi_retrans: u32,
    tcpi_fackets: u32,
    tcpi_last_data_sent: u32,
    tcpi_last_ack_sent: u32,
    tcpi_last_data_recv: u32,
    tcpi_last_ack_recv: u32,
    tcpi_pmtu: u32,
    tcpi_rcv_ssthresh: u32,
    tcpi_rtt: u32,
    tcpi_rttvar: u32,
    tcpi_snd_ssthresh: u32,
    tcpi_snd_cwnd: u32,
    tcpi_advmss: u32,
    tcpi_reordering: u32,
    tcpi_rcv_rtt: u32,
    tcpi_rcv_space: u32,
    tcpi_total_retrans: u32,
    tcpi_pacing_rate: u64,
    tcpi_max_pacing_rate: u64,
    tcpi_bytes_acked: u64,
    tcpi_bytes_received: u64,
    tcpi_segs_out: u32,
    tcpi_segs_in: u32,
    tcpi_notsent_bytes: u32,
    tcpi_min_rtt: u32,
    tcpi_data_segs_in: u32,
    tcpi_data_segs_out: u32,
    tcpi_delivery_rate: u64,
    tcpi_busy_time: u64,
    tcpi_rwnd_limited: u64,
    tcpi_sndbuf_limited: u64,
    tcpi_delivered: u32,
    tcpi_delivered_ce: u32,
    tcpi_bytes_sent: u64,
    tcpi_bytes_retrans: u64,
    tcpi_dsack_dups: u32,
    tcpi_reord_seen: u32,
    tcpi_rcv_ooopack: u32,
    tcpi_snd_wnd: u32,
    tcpi_rcv_wnd: u32,
    tcpi_rehash: u32,
    tcpi_total_rto: u16,
    tcpi_total_rto_recoveries: u16,
    tcpi_total_rto_time: u32,
}

#[cfg(target_os = "linux")]
fn field_available<T>(returned_len: usize, offset: usize) -> bool {
    returned_len >= offset + std::mem::size_of::<T>()
}

#[cfg(target_os = "linux")]
fn stats_from_tcp_info(info: &LinuxTcpInfo, returned_len: usize) -> TcpReceiverStats {
    TcpReceiverStats {
        tcp_rtt_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rtt),
        )
        .then_some(info.tcpi_rtt as f64 / 1_000.0),
        tcp_rtt_var_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rttvar),
        )
        .then_some(info.tcpi_rttvar as f64 / 1_000.0),
        tcp_bytes_received: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_bytes_received),
        )
        .then_some(info.tcpi_bytes_received),
        tcp_bytes_sent: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_bytes_sent),
        )
        .then_some(info.tcpi_bytes_sent),
        tcp_bytes_acked: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_bytes_acked),
        )
        .then_some(info.tcpi_bytes_acked),
        tcp_bytes_retrans: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_bytes_retrans),
        )
        .then_some(info.tcpi_bytes_retrans),
        tcp_last_rcv_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_last_data_recv),
        )
        .then_some(info.tcpi_last_data_recv as u64),
        tcp_last_snd_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_last_data_sent),
        )
        .then_some(info.tcpi_last_data_sent as u64),
        tcp_rcv_rtt_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rcv_rtt),
        )
        .then_some(info.tcpi_rcv_rtt as f64 / 1_000.0),
        tcp_rcv_space: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rcv_space),
        )
        .then_some(info.tcpi_rcv_space as u64),
        tcp_rcv_ooopack: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rcv_ooopack),
        )
        .then_some(info.tcpi_rcv_ooopack as u64),
        tcp_snd_mss: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_snd_mss),
        )
        .then_some(info.tcpi_snd_mss as u64),
        tcp_pmtu: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_pmtu),
        )
        .then_some(info.tcpi_pmtu as u64),
        tcp_unacked: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_unacked),
        )
        .then_some(info.tcpi_unacked as u64),
        tcp_sacked: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_sacked),
        )
        .then_some(info.tcpi_sacked as u64),
        tcp_lost: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_lost),
        )
        .then_some(info.tcpi_lost as u64),
        tcp_retrans: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_retrans),
        )
        .then_some(info.tcpi_retrans as u64),
        tcp_snd_cwnd: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_snd_cwnd),
        )
        .then_some(info.tcpi_snd_cwnd as u64),
        tcp_snd_ssthresh: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_snd_ssthresh),
        )
        .then_some(info.tcpi_snd_ssthresh as u64),
        tcp_advmss: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_advmss),
        )
        .then_some(info.tcpi_advmss as u64),
        tcp_reordering: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_reordering),
        )
        .then_some(info.tcpi_reordering as u64),
        tcp_notsent_bytes: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_notsent_bytes),
        )
        .then_some(info.tcpi_notsent_bytes as u64),
        tcp_total_retrans: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_total_retrans),
        )
        .then_some(info.tcpi_total_retrans as u64),
        tcp_pacing_rate_bps: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_pacing_rate),
        )
        .then_some(info.tcpi_pacing_rate),
        tcp_max_pacing_rate_bps: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_max_pacing_rate),
        )
        .then_some(info.tcpi_max_pacing_rate),
        tcp_delivery_rate_bps: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_delivery_rate),
        )
        .then_some(info.tcpi_delivery_rate),
        tcp_segs_out: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_segs_out),
        )
        .then_some(info.tcpi_segs_out as u64),
        tcp_data_segs_out: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_data_segs_out),
        )
        .then_some(info.tcpi_data_segs_out as u64),
        tcp_delivered: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_delivered),
        )
        .then_some(info.tcpi_delivered as u64),
        tcp_delivered_ce: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_delivered_ce),
        )
        .then_some(info.tcpi_delivered_ce as u64),
        tcp_busy_time_ms: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_busy_time),
        )
        .then_some(info.tcpi_busy_time / 1_000),
        tcp_rwnd_limited_ms: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_rwnd_limited),
        )
        .then_some(info.tcpi_rwnd_limited / 1_000),
        tcp_sndbuf_limited_ms: field_available::<u64>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_sndbuf_limited),
        )
        .then_some(info.tcpi_sndbuf_limited / 1_000),
        tcp_dsack_dups: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_dsack_dups),
        )
        .then_some(info.tcpi_dsack_dups as u64),
        tcp_reord_seen: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_reord_seen),
        )
        .then_some(info.tcpi_reord_seen as u64),
        tcp_snd_wnd: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_snd_wnd),
        )
        .then_some(info.tcpi_snd_wnd as u64),
        tcp_total_rto: field_available::<u16>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_total_rto),
        )
        .then_some(info.tcpi_total_rto as u64),
        tcp_total_rto_recoveries: field_available::<u16>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_total_rto_recoveries),
        )
        .then_some(info.tcpi_total_rto_recoveries as u64),
        tcp_total_rto_time_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_total_rto_time),
        )
        .then_some(info.tcpi_total_rto_time as u64),
        ..TcpReceiverStats::default()
    }
}

#[cfg(target_os = "linux")]
fn collect_tcp_congestion_algorithm(fd: std::os::fd::RawFd) -> Option<String> {
    let mut algorithm = [0u8; 32];
    let mut algorithm_len = algorithm.len() as libc::socklen_t;
    // SAFETY: getsockopt TCP_CONGESTION writes a NUL-terminated algorithm name
    // into the fixed-size stack buffer. fd is a valid TCP socket and
    // algorithm_len is initialized to the buffer size.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CONGESTION,
            algorithm.as_mut_ptr() as *mut libc::c_void,
            &mut algorithm_len,
        )
    };
    if result != 0 {
        return None;
    }
    let written = (algorithm_len as usize).min(algorithm.len());
    let nul = algorithm[..written]
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(written);
    std::str::from_utf8(&algorithm[..nul])
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[cfg(target_os = "linux")]
fn collect_tcp_stats(socket: &tokio::net::TcpStream) -> io::Result<TcpReceiverStats> {
    use std::os::fd::AsRawFd;

    let fd = socket.as_raw_fd();
    let mut info = LinuxTcpInfo::default();
    let mut info_len = std::mem::size_of::<LinuxTcpInfo>() as libc::socklen_t;
    // SAFETY: getsockopt TCP_INFO fills a LinuxTcpInfo struct. `fd` is a
    // valid socket from tokio. `info` is a stack-allocated default-zeroed
    // struct of the correct size for the TCP_INFO option. `info_len` is
    // correctly initialized to sizeof(LinuxTcpInfo) and may be updated
    // by the kernel to the actual bytes written.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            &mut info as *mut LinuxTcpInfo as *mut libc::c_void,
            &mut info_len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut stats = stats_from_tcp_info(&info, info_len as usize);
    stats.tcp_congestion_algorithm = collect_tcp_congestion_algorithm(fd);
    let mut memory = [0u32; 9];
    let mut memory_len = std::mem::size_of_val(&memory) as libc::socklen_t;
    // SAFETY: getsockopt SO_MEMINFO reads socket memory buffer usage.
    // `fd` is a valid socket. `memory` is a stack-allocated [u32; 9] array
    // correctly sized for the SO_MEMINFO option. `memory_len` is initialized
    // to the array byte size. The raw pointer cast is valid for any aligned
    // byte buffer.
    let memory_result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_MEMINFO,
            memory.as_mut_ptr() as *mut libc::c_void,
            &mut memory_len,
        )
    };
    if memory_result == 0 {
        let fields = memory_len as usize / std::mem::size_of::<u32>();
        if fields > libc::SK_MEMINFO_RMEM_ALLOC as usize {
            stats.tcp_skmem_rmem_alloc = Some(memory[libc::SK_MEMINFO_RMEM_ALLOC as usize] as u64);
        }
        if fields > libc::SK_MEMINFO_RCVBUF as usize {
            stats.tcp_skmem_rmem_max = Some(memory[libc::SK_MEMINFO_RCVBUF as usize] as u64);
        }
        if fields > libc::SK_MEMINFO_WMEM_ALLOC as usize {
            stats.tcp_skmem_wmem_alloc = Some(memory[libc::SK_MEMINFO_WMEM_ALLOC as usize] as u64);
        }
        if fields > libc::SK_MEMINFO_SNDBUF as usize {
            stats.tcp_skmem_wmem_max = Some(memory[libc::SK_MEMINFO_SNDBUF as usize] as u64);
        }
    }

    Ok(stats)
}

#[cfg(target_os = "linux")]
pub fn collect_rtmp_receiver_stats(socket: &tokio::net::TcpStream) -> io::Result<TcpReceiverStats> {
    collect_tcp_stats(socket)
}

#[cfg(target_os = "linux")]
pub fn collect_rtmp_sender_stats(socket: &tokio::net::TcpStream) -> io::Result<TcpReceiverStats> {
    collect_tcp_stats(socket)
}

#[cfg(not(target_os = "linux"))]
pub fn collect_rtmp_receiver_stats(
    _socket: &tokio::net::TcpStream,
) -> io::Result<TcpReceiverStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "receiver TCP statistics require Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn collect_rtmp_sender_stats(_socket: &tokio::net::TcpStream) -> io::Result<TcpReceiverStats> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sender TCP statistics require Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn converts_receiver_side_tcp_info_fields() {
        let info = LinuxTcpInfo {
            tcpi_rtt: 12_000,
            tcpi_rttvar: 2_000,
            tcpi_snd_mss: 1448,
            tcpi_unacked: 4,
            tcpi_sacked: 1,
            tcpi_lost: 2,
            tcpi_retrans: 1,
            tcpi_last_data_sent: 24,
            tcpi_last_data_recv: 42,
            tcpi_pmtu: 1500,
            tcpi_rcv_rtt: 8_500,
            tcpi_rcv_space: 65_536,
            tcpi_snd_ssthresh: 128,
            tcpi_snd_cwnd: 10,
            tcpi_advmss: 1448,
            tcpi_reordering: 3,
            tcpi_pacing_rate: 9_000_000,
            tcpi_max_pacing_rate: 10_000_000,
            tcpi_bytes_acked: 7_600_000,
            tcpi_bytes_received: 1_234_567,
            tcpi_segs_out: 500,
            tcpi_notsent_bytes: 2048,
            tcpi_data_segs_out: 480,
            tcpi_delivery_rate: 8_000_000,
            tcpi_busy_time: 3_000,
            tcpi_rwnd_limited: 4_000,
            tcpi_sndbuf_limited: 5_000,
            tcpi_delivered: 490,
            tcpi_delivered_ce: 2,
            tcpi_bytes_sent: 7_654_321,
            tcpi_bytes_retrans: 12_345,
            tcpi_dsack_dups: 6,
            tcpi_reord_seen: 7,
            tcpi_rcv_ooopack: 15,
            tcpi_snd_wnd: 65_000,
            tcpi_total_retrans: 3,
            tcpi_total_rto: 4,
            tcpi_total_rto_recoveries: 5,
            tcpi_total_rto_time: 600,
            ..LinuxTcpInfo::default()
        };

        let stats = stats_from_tcp_info(&info, std::mem::size_of::<LinuxTcpInfo>());
        assert_eq!(stats.tcp_rtt_ms, Some(12.0));
        assert_eq!(stats.tcp_congestion_algorithm, None);
        assert_eq!(stats.tcp_rtt_var_ms, Some(2.0));
        assert_eq!(stats.tcp_bytes_received, Some(1_234_567));
        assert_eq!(stats.tcp_bytes_sent, Some(7_654_321));
        assert_eq!(stats.tcp_bytes_acked, Some(7_600_000));
        assert_eq!(stats.tcp_bytes_retrans, Some(12_345));
        assert_eq!(stats.tcp_last_rcv_ms, Some(42));
        assert_eq!(stats.tcp_last_snd_ms, Some(24));
        assert_eq!(stats.tcp_rcv_rtt_ms, Some(8.5));
        assert_eq!(stats.tcp_rcv_space, Some(65_536));
        assert_eq!(stats.tcp_rcv_ooopack, Some(15));
        assert_eq!(stats.tcp_snd_mss, Some(1448));
        assert_eq!(stats.tcp_pmtu, Some(1500));
        assert_eq!(stats.tcp_unacked, Some(4));
        assert_eq!(stats.tcp_sacked, Some(1));
        assert_eq!(stats.tcp_lost, Some(2));
        assert_eq!(stats.tcp_retrans, Some(1));
        assert_eq!(stats.tcp_snd_cwnd, Some(10));
        assert_eq!(stats.tcp_snd_ssthresh, Some(128));
        assert_eq!(stats.tcp_advmss, Some(1448));
        assert_eq!(stats.tcp_reordering, Some(3));
        assert_eq!(stats.tcp_notsent_bytes, Some(2048));
        assert_eq!(stats.tcp_total_retrans, Some(3));
        assert_eq!(stats.tcp_pacing_rate_bps, Some(9_000_000));
        assert_eq!(stats.tcp_max_pacing_rate_bps, Some(10_000_000));
        assert_eq!(stats.tcp_delivery_rate_bps, Some(8_000_000));
        assert_eq!(stats.tcp_segs_out, Some(500));
        assert_eq!(stats.tcp_data_segs_out, Some(480));
        assert_eq!(stats.tcp_delivered, Some(490));
        assert_eq!(stats.tcp_delivered_ce, Some(2));
        assert_eq!(stats.tcp_busy_time_ms, Some(3));
        assert_eq!(stats.tcp_rwnd_limited_ms, Some(4));
        assert_eq!(stats.tcp_sndbuf_limited_ms, Some(5));
        assert_eq!(stats.tcp_dsack_dups, Some(6));
        assert_eq!(stats.tcp_reord_seen, Some(7));
        assert_eq!(stats.tcp_snd_wnd, Some(65_000));
        assert_eq!(stats.tcp_total_rto, Some(4));
        assert_eq!(stats.tcp_total_rto_recoveries, Some(5));
        assert_eq!(stats.tcp_total_rto_time_ms, Some(600));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn omits_fields_not_returned_by_an_older_kernel() {
        let info = LinuxTcpInfo {
            tcpi_rtt: 12_000,
            tcpi_bytes_received: 1_234_567,
            ..LinuxTcpInfo::default()
        };
        let returned_len = std::mem::offset_of!(LinuxTcpInfo, tcpi_bytes_received);
        let stats = stats_from_tcp_info(&info, returned_len);

        assert_eq!(stats.tcp_rtt_ms, Some(12.0));
        assert_eq!(stats.tcp_bytes_received, None);
        assert_eq!(stats.tcp_rcv_ooopack, None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reads_stats_from_an_owned_tcp_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(b"receiver-side-stats").await.unwrap();
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let mut payload = [0u8; 19];
        server.read_exact(&mut payload).await.unwrap();
        client.await.unwrap();

        let stats = collect_rtmp_receiver_stats(&server).unwrap();
        assert!(stats.tcp_congestion_algorithm.is_some());
        assert!(stats.tcp_rtt_ms.is_some());
        assert!(stats.tcp_bytes_received.unwrap_or(0) >= payload.len() as u64);
        assert!(stats.tcp_skmem_rmem_max.unwrap_or(0) > 0);
        assert!(stats.tcp_skmem_wmem_max.unwrap_or(0) > 0);
    }
}
