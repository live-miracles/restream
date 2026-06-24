//! Native Linux receiver-side TCP statistics for RTMP publishers.
//!
//! The RTMP server owns the accepted socket, so it can read `TCP_INFO` and
//! `SO_MEMINFO` directly without spawning `ss` or matching address strings.

use std::io;

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
        tcp_last_rcv_ms: field_available::<u32>(
            returned_len,
            std::mem::offset_of!(LinuxTcpInfo, tcpi_last_data_recv),
        )
        .then_some(info.tcpi_last_data_recv as u64),
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
        ..TcpReceiverStats::default()
    }
}

#[cfg(target_os = "linux")]
pub fn collect_rtmp_receiver_stats(socket: &tokio::net::TcpStream) -> io::Result<TcpReceiverStats> {
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
    }

    Ok(stats)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn converts_receiver_side_tcp_info_fields() {
        let info = LinuxTcpInfo {
            tcpi_rtt: 12_000,
            tcpi_rttvar: 2_000,
            tcpi_last_data_recv: 42,
            tcpi_rcv_rtt: 8_500,
            tcpi_rcv_space: 65_536,
            tcpi_bytes_received: 1_234_567,
            tcpi_rcv_ooopack: 15,
            ..LinuxTcpInfo::default()
        };

        let stats = stats_from_tcp_info(&info, std::mem::size_of::<LinuxTcpInfo>());
        assert_eq!(stats.tcp_rtt_ms, Some(12.0));
        assert_eq!(stats.tcp_rtt_var_ms, Some(2.0));
        assert_eq!(stats.tcp_bytes_received, Some(1_234_567));
        assert_eq!(stats.tcp_last_rcv_ms, Some(42));
        assert_eq!(stats.tcp_rcv_rtt_ms, Some(8.5));
        assert_eq!(stats.tcp_rcv_space, Some(65_536));
        assert_eq!(stats.tcp_rcv_ooopack, Some(15));
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
        assert!(stats.tcp_rtt_ms.is_some());
        assert!(stats.tcp_bytes_received.unwrap_or(0) >= payload.len() as u64);
        assert!(stats.tcp_skmem_rmem_max.unwrap_or(0) > 0);
    }
}
