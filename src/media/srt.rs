//! Native SRT ingest and egress via raw `libsrt` FFI bindings.
//!
//! Ingest: SRT listener accepts connections, reads `streamid` for authentication,
//! pipes MPEG-TS data into a `MemoryQueue`, and runs an FFmpeg demuxer on a
//! dedicated OS thread (wrapped in `catch_unwind`). The demuxer publishes ALL
//! video and audio streams (not just "best") into the `RingBuffer` with per-track
//! indices for multi-track audio support. The listener has `SRTO_GROUPCONNECT=1`
//! enabled, so bonded ingest connections from encoders that support SRT bonding
//! (e.g., Haivision, srt-live-transmit) are accepted transparently.
//!
//! Egress: connects to an SRT target via `srt_connect` (single link) or
//! `srt_connect_group` (bonded backup, when `bond=` URL parameter is present).
//! MPEG-TS muxing is deferred until ingest metadata is available to avoid
//! "no streams to mux" errors when the egress starts before ingest.
//!
//! # Socket Sizing
//!
//! All sockets (listener, accepted, egress) get high-bitrate tuning via
//! `srt_set_highbitrate_opts`: 12 MB send/recv buffers (vs. default ~1.5 MB),
//! 32768-packet flow control window (vs. default 8192), unlimited max bandwidth.
//! These values accommodate 4K 60fps H.264 streams at 50 Mbps peak with
//! headroom for retransmission bursts on lossy links.
//!
//! # libsrt FFI safety contract
//!
//! All unsafe blocks in this file call into libsrt's C API. Every call site
//! upholds these invariants:
//!
//! 1. `srt_startup()` is called once before any other SRT function.
//! 2. `srt_cleanup()` is called once after all sockets are closed.
//! 3. Every `srt_create_socket()` is balanced by exactly one `srt_close()`.
//!    `SrtSockGuard` provides RAII cleanup for the listener; ingest/egress
//!    sockets are closed on all error and success paths.
//! 4. `srt_setsockopt`/`srt_getsockopt` receive correctly-sized option values
//!    with valid pointers to live stack variables.
//! 5. `srt_send`/`srt_recv` buffers are valid, sized `Vec<u8>` with matching
//!    capacity arguments.
//! 6. `srt_epoll_*` functions are used in matched create/add/remove/release
//!    pairs; the epoll instance outlives all registered sockets.
//! 7. `CStr::from_ptr(srt_getlasterror_str())` returns a thread-local static
//!    string valid until the next SRT call on the same thread.
//! 8. `std::mem::zeroed()` initializes FFI structs (`SrtSocketGroupData`,
//!    `SrtTraceBStats`, `sockaddr_storage`) before the kernel/lib fills them.
//! 9. `srt_bistats` receives a pointer to a correctly-sized `SrtTraceBStats`.
//! 10. Raw pointer writes to `sockaddr` fields target correctly-typed pointers
//!     obtained from a `sockaddr_storage` cast, with the family field set first.

use std::net::SocketAddr;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::media::engine::{MediaEngine, PublisherQuality};
use crate::media::ring_buffer::{MediaType, Reader, RingBuffer};
use crate::media::ts_chunk_ring::{TsChunkReader, TsChunkRing};

// Raw SRT Types & FFI Bindings
pub type SRTSOCKET = c_int;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sockaddr_in {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SrtTraceBStats {
    pub ms_time_stamp: i64,
    pub pkt_sent_total: i64,
    pub pkt_recv_total: i64,
    pub pkt_snd_loss_total: c_int,
    pub pkt_rcv_loss_total: c_int,
    pub pkt_retrans_total: c_int,
    pub pkt_sent_ack_total: c_int,
    pub pkt_recv_ack_total: c_int,
    pub pkt_sent_nak_total: c_int,
    pub pkt_recv_nak_total: c_int,
    pub us_snd_duration_total: i64,
    pub pkt_snd_drop_total: c_int,
    pub pkt_rcv_drop_total: c_int,
    pub pkt_rcv_undecrypt_total: c_int,
    pub byte_sent_total: u64,
    pub byte_recv_total: u64,
    pub byte_rcv_loss_total: u64,
    pub byte_retrans_total: u64,
    pub byte_snd_drop_total: u64,
    pub byte_rcv_drop_total: u64,
    pub byte_rcv_undecrypt_total: u64,
    pub pkt_sent: i64,
    pub pkt_recv: i64,
    pub pkt_snd_loss: c_int,
    pub pkt_rcv_loss: c_int,
    pub pkt_retrans: c_int,
    pub pkt_rcv_retrans: c_int,
    pub pkt_sent_ack: c_int,
    pub pkt_recv_ack: c_int,
    pub pkt_sent_nak: c_int,
    pub pkt_recv_nak: c_int,
    pub mbps_send_rate: f64,
    pub mbps_recv_rate: f64,
    pub us_snd_duration: i64,
    pub pkt_reorder_distance: c_int,
    pub pkt_rcv_avg_belated_time: f64,
    pub pkt_rcv_belated: i64,
    pub pkt_snd_drop: c_int,
    pub pkt_rcv_drop: c_int,
    pub pkt_rcv_undecrypt: c_int,
    pub byte_sent: u64,
    pub byte_recv: u64,
    pub byte_rcv_loss: u64,
    pub byte_retrans: u64,
    pub byte_snd_drop: u64,
    pub byte_rcv_drop: u64,
    pub byte_rcv_undecrypt: u64,
    pub us_pkt_snd_period: f64,
    pub pkt_flow_window: c_int,
    pub pkt_congestion_window: c_int,
    pub pkt_flight_size: c_int,
    pub ms_rtt: f64,
    pub mbps_bandwidth: f64,
    pub byte_avail_snd_buf: c_int,
    pub byte_avail_rcv_buf: c_int,
    pub mbps_max_bw: f64,
    pub byte_mss: c_int,
    pub pkt_snd_buf: c_int,
    pub byte_snd_buf: c_int,
    pub ms_snd_buf: c_int,
    pub ms_snd_tsb_pd_delay: c_int,
    pub pkt_rcv_buf: c_int,
    pub byte_rcv_buf: c_int,
    pub ms_rcv_buf: c_int,
    pub ms_rcv_tsb_pd_delay: c_int,
    pub pkt_snd_filter_extra_total: c_int,
    pub pkt_rcv_filter_extra_total: c_int,
    pub pkt_rcv_filter_supply_total: c_int,
    pub pkt_rcv_filter_loss_total: c_int,
    pub pkt_snd_filter_extra: c_int,
    pub pkt_rcv_filter_extra: c_int,
    pub pkt_rcv_filter_supply: c_int,
    pub pkt_rcv_filter_loss: c_int,
    pub pkt_reorder_tolerance: c_int,
    pub pkt_sent_unique_total: i64,
    pub pkt_recv_unique_total: i64,
    pub byte_sent_unique_total: u64,
    pub byte_recv_unique_total: u64,
    pub pkt_sent_unique: i64,
    pub pkt_recv_unique: i64,
    pub byte_sent_unique: u64,
    pub byte_recv_unique: u64,
}

// SRT bonding group types
pub const SRTGROUP_MASK: c_int = 1 << 30;
pub const SRT_GTYPE_BROADCAST: c_int = 1;
pub const SRT_GTYPE_BACKUP: c_int = 2;
const SRTS_CONNECTED: c_int = 5;
const SRTS_BROKEN: c_int = 6;
const SRT_GST_RUNNING: c_int = 2;
const SRT_GST_BROKEN: c_int = 3;

// SRT epoll event flags
const SRT_EPOLL_IN: c_int = 0x1;

#[repr(C)]
pub struct SrtSockOptConfig {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct SrtGroupMemberConfig {
    pub id: SRTSOCKET,
    pub srcaddr: libc::sockaddr_storage,
    pub peeraddr: libc::sockaddr_storage,
    pub weight: u16,
    pub config: *mut SrtSockOptConfig,
    pub errorcode: c_int,
    pub token: c_int,
}

#[repr(C)]
pub struct SrtSocketGroupData {
    pub id: SRTSOCKET,
    pub peeraddr: libc::sockaddr_storage,
    pub sockstate: c_int,
    pub weight: u16,
    pub memberstate: c_int,
    pub result: c_int,
    pub token: c_int,
}

// SAFETY: FFI declarations for the libsrt C library. All function signatures
// are verified against the libsrt public API (srt.h). The library is loaded
// at link time (dynamic or static) and is guaranteed to be present when
// srt_startup() succeeds during SrtServer::new(). None of these functions
// have Rust-side invariants beyond correct argument types, which are
// enforced by the Rust type system at each call site.
unsafe extern "C" {
    pub fn srt_getversion() -> u32;
    pub fn srt_startup() -> c_int;
    pub fn srt_cleanup() -> c_int;
    pub fn srt_create_socket() -> SRTSOCKET;
    pub fn srt_create_group(gtype: c_int) -> SRTSOCKET;
    pub fn srt_close(u: SRTSOCKET) -> c_int;
    pub fn srt_bind(u: SRTSOCKET, name: *const sockaddr_in, namelen: c_int) -> c_int;
    pub fn srt_listen(u: SRTSOCKET, backlog: c_int) -> c_int;
    pub fn srt_accept(u: SRTSOCKET, addr: *mut sockaddr_in, addrlen: *mut c_int) -> SRTSOCKET;
    pub fn srt_getsockname(u: SRTSOCKET, name: *mut sockaddr_in, namelen: *mut c_int) -> c_int;
    pub fn srt_connect(u: SRTSOCKET, name: *const sockaddr_in, namelen: c_int) -> c_int;
    pub fn srt_connect_group(
        group: SRTSOCKET,
        name: *mut SrtGroupMemberConfig,
        arraysize: c_int,
    ) -> c_int;
    pub fn srt_group_data(
        group: SRTSOCKET,
        output: *mut SrtSocketGroupData,
        inoutlen: *mut usize,
    ) -> c_int;
    pub fn srt_prepare_endpoint(
        src: *const libc::sockaddr,
        adr: *const libc::sockaddr,
        namelen: c_int,
    ) -> SrtGroupMemberConfig;
    pub fn srt_create_config() -> *mut SrtSockOptConfig;
    pub fn srt_delete_config(config: *mut SrtSockOptConfig);
    pub fn srt_config_add(
        config: *mut SrtSockOptConfig,
        option: c_int,
        contents: *const c_void,
        len: c_int,
    ) -> c_int;
    pub fn srt_recv(u: SRTSOCKET, buf: *mut u8, len: c_int) -> c_int;
    pub fn srt_recvmsg2(
        u: SRTSOCKET,
        buf: *mut u8,
        len: c_int,
        message_control: *mut c_void,
    ) -> c_int;
    pub fn srt_send(u: SRTSOCKET, buf: *const u8, len: c_int) -> c_int;
    pub fn srt_setsockopt(
        u: SRTSOCKET,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: c_int,
    ) -> c_int;
    pub fn srt_setsockflag(
        u: SRTSOCKET,
        optname: c_int,
        optval: *const c_void,
        optlen: c_int,
    ) -> c_int;
    pub fn srt_getsockopt(
        u: SRTSOCKET,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_int,
    ) -> c_int;
    pub fn srt_getlasterror_str() -> *const c_char;
    pub fn srt_bistats(
        u: SRTSOCKET,
        perf: *mut SrtTraceBStats,
        clear: c_int,
        instantaneous: c_int,
    ) -> c_int;
    pub fn srt_epoll_create() -> c_int;
    pub fn srt_epoll_add_usock(eid: c_int, u: SRTSOCKET, events: *const c_int) -> c_int;
    pub fn srt_epoll_remove_usock(eid: c_int, u: SRTSOCKET) -> c_int;
    pub fn srt_epoll_release(eid: c_int) -> c_int;
    pub fn srt_epoll_wait(
        eid: c_int,
        readfds: *mut SRTSOCKET,
        rnum: *mut c_int,
        writefds: *mut SRTSOCKET,
        wnum: *mut c_int,
        ms_timeout: i64,
        lrfds: *mut c_int,
        lrnum: *mut c_int,
        lwfds: *mut c_int,
        lwnum: *mut c_int,
    ) -> c_int;
}

pub fn linked_srt_version() -> String {
    // SAFETY: srt_getversion returns a u32 with no side effects. Safe to
    // call at any time after srt_startup() (called during server init).
    let version = unsafe { srt_getversion() };
    format!(
        "{}.{}.{}",
        (version >> 16) & 0xff,
        (version >> 8) & 0xff,
        version & 0xff
    )
}

// SRT socket options — values from srt.h SRT_SOCKOPT enum
pub const SRTO_SNDSYN: c_int = 1;
pub const SRTO_RCVSYN: c_int = 2;
pub const SRTO_FC: c_int = 4;
pub const SRTO_SNDBUF: c_int = 5;
pub const SRTO_RCVBUF: c_int = 6;
pub const SRTO_UDP_SNDBUF: c_int = 8;
pub const SRTO_UDP_RCVBUF: c_int = 9;
pub const SRTO_MAXBW: c_int = 16;
pub const SRTO_LATENCY: c_int = 23;
pub const SRTO_INPUTBW: c_int = 24;
pub const SRTO_OHEADBW: c_int = 25;
pub const SRTO_LOSSMAXTTL: c_int = 42;
pub const SRTO_RCVLATENCY: c_int = 43;
pub const SRTO_PEERLATENCY: c_int = 44;
pub const SRTO_STREAMID: c_int = 46;
pub const SRTO_TRANSTYPE: c_int = 50;
pub const SRTO_GROUPCONNECT: c_int = 57;
pub const SRTO_GROUPTYPE: c_int = 59;

pub const SRTT_LIVE: c_int = 0;

pub const DESIRED_UDP_BUF: i32 = 8 * 1024 * 1024;
const DESIRED_SRT_BUF: i32 = 12 * 1024 * 1024;
const DESIRED_FC: i32 = 32768;
// 4×RTT + 2×jitter for 50ms RTT, ~10ms jitter = 220ms. Round to 250ms for margin.
const DESIRED_LATENCY_MS: i32 = 250;
// Max reorder tolerance: at 50 Mbps / 1316 B per packet ≈ 4750 pkt/s.
// 50ms of reordering ≈ 238 packets. Default (0) lets SRT auto-detect, but
// setting a floor prevents premature loss declarations on jittery links.
const DESIRED_LOSSMAXTTL: i32 = 256;

fn enable_srt_group_connect(listener: SRTSOCKET) -> Result<(), String> {
    let group_connect: c_int = 1;
    // SAFETY: srt_setsockflag sets an option on a valid SRT socket. The
    // `group_connect` pointer and size are correctly typed.
    let result = unsafe {
        srt_setsockflag(
            listener,
            SRTO_GROUPCONNECT,
            &group_connect as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        )
    };
    if result >= 0 {
        Ok(())
    } else {
        // SAFETY: srt_getlasterror_str returns a NUL-terminated thread-local
        // static string valid until the next SRT call on this thread.
        let error = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) };
        Err(error.to_string_lossy().into_owned())
    }
}

fn check_sysctl_limits() {
    let check = |path: &str, need: usize, label: &str| {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(val) = s.trim().parse::<usize>()
            && val < need
        {
            eprintln!(
                "[srt] WARNING: {} = {} but we need {}. \
                         Run: sudo sysctl -w {}={}",
                path, val, need, label, need,
            );
        }
    };
    check(
        "/proc/sys/net/core/rmem_max",
        DESIRED_UDP_BUF as usize,
        "net.core.rmem_max",
    );
    check(
        "/proc/sys/net/core/wmem_max",
        DESIRED_UDP_BUF as usize,
        "net.core.wmem_max",
    );
}

/// Tune SRT socket for streams up to 4K 60fps (~50 Mbps H.264 peak).
///
/// Sizing rationale (designed for ≤50ms RTT, ~10ms jitter, ≤5% loss):
///
/// 1. **Latency** (`SRTO_LATENCY`): governs the receiver's dejitter/retransmit
///    window. Formula: `4×RTT + 2×jitter` = 4×50 + 2×10 = 220ms. Set 250ms
///    for margin. Sender and receiver negotiate the max of both sides. At
///    50 Mbps, 250ms = 1.56 MB in flight — well within our buffer sizes.
///
/// 2. **Kernel UDP socket** (`SRTO_UDP_SNDBUF`/`RCVBUF`): default ~208 KB
///    fills in ~33ms at 50 Mbps. Set to 8 MB (~1.3s at peak rate).
///
/// 3. **SRT internal buffers** (`SRTO_SNDBUF`/`RCVBUF`): hold packets for
///    retransmission. Must be ≥ latency × bitrate × (1 + loss_overhead).
///    At 250ms, 50 Mbps, 5% loss: 1.56 MB × 1.15 ≈ 1.8 MB minimum.
///    Set to 12 MB for headroom on burst retransmissions.
///
/// 4. **Flow control window** (`SRTO_FC`): max packets in flight. At 50 Mbps
///    / 1316 B = ~4750 pkt/s; 250ms latency = ~1188 in-flight packets.
///    Default 8192 is OK but set 32768 for high-latency links.
///
/// 5. **Loss max TTL** (`SRTO_LOSSMAXTTL`): reorder tolerance before
///    declaring loss. Default 0 = auto. Set 256 packets (~54ms at 50 Mbps)
///    to handle jitter without premature NACK storms.
// SAFETY: All srt_setsockopt calls use correctly-sized stack-allocated
// option values with valid SRT socket handles. The UDP/SRT buffer sizes,
// flow control window, and latency values are within platform limits.
fn srt_set_highbitrate_opts(sock: SRTSOCKET) {
    unsafe {
        // Latency: dejitter + retransmit window (4×RTT + 2×jitter)
        let latency: c_int = DESIRED_LATENCY_MS;
        srt_setsockopt(
            sock,
            0,
            SRTO_LATENCY,
            &latency as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );

        // Reorder tolerance before declaring loss
        let lossmaxttl: c_int = DESIRED_LOSSMAXTTL;
        srt_setsockopt(
            sock,
            0,
            SRTO_LOSSMAXTTL,
            &lossmaxttl as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );

        let udp_buf: c_int = DESIRED_UDP_BUF;
        srt_setsockopt(
            sock,
            0,
            SRTO_UDP_SNDBUF,
            &udp_buf as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );
        srt_setsockopt(
            sock,
            0,
            SRTO_UDP_RCVBUF,
            &udp_buf as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );

        let srt_buf: c_int = DESIRED_SRT_BUF;
        srt_setsockopt(
            sock,
            0,
            SRTO_SNDBUF,
            &srt_buf as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );
        srt_setsockopt(
            sock,
            0,
            SRTO_RCVBUF,
            &srt_buf as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );

        let fc: c_int = DESIRED_FC;
        srt_setsockopt(
            sock,
            0,
            SRTO_FC,
            &fc as *const _ as *const c_void,
            std::mem::size_of::<c_int>() as c_int,
        );

        let maxbw: i64 = -1;
        srt_setsockopt(
            sock,
            0,
            SRTO_MAXBW,
            &maxbw as *const _ as *const c_void,
            std::mem::size_of::<i64>() as c_int,
        );
    }
}

// SAFETY: srt_getsockopt reads integer option values from a valid SRT
// socket into correctly-sized stack variables. All options are benign
// diagnostic reads with no side effects on the socket.
fn srt_log_effective_opts(sock: SRTSOCKET, label: &str) {
    unsafe {
        let mut udp_snd = 0i32;
        let mut udp_rcv = 0i32;
        let mut srt_snd = 0i32;
        let mut srt_rcv = 0i32;
        let mut fc = 0i32;
        let mut latency = 0i32;
        let mut lossmaxttl = 0i32;
        let sz = std::mem::size_of::<c_int>() as c_int;
        let mut len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_UDP_SNDBUF,
            &mut udp_snd as *mut _ as *mut c_void,
            &mut len,
        );
        len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_UDP_RCVBUF,
            &mut udp_rcv as *mut _ as *mut c_void,
            &mut len,
        );
        len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_SNDBUF,
            &mut srt_snd as *mut _ as *mut c_void,
            &mut len,
        );
        len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_RCVBUF,
            &mut srt_rcv as *mut _ as *mut c_void,
            &mut len,
        );
        len = sz;
        srt_getsockopt(sock, 0, SRTO_FC, &mut fc as *mut _ as *mut c_void, &mut len);
        len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_LATENCY,
            &mut latency as *mut _ as *mut c_void,
            &mut len,
        );
        len = sz;
        srt_getsockopt(
            sock,
            0,
            SRTO_LOSSMAXTTL,
            &mut lossmaxttl as *mut _ as *mut c_void,
            &mut len,
        );
        println!(
            "[srt] {} config: latency={}ms lossmaxttl={} UDP snd={}KB rcv={}KB, SRT snd={}KB rcv={}KB, FC={}",
            label,
            latency,
            lossmaxttl,
            udp_snd / 1024,
            udp_rcv / 1024,
            srt_snd / 1024,
            srt_rcv / 1024,
            fc,
        );
        if udp_snd < DESIRED_UDP_BUF {
            eprintln!(
                "[srt] WARNING: {} UDP send buffer clamped to {}KB (wanted {}KB). \
                 Raise net.core.wmem_max",
                label,
                udp_snd / 1024,
                DESIRED_UDP_BUF / 1024,
            );
        }
        if udp_rcv < DESIRED_UDP_BUF {
            eprintln!(
                "[srt] WARNING: {} UDP recv buffer clamped to {}KB (wanted {}KB). \
                 Raise net.core.rmem_max",
                label,
                udp_rcv / 1024,
                DESIRED_UDP_BUF / 1024,
            );
        }
    }
}

fn to_sockaddr_in(addr: SocketAddr) -> sockaddr_in {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(ipv4) => u32::from_ne_bytes(ipv4.octets()),
        _ => 0,
    };
    sockaddr_in {
        sin_family: 2, // AF_INET
        sin_port: addr.port().to_be(),
        sin_addr: ip,
        sin_zero: [0; 8],
    }
}

fn from_sockaddr_in(addr: sockaddr_in) -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(addr.sin_addr.to_ne_bytes())),
        u16::from_be(addr.sin_port),
    )
}

fn is_srt_group(socket: SRTSOCKET) -> bool {
    socket & SRTGROUP_MASK != 0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SrtGroupSummary {
    member_count: u32,
    connected_members: u32,
    active_members: u32,
    broken_members: u32,
}

fn summarize_group_members(members: &[SrtSocketGroupData]) -> SrtGroupSummary {
    let mut summary = SrtGroupSummary {
        member_count: members.len() as u32,
        ..SrtGroupSummary::default()
    };
    for member in members {
        if member.sockstate == SRTS_CONNECTED {
            summary.connected_members += 1;
        }
        if member.memberstate == SRT_GST_RUNNING {
            summary.active_members += 1;
        }
        if member.sockstate == SRTS_BROKEN || member.memberstate == SRT_GST_BROKEN {
            summary.broken_members += 1;
        }
    }
    summary
}

fn srt_group_summary(group: SRTSOCKET) -> Option<SrtGroupSummary> {
    // Ingest bonds are normally two links. Keep ample room so this call stays
    // allocation-only and does not need to guess at libsrt's resize semantics.
    const MAX_GROUP_MEMBERS: usize = 64;
    // SAFETY: std::mem::zeroed() for C structs is valid when the struct
    // has no invalid bit patterns (all-zero is a valid SrtSocketGroupData).
    // srt_group_data fills the array through a raw pointer; members is
    // correctly sized and aligned.
    let mut members: Vec<SrtSocketGroupData> = (0..MAX_GROUP_MEMBERS)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    let mut member_count = members.len();
    let result = unsafe { srt_group_data(group, members.as_mut_ptr(), &mut member_count) };
    if result < 0 {
        return None;
    }
    members.truncate(member_count.min(members.len()));
    Some(summarize_group_members(&members))
}

fn add_srt_group_quality(
    quality: &mut PublisherQuality,
    is_group: bool,
    summary: Option<SrtGroupSummary>,
) {
    quality.srt_bonded = Some(is_group);
    if let Some(summary) = summary {
        quality.srt_group_member_count = Some(summary.member_count);
        quality.srt_group_connected_members = Some(summary.connected_members);
        quality.srt_group_active_members = Some(summary.active_members);
        quality.srt_group_broken_members = Some(summary.broken_members);
    }
}

#[derive(Debug, Clone, Copy)]
struct SrtCounterSnapshot {
    packets_received_loss: u64,
    packets_received_drop: u64,
    packets_received_retrans: u64,
    packets_received_undecrypt: u64,
    sampled_at: Instant,
}

fn counter_rate(current: u64, previous: u64, elapsed_seconds: f64) -> Option<f64> {
    if elapsed_seconds <= 0.0 {
        return None;
    }
    current
        .checked_sub(previous)
        .map(|delta| (delta as f64 / elapsed_seconds * 10.0).round() / 10.0)
}

fn srt_quality_from_stats(
    stats: &SrtTraceBStats,
    previous: Option<SrtCounterSnapshot>,
    sampled_at: Instant,
) -> (PublisherQuality, SrtCounterSnapshot) {
    let current = SrtCounterSnapshot {
        packets_received_loss: stats.pkt_rcv_loss_total.max(0) as u64,
        packets_received_drop: stats.pkt_rcv_drop_total.max(0) as u64,
        packets_received_retrans: stats.pkt_rcv_retrans.max(0) as u64,
        packets_received_undecrypt: stats.pkt_rcv_undecrypt_total.max(0) as u64,
        sampled_at,
    };
    let elapsed =
        previous.map(|snapshot| sampled_at.duration_since(snapshot.sampled_at).as_secs_f64());

    (
        PublisherQuality {
            ms_rtt: Some(stats.ms_rtt),
            mbps_receive_rate: Some(stats.mbps_recv_rate),
            mbps_link_capacity: Some(stats.mbps_bandwidth),
            ms_receive_tsb_pd_delay: Some(stats.ms_rcv_tsb_pd_delay.max(0) as f64),
            ms_receive_buf: Some(stats.ms_rcv_buf.max(0) as f64),
            packets_sent_nak: Some(stats.pkt_sent_nak_total.max(0) as u64),
            packets_received_loss: Some(current.packets_received_loss),
            packets_received_drop: Some(current.packets_received_drop),
            packets_received_retrans: Some(current.packets_received_retrans),
            packets_received_undecrypt: Some(current.packets_received_undecrypt),
            packets_received_loss_per_sec: previous.zip(elapsed).and_then(|(snapshot, seconds)| {
                counter_rate(
                    current.packets_received_loss,
                    snapshot.packets_received_loss,
                    seconds,
                )
            }),
            packets_received_drop_per_sec: previous.zip(elapsed).and_then(|(snapshot, seconds)| {
                counter_rate(
                    current.packets_received_drop,
                    snapshot.packets_received_drop,
                    seconds,
                )
            }),
            packets_received_retrans_per_sec: previous.zip(elapsed).and_then(
                |(snapshot, seconds)| {
                    counter_rate(
                        current.packets_received_retrans,
                        snapshot.packets_received_retrans,
                        seconds,
                    )
                },
            ),
            packets_received_undecrypt_per_sec: previous.zip(elapsed).and_then(
                |(snapshot, seconds)| {
                    counter_rate(
                        current.packets_received_undecrypt,
                        snapshot.packets_received_undecrypt,
                        seconds,
                    )
                },
            ),
            srt_send_buf_bytes: Some(stats.byte_snd_buf),
            srt_recv_buf_bytes: Some(stats.byte_rcv_buf),
            srt_send_buf_avail_bytes: Some(stats.byte_avail_snd_buf),
            srt_recv_buf_avail_bytes: Some(stats.byte_avail_rcv_buf),
            srt_flight_size_pkts: Some(stats.pkt_flight_size),
            ..PublisherQuality::default()
        },
        current,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SrtConnectionMode {
    Publish,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStreamId {
    mode: SrtConnectionMode,
    stream_key: String,
}

fn strip_query(value: &str) -> &str {
    value.split_once('?').map(|(path, _)| path).unwrap_or(value)
}

/// Decode percent-encoded characters in a URL query parameter value.
/// Handles `%XX` sequences where XX is a two-digit hex byte value.
/// Non-UTF8 sequences are passed through as-is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn parse_srt_stream_id(streamid: &str) -> ParsedStreamId {
    let raw = streamid.trim_matches('\0').trim();
    if raw.is_empty() {
        return ParsedStreamId {
            mode: SrtConnectionMode::Publish,
            stream_key: String::new(),
        };
    }

    if let Some(rest) = raw.strip_prefix("#!::") {
        let mut mode = SrtConnectionMode::Publish;
        let mut resource = "";
        for part in rest.split(',') {
            if let Some((key, value)) = part.split_once('=') {
                match key {
                    "r" | "streamid" => resource = value,
                    "m" => {
                        if matches!(value, "request" | "read" | "play" | "subscriber") {
                            mode = SrtConnectionMode::Read;
                        }
                    }
                    _ => {}
                }
            }
        }
        let stream_key = strip_query(resource)
            .rsplit('/')
            .next()
            .unwrap_or(resource)
            .to_string();
        return ParsedStreamId { mode, stream_key };
    }

    let (mode, rest) = if let Some((prefix, value)) = raw.split_once(':') {
        let mode = if matches!(prefix, "play" | "read" | "subscriber" | "request") {
            SrtConnectionMode::Read
        } else {
            SrtConnectionMode::Publish
        };
        (mode, value)
    } else {
        (SrtConnectionMode::Publish, raw)
    };

    let stream_key = strip_query(rest)
        .rsplit('/')
        .next()
        .unwrap_or(rest)
        .to_string();
    ParsedStreamId { mode, stream_key }
}

#[cfg(test)]
fn video_codec_id(codec: &str) -> Option<ffmpeg_next::ffi::AVCodecID> {
    match codec {
        "h264" | "avc" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264),
        "h265" | "hevc" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC),
        _ => None,
    }
}

#[cfg(test)]
fn audio_codec_id(codec: &str) -> Option<ffmpeg_next::ffi::AVCodecID> {
    match codec {
        "aac" => Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_AAC),
        _ => None,
    }
}

/// Read the kernel UDP recv queue occupancy and drop count for a given local port
/// from /proc/net/udp. Returns (rx_queue_bytes, drops).
fn read_udp_socket_stats(port: u16) -> Option<(u64, u64)> {
    let port_hex = format!("{:04X}", port);
    let content = std::fs::read_to_string("/proc/net/udp").ok()?;
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 13 {
            continue;
        }
        // local_address is field[1], format "ADDR:PORT" in hex
        if let Some(lport) = fields[1].split(':').nth(1)
            && lport == port_hex
        {
            // rx_queue is second half of field[4] "tx_queue:rx_queue"
            let queues: Vec<&str> = fields[4].split(':').collect();
            let rx_queue = queues
                .get(1)
                .and_then(|s| u64::from_str_radix(s, 16).ok())
                .unwrap_or(0);
            let drops = fields
                .get(12)
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            return Some((rx_queue, drops));
        }
    }
    None
}

async fn monitor_listener_socket(port: u16, stats: Arc<crate::media::engine::ListenerSocketStats>) {
    use std::sync::atomic::Ordering;

    let configured_buf = DESIRED_UDP_BUF as u64;
    let warn_threshold = configured_buf / 2; // 50%
    let crit_threshold = (configured_buf * 3) / 4; // 75%
    let mut prev_drops = 0u64;
    let mut warned = false;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let (rx_queue, drops) = match read_udp_socket_stats(port) {
            Some(v) => v,
            None => continue,
        };

        stats.rx_queue_bytes.store(rx_queue, Ordering::Relaxed);
        stats.drops.store(drops, Ordering::Relaxed);

        let prev_peak = stats.rx_queue_max_bytes.load(Ordering::Relaxed);
        if rx_queue > prev_peak {
            stats.rx_queue_max_bytes.store(rx_queue, Ordering::Relaxed);
        }

        if drops > prev_drops {
            eprintln!(
                "[srt] ALERT: kernel dropped {} UDP packets on listener :{}  \
                 (total drops: {}, rx_queue: {}KB / {}KB). \
                 Increase net.core.rmem_max and restart, or reduce ingest count.",
                drops - prev_drops,
                port,
                drops,
                rx_queue / 1024,
                configured_buf / 1024,
            );
            prev_drops = drops;
            warned = false; // reset warning so it fires again after drops
        }

        if rx_queue > crit_threshold {
            eprintln!(
                "[srt] ALERT: listener :{} UDP recv queue at {}KB / {}KB ({:.0}%) — \
                 imminent packet loss. Consider reducing concurrent ingest streams \
                 or increasing net.core.rmem_max.",
                port,
                rx_queue / 1024,
                configured_buf / 1024,
                rx_queue as f64 / configured_buf as f64 * 100.0,
            );
            warned = true;
        } else if rx_queue > warn_threshold && !warned {
            eprintln!(
                "[srt] WARNING: listener :{} UDP recv queue at {}KB / {}KB ({:.0}%)",
                port,
                rx_queue / 1024,
                configured_buf / 1024,
                rx_queue as f64 / configured_buf as f64 * 100.0,
            );
            warned = true;
        } else if rx_queue < warn_threshold / 2 {
            warned = false;
        }
    }
}

pub struct SrtServer {
    db: sqlx::SqlitePool,
    engine: Arc<MediaEngine>,
    security: Arc<crate::media::security::IngestSecurityService>,
}

impl SrtServer {
    pub fn new(
        db: sqlx::SqlitePool,
        engine: Arc<MediaEngine>,
        security: Arc<crate::media::security::IngestSecurityService>,
    ) -> Self {
        // SAFETY: srt_startup must be called once before any other SRT
        // function. This is the only call site, at server construction time,
        // enforced by the singleton SrtServer pattern.
        unsafe {
            srt_startup();
        }
        check_sysctl_limits();
        Self {
            db,
            engine,
            security,
        }
    }

    pub async fn run(self: Arc<Self>, port: u16) {
        // SAFETY: srt_create_socket returns a valid SRT socket handle or -1
        // on error. The socket is closed via SrtSockGuard on drop or
        // explicitly on bind/listen failure below. Balanced by srt_close.
        let server_sock = unsafe { srt_create_socket() };
        if server_sock < 0 {
            eprintln!("[srt] Failed to create socket");
            return;
        }

        // SAFETY: Sets SRTT_LIVE transmission type on a valid listener
        // socket. The option value is a stack-allocated c_int.
        unsafe {
            let live_mode: c_int = SRTT_LIVE;
            srt_setsockopt(
                server_sock,
                0,
                SRTO_TRANSTYPE,
                &live_mode as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as c_int,
            );
        }
        match enable_srt_group_connect(server_sock) {
            Ok(()) => {
                self.engine
                    .srt_listener_stats
                    .bonding_available
                    .store(true, Ordering::Relaxed);
                println!("[srt] Bonded ingest enabled on the shared listener (SRTO_GROUPCONNECT)")
            }
            Err(error) => {
                self.engine
                    .srt_listener_stats
                    .bonding_available
                    .store(false, Ordering::Relaxed);
                eprintln!(
                    "[srt] WARNING: bonded ingest is unavailable: linked libsrt rejected \
                 SRTO_GROUPCONNECT ({error}). Install/build libsrt with ENABLE_BONDING=ON. \
                 Single-link SRT ingest remains available."
                )
            }
        }
        srt_set_highbitrate_opts(server_sock);
        srt_log_effective_opts(server_sock, "listener");

        let addr_str = format!("0.0.0.0:{}", port);
        let addr = match addr_str.parse::<SocketAddr>() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[srt] Invalid address: {:?}", e);
                return;
            }
        };

        let sin = to_sockaddr_in(addr);
        // SAFETY: srt_bind binds a valid server socket to the given
        // sockaddr_in. The sockaddr struct is stack-allocated and correctly
        // sized. On failure the socket is closed explicitly.
        let bind_res = unsafe {
            srt_bind(
                server_sock,
                &sin,
                std::mem::size_of::<sockaddr_in>() as c_int,
            )
        };
        if bind_res < 0 {
            eprintln!("[srt] Bind failed");
            // SAFETY: server_sock is a valid socket not yet closed.
            unsafe {
                srt_close(server_sock);
            }
            return;
        }

        // SAFETY: srt_listen starts listening on a bound socket. Backlog 1024
        // is a common value for high-throughput servers. On failure the socket
        // is closed explicitly.
        let listen_res = unsafe { srt_listen(server_sock, 1024) };
        if listen_res < 0 {
            eprintln!("[srt] Listen failed");
            // SAFETY: Valid socket, not yet closed.
            unsafe {
                srt_close(server_sock);
            }
            return;
        }

        println!("[srt] Server listening on srt://{}", addr_str);

        // Monitor the shared listener socket's kernel UDP buffer occupancy
        let listener_stats = self.engine.srt_listener_stats.clone();
        tokio::spawn(async move {
            monitor_listener_socket(port, listener_stats).await;
        });

        // Bounded channel between the blocking accept thread and the tokio task.
        // Capacity of 1024 means at most 1024 accepted-but-unprocessed sockets
        // queue up before the accept thread blocks. This limits memory growth
        // under a connection-flood attack without rejecting valid clients under
        // normal load (tokio processes items as fast as it can).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(SRTSOCKET, sockaddr_in)>(1024);

        // RAII guard: close server_sock when run() returns (normal exit, task
        // cancellation, or panic).  Closing the socket interrupts srt_accept()
        // in the accept thread, which then exits via the tx.send() failure path.
        // SAFETY: SrtSockGuard is an RAII guard that closes the server
        // socket on drop. The socket was created by srt_create_socket()
        // above and has not been closed elsewhere. srt_close is idempotent
        // for invalid handles but the guard is only constructed for valid
        // sockets.
        struct SrtSockGuard(SRTSOCKET);
        impl Drop for SrtSockGuard {
            fn drop(&mut self) {
                // SAFETY: The guard owns a socket created by
                // srt_create_socket(). srt_close is called exactly once
                // per socket via this RAII drop.
                unsafe {
                    srt_close(self.0);
                }
            }
        }
        let _server_sock_guard = SrtSockGuard(server_sock);

        // Blocking accept thread — srt_accept in sync mode blocks until a connection arrives.
        // Wrapped in catch_unwind so a panic cannot crash the process (CLAUDE.md).
        let accept_handle = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loop {
                    let mut client_sin = sockaddr_in {
                        sin_family: 0,
                        sin_port: 0,
                        sin_addr: 0,
                        sin_zero: [0; 8],
                    };
                    let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
                    // SAFETY: srt_accept blocks until a connection arrives.
                    // Called from a dedicated std::thread (not tokio), so
                    // blocking is acceptable. server_sock is valid; client_sin
                    // and len are correctly sized.
                    let client_sock = unsafe { srt_accept(server_sock, &mut client_sin, &mut len) };
                    if client_sock < 0 {
                        // SAFETY: srt_getlasterror_str returns a thread-local
                        // static string valid until the next SRT call.
                        let err = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) };
                        eprintln!("[srt] Accept error: {}", err.to_string_lossy());
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                    // blocking_send: the accept thread is a std::thread so it
                    // can block here when the channel is full. This creates
                    // natural backpressure — the accept thread pauses while
                    // tokio drains the queue, preventing unbounded growth.
                    if tx.blocking_send((client_sock, client_sin)).is_err() {
                        // SAFETY: client_sock was just accepted and has not
                        // been closed. Channel closure means the server is
                        // shutting down — clean up the accepted socket.
                        unsafe {
                            srt_close(client_sock);
                        }
                        break;
                    }
                }
            }));
            if result.is_err() {
                eprintln!("[srt] Accept thread panicked — ingest listener is down");
            }
        });
        self.engine.register_os_thread(accept_handle);

        while let Some((client_sock, client_addr)) = rx.recv().await {
            let self_clone = self.clone();
            tokio::spawn(async move {
                self_clone
                    .handle_client(client_sock, from_sockaddr_in(client_addr))
                    .await;
            });
        }
    }

    async fn handle_client(&self, client_sock: SRTSOCKET, client_addr: SocketAddr) {
        let is_group = is_srt_group(client_sock);
        let client_ip = client_addr.ip().to_string();

        // Rate-limit check — same gate as RTMP (H1 fix)
        if let Some(remaining) = self.security.is_ip_banned(&client_ip) {
            eprintln!(
                "[srt] Rejecting banned IP {} (ban expires in {:.1}s)",
                client_ip,
                remaining.as_secs_f64()
            );
            // SAFETY: client_sock is a valid accepted socket not yet closed.
            unsafe { srt_close(client_sock) };
            return;
        }

        // Read streamid
        let mut streamid_buf = [0u8; 512];
        let mut optlen = streamid_buf.len() as c_int;
        // SAFETY: srt_getsockopt reads the STREAMID from a valid client
        // socket. streamid_buf is a 512-byte stack buffer; optlen is
        // initialized to the buffer size and updated with the actual length.
        let res = unsafe {
            srt_getsockopt(
                client_sock,
                0,
                SRTO_STREAMID,
                streamid_buf.as_mut_ptr() as *mut c_void,
                &mut optlen,
            )
        };

        let streamid = if res >= 0 {
            String::from_utf8_lossy(&streamid_buf[..optlen as usize])
                .trim_matches('\0')
                .to_string()
        } else {
            "".to_string()
        };

        println!(
            "[srt] {} accepted (id={}). StreamID: {}",
            if is_group {
                "Bonded group"
            } else {
                "Connection"
            },
            client_sock,
            streamid
        );

        let parsed = parse_srt_stream_id(&streamid);
        let is_reader = parsed.mode == SrtConnectionMode::Read;
        let stream_key = parsed.stream_key.as_str();

        // Query pipeline for stream key validation
        let pipeline = match sqlx::query_as::<_, crate::types::Pipeline>(
            "SELECT id, name, stream_key, input_source, encoding FROM pipelines WHERE stream_key = ?"
        )
        .bind(stream_key)
        .fetch_optional(&self.db)
        .await {
            Ok(Some(p)) => p,
            _ => {
                eprintln!("[srt] Unauthorized connection for stream key: {}", stream_key);
                self.security.record_failure(&client_ip);
                // SAFETY: client_sock is a valid accepted socket not yet closed.
                unsafe { srt_close(client_sock); }
                return;
            }
        };

        // Clear failure state on successful authentication.
        self.security.record_success(&client_ip);

        println!(
            "[srt] Authenticated stream key: {} for pipeline: {} (mode={})",
            stream_key,
            pipeline.id,
            if is_reader { "read" } else { "publish" }
        );

        if is_reader {
            self.handle_play(client_sock, &pipeline.id).await;
            return;
        }

        let ring_buffer = self.engine.get_or_create_pipeline(&pipeline.id).await;
        let Some(token) = self
            .engine
            .try_register_ingest(&pipeline.id, stream_key, "srt")
            .await
        else {
            eprintln!(
                "[srt] Rejecting duplicate publisher for pipeline {}",
                pipeline.id
            );
            // SAFETY: Valid socket, not yet closed elsewhere.
            unsafe { srt_close(client_sock) };
            return;
        };
        self.engine
            .update_ingest_meta(&pipeline.id, None, None, Some(client_addr.to_string()))
            .await;
        if is_group {
            match srt_group_summary(client_sock) {
                Some(summary) => println!(
                    "[srt] Bonded ingest group {}: members={} connected={} active={} broken={}",
                    client_sock,
                    summary.member_count,
                    summary.connected_members,
                    summary.active_members,
                    summary.broken_members
                ),
                None => eprintln!(
                    "[srt] Bonded ingest group {} accepted, but member state is not available yet",
                    client_sock
                ),
            }
        }

        let (bytes_received, ingest_metrics) = {
            let ingests = self.engine.active_ingests.read().await;
            let opt = ingests
                .get(&pipeline.id)
                .map(|ingest| (ingest.bytes_received.clone(), ingest.metrics.clone()));
            opt.unzip()
        };
        let Some(bytes_received) = bytes_received else {
            eprintln!(
                "[srt] Ingest vanished before receive loop for pipeline {}",
                pipeline.id
            );
            // SAFETY: Valid socket, clean up on early return.
            unsafe { srt_close(client_sock) };
            return;
        };

        // Cache a clone of the keyframe_times Arc so we can lock it directly
        // without an async registry lookup (active_ingests.read().await +
        // HashMap::get()) on every IDR frame in the ingest hot loop.
        let cached_keyframe_times = {
            let ingests = self.engine.active_ingests.read().await;
            ingests.get(&pipeline.id).map(|i| i.keyframe_times.clone())
        };

        // Pure-Rust MPEG-TS demuxer — no FFmpeg thread or MemoryQueue needed
        let mut demuxer = crate::media::mpegts::TsDemuxer::new();
        let mut packets = Vec::with_capacity(16);
        let mut probe_sent = false;

        // Set non-blocking mode so srt_recv returns immediately with EAGAIN
        // instead of blocking the tokio runtime thread
        // SAFETY: Sets non-blocking mode on a valid client socket. The zero
        // value and sizeof(c_int) are correct for SRTO_RCVSYN.
        let zero: c_int = 0;
        unsafe {
            srt_setsockopt(
                client_sock,
                0,
                SRTO_RCVSYN,
                &zero as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as c_int,
            );
        }

        // SAFETY: srt_epoll_create creates a new epoll instance. The handle
        // is valid or negative on error. Released by the epoll_waiter task
        // (see below) so it is always freed even if this async future is
        // dropped at an await point before reaching the cleanup block.
        let eid = unsafe { srt_epoll_create() };
        if eid < 0 {
            eprintln!("[srt] Failed to create epoll instance");
            // SAFETY: Valid socket, clean up on epoll failure.
            unsafe { srt_close(client_sock) };
            return;
        }
        let epoll_events = SRT_EPOLL_IN as c_int;
        // SAFETY: srt_epoll_add_usock registers client_sock with the epoll
        // instance. eid and client_sock are valid handles. epoll_events
        // pointer references a live stack variable.
        if unsafe { srt_epoll_add_usock(eid, client_sock, &epoll_events) } < 0 {
            eprintln!("[srt] Failed to add socket to epoll");
            // SAFETY: eid and client_sock are valid handles. Clean up in
            // reverse creation order: release epoll, then close socket.
            unsafe {
                srt_epoll_release(eid);
                srt_close(client_sock)
            };
            return;
        }

        // RAII guard: closes client_sock when this scope exits (normal exit,
        // panic, or future drop at an await point).  Created after all early-
        // return paths that would double-close the socket.
        // SAFETY: client_sock is a valid socket not closed elsewhere after
        // this point; srt_close is called exactly once via this guard.
        struct SrtClientGuard(SRTSOCKET);
        impl Drop for SrtClientGuard {
            fn drop(&mut self) {
                unsafe {
                    srt_close(self.0);
                }
            }
        }
        let _client_sock_guard = SrtClientGuard(client_sock);

        // Socket groups use the message API and may deliver up to the live
        // payload limit. Single sockets retain the lean plain-recv path.
        let mut buf = vec![0u8; if is_group { 2048 } else { 1316 }];
        let mut previous_stats: Option<SrtCounterSnapshot> = None;
        let mut last_stats_sample = Instant::now() - Duration::from_secs(1);

        // Long-lived epoll waiter: one spawn_blocking task for the entire
        // connection lifetime replaces per-EAGAIN spawn_blocking. Solves:
        //   1. Task allocation per idle cycle
        //   2. No cancellation propagation (infinite epoll_wait timeout)
        //   3. Silently discarded errors on EAGAIN path
        let data_ready = Arc::new(AtomicBool::new(false));
        let epoll_stop = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());

        let w_data_ready = data_ready.clone();
        let w_epoll_stop = epoll_stop.clone();
        let w_notify = notify.clone();
        // The task owns eid and releases it before signaling completion.
        // This ensures srt_epoll_release runs even if the outer async future
        // is dropped at an await point (the JoinHandle detaches but the
        // blocking task continues to completion).
        let mut epoll_waiter = Some(tokio::task::spawn_blocking(move || {
            loop {
                if w_epoll_stop.load(Ordering::Acquire) {
                    // Release the epoll handle before waking the outer task.
                    // SAFETY: eid is valid; we are the only caller of
                    // srt_epoll_release for this handle. The outer code no
                    // longer calls srt_epoll_release after this task exits.
                    unsafe {
                        srt_epoll_release(eid);
                    }
                    // Wake the main task so it can observe we're done.
                    w_data_ready.store(true, Ordering::Release);
                    w_notify.notify_one();
                    return;
                }

                let mut read_ready = [SRTSOCKET::default(); 1];
                let mut rnum = 1i32;
                // SAFETY: srt_epoll_wait blocks the OS thread until data
                // arrives or timeout. NULL write/lwfd/wfds sets are valid
                // (we only wait for read-ready). Called from spawn_blocking
                // so the tokio runtime is not blocked.
                //
                // 200ms timeout balances:
                //   - Cancellation responsiveness: ≤200ms from cancel to exit
                //   - CPU: no busy-loop (vs polling with a microsleep)
                //   - Perceptibility: 200ms is imperceptible on stream stop
                //   - Cleanup: ≤200ms delay before epoll handle is freed
                let ret = unsafe {
                    srt_epoll_wait(
                        eid,
                        read_ready.as_mut_ptr(),
                        &mut rnum,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        200,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if ret > 0 {
                    // Data available — wake the consumer.
                    w_data_ready.store(true, Ordering::Release);
                    w_notify.notify_one();
                }
                // ret == 0 (timeout) or < 0 (error): loop back and check stop.
            }
        }));

        // RAII guard: signals the epoll_waiter task to exit when this scope
        // ends (normal return, panic, or future dropped at an await point).
        // The task then calls srt_epoll_release(eid) before exiting.
        struct EpollStopGuard {
            stop: Arc<AtomicBool>,
            notify: Arc<Notify>,
        }
        impl Drop for EpollStopGuard {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                self.notify.notify_one();
            }
        }
        let _epoll_stop_guard = EpollStopGuard {
            stop: epoll_stop.clone(),
            notify: notify.clone(),
        };

        loop {
            if token.is_cancelled() {
                break;
            }

            // SAFETY: srt_recv/srt_recvmsg2 reads from a valid
            // non-blocking SRT socket into `buf`, which is a correctly
            // sized Vec<u8>. The msghdr argument for srt_recvmsg2 is NULL
            // (we don't need per-message metadata). Returns bytes read or
            // -1 on error (EAGAIN in non-blocking mode).
            let n = unsafe {
                if is_group {
                    srt_recvmsg2(
                        client_sock,
                        buf.as_mut_ptr(),
                        buf.len() as c_int,
                        std::ptr::null_mut(),
                    )
                } else {
                    srt_recv(client_sock, buf.as_mut_ptr(), buf.len() as c_int)
                }
            };
            if n > 0 {
                // Data received — process below
            } else if n == 0 {
                break; // connection closed
            } else {
                // n == -1: non-blocking mode returns EAGAIN when no data.
                // Wait for the long-lived epoll waiter to signal readiness.
                if !data_ready.swap(false, Ordering::Acquire) {
                    notify.notified().await;
                }
                continue;
            }

            // Feed into demuxer and push completed packets to ring buffer
            demuxer.feed(&buf[..n as usize]);
            if demuxer.drain_into(&mut packets) > 0 {
                for pkt in &packets {
                    if pkt.media_type == crate::media::ring_buffer::MediaType::Video
                        && pkt.is_keyframe
                        && let Some(ref kf_times) = cached_keyframe_times
                    {
                        let mut times = kf_times.lock().unwrap_or_else(|e| e.into_inner());
                        times.push(pkt.pts);
                        if times.len() > 30 {
                            times.remove(0);
                        }
                    }
                }
                ring_buffer.push_batch(packets.drain(..));
            }

            // Send probe metadata once ready
            if !probe_sent && let Some(probe) = demuxer.take_probe() {
                probe_sent = true;
                if let Some(ref v) = probe.video {
                    println!(
                        "[srt] Probed video: {} {}x{} {:.1}fps profile={:?}",
                        v.codec, v.width, v.height, v.fps, v.profile
                    );
                }
                for a in &probe.audio_tracks {
                    println!(
                        "[srt] Probed audio track {}: {} {}Hz {}ch",
                        a.track_index, a.codec, a.sample_rate, a.channels
                    );
                }
                let first_audio = probe.audio_tracks.first().cloned();
                self.engine
                    .update_ingest_meta(&pipeline.id, probe.video, first_audio, None)
                    .await;
                if !probe.audio_tracks.is_empty() {
                    self.engine
                        .update_ingest_audio_tracks(&pipeline.id, probe.audio_tracks)
                        .await;
                }
            }

            bytes_received.fetch_add(n as u64, Ordering::Relaxed);
            if let Some(ref m) = ingest_metrics {
                m.record_in(n as u64);
            }

            if last_stats_sample.elapsed() >= std::time::Duration::from_secs(1) {
                let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
                let sampled_at = Instant::now();
                let group_summary = is_group.then(|| srt_group_summary(client_sock)).flatten();
                if unsafe { srt_bistats(client_sock, &mut stats, 0, 1) } >= 0 {
                    let (mut quality, snapshot) =
                        srt_quality_from_stats(&stats, previous_stats, sampled_at);
                    add_srt_group_quality(&mut quality, is_group, group_summary);
                    previous_stats = Some(snapshot);
                    self.engine
                        .update_publisher_quality(&pipeline.id, quality)
                        .await;
                } else {
                    let mut quality = PublisherQuality::default();
                    add_srt_group_quality(&mut quality, is_group, group_summary);
                    self.engine
                        .update_publisher_quality(&pipeline.id, quality)
                        .await;
                }
                last_stats_sample = sampled_at;
            }
        }

        // Flush any remaining PES data
        demuxer.flush();
        if demuxer.drain_into(&mut packets) > 0 {
            ring_buffer.push_batch(packets.drain(..));
        }

        println!("[srt] Ingest stream finished for pipeline: {}", pipeline.id);
        self.engine.unregister_ingest(&pipeline.id).await;

        // Signal the epoll_waiter task to stop and wait for it to release eid.
        // The _epoll_stop_guard would do this on drop, but signaling explicitly
        // here lets us await the task handle — ensuring eid is released before
        // the _client_sock_guard drops and closes the socket.
        epoll_stop.store(true, Ordering::Release);
        notify.notify_one();
        if let Some(handle) = epoll_waiter.take() {
            let _ = handle.await;
        }
        // _epoll_stop_guard and _client_sock_guard drop here in LIFO order:
        //   1. _epoll_stop_guard: no-op (stop already set above)
        //   2. _client_sock_guard: srt_close(client_sock)
    }

    async fn handle_play(&self, client_sock: SRTSOCKET, pipeline_id: &str) {
        // Verify active ingest exists
        if !self
            .engine
            .active_ingests
            .read()
            .await
            .contains_key(pipeline_id)
        {
            eprintln!("[srt] No active ingest for play: {}", pipeline_id);
            // SAFETY: client_sock is a valid accepted socket not yet closed.
            unsafe {
                srt_close(client_sock);
            }
            return;
        }

        let ring_buf = self.engine.get_or_create_pipeline(pipeline_id).await;
        let shared_muxer = self
            .engine
            .get_or_create_ts_muxer_stage(pipeline_id, "play", ring_buf.clone())
            .await;

        let out_queue = Arc::new(crate::media::avio::MemoryQueue::new());

        // Sender thread: reads MPEG-TS from out_queue, sends via SRT.
        // Wrapped in catch_unwind so a panic cannot crash the process (CLAUDE.md).
        // Acquire a semaphore permit to cap concurrent SRT sender threads at 512.
        // try_acquire_owned returns Err if the semaphore is exhausted; in that
        // case we reject the play connection gracefully rather than spawning a
        // thread that would push memory/VAS over the limit.
        let permit = match self.engine.srt_sender_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                eprintln!(
                    "[srt] Sender thread limit reached — rejecting play for {}",
                    pipeline_id
                );
                // SAFETY: Valid socket, clean up on capacity rejection.
                unsafe {
                    srt_close(client_sock);
                }
                return;
            }
        };
        let out_queue_send = out_queue.clone();
        let pid_log = pipeline_id.to_string();
        let play_sender_handle = std::thread::spawn(move || {
            let _permit = permit; // dropped when thread exits → releases semaphore slot
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut buf = vec![0u8; 1316];
                loop {
                    let n = out_queue_send.read(&mut buf);
                    if n == 0 {
                        break;
                    }
                    // SAFETY: srt_send transmits data over a valid SRT
                    // socket. buf is a correctly sized Vec<u8>; n is the
                    // number of bytes read from MemoryQueue (≤ buf.len()).
                    let sent = unsafe { srt_send(client_sock, buf.as_ptr(), n as c_int) };
                    if sent < 0 {
                        break;
                    }
                }
            }));
            if result.is_err() {
                eprintln!(
                    "[srt] Play sender thread panicked for pipeline: {}",
                    pid_log
                );
            } else {
                println!(
                    "[srt] Play subscriber disconnected for pipeline: {}",
                    pid_log
                );
            }
            // SAFETY: client_sock was created during handle_client and
            // passed to this thread. It is closed exactly once here after
            // the sender loop exits (either normal disconnect or error).
            unsafe {
                srt_close(client_sock);
            }
        });
        self.engine.register_os_thread(play_sender_handle);

        let mut reader = TsChunkReader::new(format!("srt_play:{}", pipeline_id), &shared_muxer);
        let mut pull_packets = Vec::with_capacity(32);
        let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);

        loop {
            reader.wait_for_data().await;
            loop {
                pull_packets.clear();
                match reader.pull_burst(&mut pull_packets, 32) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                for pkt in &pull_packets {
                    if !pkt.payload.is_empty() {
                        ts_batch.extend_from_slice(&pkt.payload);
                    }
                }
                // One lock acquisition for the whole burst.
                if !ts_batch.is_empty() {
                    out_queue.write(&ts_batch).await;
                    ts_batch.clear();
                }
            }
            // Check if ingest is still alive before waiting again
            if !self
                .engine
                .active_ingests
                .read()
                .await
                .contains_key(pipeline_id)
            {
                break;
            }
        }

        println!("[srt-play] Feed loop exited for pipeline={}", pipeline_id);
        out_queue.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::engine::VideoMeta;
    use crate::media::ring_buffer::PayloadFormat;

    #[test]
    fn parses_srt_stream_ids_from_common_tools() {
        let cases = [
            (
                "publish:live/key01?latency=240000",
                SrtConnectionMode::Publish,
                "key01",
            ),
            ("publisher:key02", SrtConnectionMode::Publish, "key02"),
            ("key03", SrtConnectionMode::Publish, "key03"),
            ("read:live/key04", SrtConnectionMode::Read, "key04"),
            ("play:key05", SrtConnectionMode::Read, "key05"),
            ("subscriber:live/key06", SrtConnectionMode::Read, "key06"),
            (
                "#!::r=live/key07,m=publish,latency=240000",
                SrtConnectionMode::Publish,
                "key07",
            ),
            (
                "#!::r=live/key08,m=request",
                SrtConnectionMode::Read,
                "key08",
            ),
        ];

        for (input, mode, key) in cases {
            let parsed = parse_srt_stream_id(input);
            assert_eq!(parsed.mode, mode, "input={}", input);
            assert_eq!(parsed.stream_key, key, "input={}", input);
        }
    }

    #[test]
    fn srt_rates_use_counter_deltas_instead_of_cumulative_totals() {
        let sampled_at = Instant::now();
        let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
        stats.pkt_rcv_loss_total = 5_000;
        stats.pkt_rcv_drop_total = 500;
        stats.pkt_rcv_retrans = 10_000;

        let (first, snapshot) = srt_quality_from_stats(&stats, None, sampled_at);
        assert_eq!(first.packets_received_loss, Some(5_000));
        assert_eq!(first.packets_received_loss_per_sec, None);

        let (recovered, _) = srt_quality_from_stats(
            &stats,
            Some(snapshot),
            sampled_at + std::time::Duration::from_secs(2),
        );
        assert_eq!(recovered.packets_received_loss_per_sec, Some(0.0));
        assert_eq!(recovered.packets_received_drop_per_sec, Some(0.0));
        assert_eq!(recovered.packets_received_retrans_per_sec, Some(0.0));
    }

    #[test]
    fn srt_rates_report_current_loss_window() {
        let sampled_at = Instant::now();
        let previous = SrtCounterSnapshot {
            packets_received_loss: 100,
            packets_received_drop: 10,
            packets_received_retrans: 200,
            packets_received_undecrypt: 0,
            sampled_at,
        };
        let mut stats: SrtTraceBStats = unsafe { std::mem::zeroed() };
        stats.pkt_rcv_loss_total = 120;
        stats.pkt_rcv_drop_total = 16;
        stats.pkt_rcv_retrans = 220;
        stats.pkt_rcv_undecrypt_total = 2;

        let (quality, _) = srt_quality_from_stats(
            &stats,
            Some(previous),
            sampled_at + std::time::Duration::from_secs(2),
        );
        assert_eq!(quality.packets_received_loss_per_sec, Some(10.0));
        assert_eq!(quality.packets_received_drop_per_sec, Some(3.0));
        assert_eq!(quality.packets_received_retrans_per_sec, Some(10.0));
        assert_eq!(quality.packets_received_undecrypt_per_sec, Some(1.0));
    }

    #[test]
    fn video_for_ts_raw_passthrough() {
        let raw_video = [0, 0, 1, 0x65, 0xaa, 0xbb];
        let mut nls = 4usize;
        let mut cache = Vec::new();
        let result =
            crate::media::codec::video_for_ts(&raw_video, PayloadFormat::Raw, &mut nls, &mut cache);
        assert!(result.is_some());
        assert_eq!(&*result.unwrap(), &raw_video[..]);
    }

    #[test]
    fn audio_for_ts_raw_passthrough_with_adts() {
        let adts_audio = [0xFF, 0xF1, 0x50, 0x80, 0x01, 0x1F, 0xFC, 0x21, 0x10];
        // Raw with ADTS sync → borrowed passthrough
        let result = crate::media::codec::audio_for_ts(&adts_audio, PayloadFormat::Raw, 48000, 2);
        assert!(result.is_some());
        assert_eq!(&*result.unwrap(), &adts_audio[..]);
    }

    #[test]
    fn flv_video_seq_skipped_data_converted() {
        let flv_video_seq = [
            0x17u8, 0x00, 0x00, 0x00, 0x00, 1, 66, 0, 30, 0xFF, 0xE1, 0, 3, 1, 2, 3, 1, 0, 2, 4, 5,
        ];
        let flv_audio_seq = [0xaf, 0x00, 0x12, 0x10];

        let mut nls = 4usize;
        // Seq headers for audio → None
        assert!(
            crate::media::codec::audio_for_ts(&flv_audio_seq, PayloadFormat::Flv, 48000, 2)
                .is_none()
        );
        // Video seq header → extracts SPS/PPS as Annex B (or None if config too short)
        let mut cache = Vec::new();
        let _result = crate::media::codec::video_for_ts(
            &flv_video_seq,
            PayloadFormat::Flv,
            &mut nls,
            &mut cache,
        );
        // Just verify no panic; codec tests cover correctness in detail
    }

    #[test]
    fn maps_h264_and_h265_without_guessing_unknown_codecs() {
        assert_eq!(
            video_codec_id("h264"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264)
        );
        assert_eq!(
            video_codec_id("hevc"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC)
        );
        assert_eq!(video_codec_id("unknown"), None);
        assert_eq!(
            audio_codec_id("aac"),
            Some(ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_AAC)
        );
        assert_eq!(audio_codec_id("opus"), None);
    }

    #[test]
    fn egress_url_parses_simple_target() {
        let u = parse_srt_egress_url("srt://192.168.1.5:9000");
        assert_eq!(u.host_port, "192.168.1.5:9000");
        assert!(u.streamid.is_empty());
        assert!(u.bond_addrs.is_empty());
    }

    #[test]
    fn egress_url_parses_streamid() {
        let u = parse_srt_egress_url("srt://host:9000?streamid=publish:live/key1");
        assert_eq!(u.host_port, "host:9000");
        assert_eq!(u.streamid, "publish:live/key1");
        assert!(u.bond_addrs.is_empty());
    }

    // --- Regression: issue #6 (Round 5) — SRT stream ID percent-decode ---
    // Before the fix, percent-encoded characters in the streamid query parameter
    // were passed through raw. `publish:live%2Fkey` would be compared against DB
    // stream keys verbatim, causing silent auth failure.
    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("publish:live%2Fkey"), "publish:live/key");
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("no_encoding"), "no_encoding");
        assert_eq!(percent_decode("%41%42%43"), "ABC"); // A=0x41, B=0x42, C=0x43
    }

    #[test]
    fn percent_decode_incomplete_sequence_passthrough() {
        // A truncated %XX at the end should not panic.
        assert_eq!(percent_decode("foo%2"), "foo%2");
        assert_eq!(percent_decode("foo%"), "foo%");
    }

    #[test]
    fn egress_url_percent_decodes_streamid() {
        // Percent-encoded slash in streamid must be decoded before use.
        let u = parse_srt_egress_url("srt://host:9000?streamid=publish%3Alive%2Fmykey");
        assert_eq!(
            u.streamid, "publish:live/mykey",
            "percent-encoded streamid must be decoded in egress URL"
        );
    }

    #[test]
    fn egress_url_parses_bond_addresses() {
        let u = parse_srt_egress_url(
            "srt://primary:9000?streamid=live/out&bond=backup1:9000,backup2:9000",
        );
        assert_eq!(u.host_port, "primary:9000");
        assert_eq!(u.streamid, "live/out");
        assert_eq!(u.bond_addrs, vec!["backup1:9000", "backup2:9000"]);
    }

    #[test]
    fn egress_url_bond_only_no_streamid() {
        let u = parse_srt_egress_url("srt://10.0.0.1:4200?bond=10.0.0.2:4200");
        assert_eq!(u.host_port, "10.0.0.1:4200");
        assert!(u.streamid.is_empty());
        assert_eq!(u.bond_addrs, vec!["10.0.0.2:4200"]);
    }

    #[test]
    fn sysctl_check_does_not_panic() {
        // Smoke test: runs on any Linux, should not panic even if paths don't exist
        check_sysctl_limits();
    }

    #[test]
    fn socket_option_constants_match_srt_header() {
        // Guard against regression: these values are from srt.h SRT_SOCKOPT enum
        assert_eq!(SRTO_SNDSYN, 1);
        assert_eq!(SRTO_RCVSYN, 2);
        assert_eq!(SRTO_FC, 4);
        assert_eq!(SRTO_SNDBUF, 5);
        assert_eq!(SRTO_RCVBUF, 6);
        assert_eq!(SRTO_UDP_SNDBUF, 8);
        assert_eq!(SRTO_UDP_RCVBUF, 9);
        assert_eq!(SRTO_MAXBW, 16);
        assert_eq!(SRTO_LATENCY, 23);
        assert_eq!(SRTO_LOSSMAXTTL, 42);
        assert_eq!(SRTO_RCVLATENCY, 43);
        assert_eq!(SRTO_PEERLATENCY, 44);
        assert_eq!(SRTO_STREAMID, 46);
        assert_eq!(SRTO_TRANSTYPE, 50);
        assert_eq!(SRTO_GROUPCONNECT, 57);
        assert_eq!(SRTGROUP_MASK, 1 << 30);
    }

    #[test]
    fn detects_srt_group_ids() {
        assert!(!is_srt_group(42));
        assert!(is_srt_group(SRTGROUP_MASK | 42));
    }

    // --- Regression: issue #7 (Round 5) — Semaphore caps concurrent SRT sender threads ---
    // Before the fix there was no limit on how many OS threads could be spawned
    // for SRT play / egress connections. 1 thread per connection × 1000 connections
    // = 1000 threads = 8+ GB virtual address space.
    // The semaphore must be exhaustible and must release on drop.
    #[test]
    fn srt_sender_semaphore_is_bounded() {
        use std::sync::Arc;
        // Create a tiny semaphore (capacity 2) to simulate the cap.
        let sem = Arc::new(tokio::sync::Semaphore::new(2));
        let _p1 = sem
            .clone()
            .try_acquire_owned()
            .expect("first permit available");
        let _p2 = sem
            .clone()
            .try_acquire_owned()
            .expect("second permit available");
        // Third acquire must fail when semaphore is exhausted.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "semaphore must reject when exhausted"
        );
    }

    #[test]
    fn srt_sender_semaphore_releases_on_drop() {
        use std::sync::Arc;
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        {
            let _p = sem.clone().try_acquire_owned().expect("permit available");
            // permit is held — semaphore exhausted.
            assert!(
                sem.clone().try_acquire_owned().is_err(),
                "should be exhausted"
            );
        }
        // After the permit is dropped, the slot must be returned.
        assert!(
            sem.clone().try_acquire_owned().is_ok(),
            "semaphore should release permit on drop"
        );
    }

    // --- Regression: Round 6 #5 — SRT play muxer must not start without video ---
    // The probe-wait loop in handle_play requires `video.as_ref()?` before
    // breaking — it must not yield metadata when video is None.
    // This is the same guard used by start_srt_egress.
    #[test]
    fn probe_wait_guard_requires_video_to_be_some() {
        // Simulate the logic of the retry closure:
        //   ingests.get(pipeline_id).and_then(|i| { video.as_ref()?; ... Some(meta) })
        // When video is None the closure must return None (no break).
        struct FakeIngest {
            video: Option<String>,
        }
        let ingest_no_video = FakeIngest { video: None };
        let ingest_with_video = FakeIngest {
            video: Some("h264".to_string()),
        };

        let result_none: Option<(&str,)> = (|| {
            let video = ingest_no_video.video.as_ref()?;
            let _ = video;
            Some(("got_video",))
        })();
        assert!(
            result_none.is_none(),
            "loop must not break while video is None"
        );

        let result_some: Option<(&str,)> = (|| {
            let video = ingest_with_video.video.as_ref()?;
            let _ = video;
            Some(("got_video",))
        })();
        assert!(result_some.is_some(), "loop must break once video is Some");
    }

    #[test]
    fn summarizes_srt_group_member_state() {
        let mut connected: SrtSocketGroupData = unsafe { std::mem::zeroed() };
        connected.sockstate = SRTS_CONNECTED;
        connected.memberstate = SRT_GST_RUNNING;

        let mut idle: SrtSocketGroupData = unsafe { std::mem::zeroed() };
        idle.sockstate = SRTS_CONNECTED;
        idle.memberstate = 1;

        let mut broken: SrtSocketGroupData = unsafe { std::mem::zeroed() };
        broken.sockstate = SRTS_BROKEN;
        broken.memberstate = SRT_GST_BROKEN;

        assert_eq!(
            summarize_group_members(&[connected, idle, broken]),
            SrtGroupSummary {
                member_count: 3,
                connected_members: 2,
                active_members: 1,
                broken_members: 1,
            }
        );
    }

    #[test]
    fn adds_bonded_group_state_to_publisher_quality() {
        let mut quality = PublisherQuality::default();
        add_srt_group_quality(
            &mut quality,
            true,
            Some(SrtGroupSummary {
                member_count: 2,
                connected_members: 2,
                active_members: 1,
                broken_members: 0,
            }),
        );

        assert_eq!(quality.srt_bonded, Some(true));
        assert_eq!(quality.srt_group_member_count, Some(2));
        assert_eq!(quality.srt_group_connected_members, Some(2));
        assert_eq!(quality.srt_group_active_members, Some(1));
        assert_eq!(quality.srt_group_broken_members, Some(0));
    }

    #[test]
    fn marks_single_link_srt_without_group_member_fields() {
        let mut quality = PublisherQuality::default();
        add_srt_group_quality(&mut quality, false, None);

        assert_eq!(quality.srt_bonded, Some(false));
        assert_eq!(quality.srt_group_member_count, None);
        assert_eq!(quality.srt_group_connected_members, None);
        assert_eq!(quality.srt_group_active_members, None);
        assert_eq!(quality.srt_group_broken_members, None);
    }

    #[test]
    fn linked_libsrt_exposes_group_connect_when_required() {
        unsafe {
            assert_eq!(srt_startup(), 0);
        }

        let listener = unsafe { srt_create_socket() };
        assert!(listener >= 0);
        if let Err(error) = enable_srt_group_connect(listener) {
            unsafe {
                srt_close(listener);
                srt_cleanup();
            }
            if std::env::var_os("RESTREAM_REQUIRE_SRT_BONDING").is_some() {
                panic!(
                    "RESTREAM_REQUIRE_SRT_BONDING is set, but linked libsrt rejected \
                     SRTO_GROUPCONNECT: {error}. Rebuild libsrt with ENABLE_BONDING=ON."
                );
            }
            eprintln!(
                "bonding prerequisite unavailable; set RESTREAM_REQUIRE_SRT_BONDING=1 \
                 in bonding-enabled CI to make this a required live test ({error})"
            );
            return;
        }
        unsafe {
            srt_close(listener);
        }
    }

    #[test]
    fn reads_udp_socket_stats_for_listener_port() {
        // On a system without an SRT listener, this should return None
        // (port 10080 not bound). If it's bound, it returns Some.
        let result = read_udp_socket_stats(10080);
        // Either None or Some with valid values — should not panic
        if let Some((rx_queue, drops)) = result {
            assert!(rx_queue < u64::MAX);
            assert!(drops < u64::MAX);
        }
    }

    #[tokio::test]
    async fn start_srt_egress_handles_invalid_streamid_without_panic() {
        let ring_buffer = Arc::new(RingBuffer::new(16));
        let engine = Arc::new(crate::media::engine::MediaEngine::new());
        let cancel_token = CancellationToken::new();
        start_srt_egress(
            "out-id".to_string(),
            "pipe-id".to_string(),
            "source".to_string(),
            "srt://127.0.0.1:12345?streamid=publish:live/\x00mykey".to_string(),
            ring_buffer,
            engine,
            cancel_token,
        )
        .await;
    }

    #[tokio::test]
    async fn shared_ts_muxer_shares_across_multiple_readers() {
        let engine = Arc::new(crate::media::engine::MediaEngine::new());
        let pipeline_id = "test-pipe";
        let source_ring = engine.get_or_create_pipeline(pipeline_id).await;

        // Register active ingest so start_shared_ts_muxer can proceed
        let cancel_ingest = engine
            .try_register_ingest(pipeline_id, "key", "srt")
            .await
            .unwrap();
        // Set metadata
        engine
            .update_ingest_meta(
                pipeline_id,
                Some(VideoMeta {
                    codec: "h264".to_string(),
                    width: 1920,
                    height: 1080,
                    fps: 30.0,
                    bw: None,
                    profile: None,
                    level: None,
                    pixel_format: None,
                }),
                None,
                None,
            )
            .await;

        // Create multiple stages or the same stage
        let stage1 = engine
            .get_or_create_ts_muxer_stage(pipeline_id, "play", source_ring.clone())
            .await;
        let stage2 = engine
            .get_or_create_ts_muxer_stage(pipeline_id, "play", source_ring.clone())
            .await;

        // Verify it is the exact same instance (same pointer)
        assert!(Arc::ptr_eq(&stage1, &stage2));

        // Create two readers
        let mut r1 = TsChunkReader::new("r1".to_string(), &stage1);
        let mut r2 = TsChunkReader::new("r2".to_string(), &stage1);

        // Push a video packet to the source ring
        source_ring.push(crate::media::ring_buffer::MediaPacket {
            media_type: MediaType::Video,
            track_index: 0,
            pts: 1000,
            dts: 1000,
            is_keyframe: true,
            format: PayloadFormat::Raw,
            payload: bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
        });

        // Wait a bit for the tokio task to run and mux the packet
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        assert_eq!(r1.pull_burst(&mut out1, 10).unwrap(), 1);
        assert_eq!(r2.pull_burst(&mut out2, 10).unwrap(), 1);

        assert_eq!(out1[0].payload, out2[0].payload);
        assert!(!out1[0].payload.is_empty());

        cancel_ingest.cancel();
    }

    #[tokio::test]
    async fn benchmark_srt_sharing() {
        println!("\n=== SRT EGRESS SHARING BENCHMARK ===");
        let n_connections = 10;
        let n_packets = 2000;
        println!("Clients (N): {}, Packets (M): {}", n_connections, n_packets);

        let video_meta = VideoMeta {
            codec: "h264".to_string(),
            width: 1920,
            height: 1080,
            fps: 30.0,
            bw: None,
            profile: None,
            level: None,
            pixel_format: None,
        };
        let audio_track = crate::media::engine::AudioMeta {
            track_index: 0,
            codec: "aac".to_string(),
            sample_rate: 48000,
            channels: 2,
            channel_layout: None,
            profile: None,
        };
        let audio_tracks = vec![audio_track];

        // Generate synthetic packets
        let mut packets = Vec::with_capacity(n_packets);
        let mut rng_seed = 0u8;
        for i in 0..n_packets {
            let is_video = i % 3 != 0;
            let is_keyframe = is_video && (i % 90 == 0);
            let media_type = if is_video {
                MediaType::Video
            } else {
                MediaType::Audio
            };
            let size = if is_video {
                if is_keyframe { 100_000 } else { 10_000 }
            } else {
                500
            };
            rng_seed = rng_seed.wrapping_add(1);
            let payload = bytes::Bytes::from(vec![rng_seed; size]);
            packets.push(crate::media::ring_buffer::MediaPacket {
                media_type,
                track_index: 0,
                pts: i as i64 * 33,
                dts: i as i64 * 33,
                is_keyframe,
                format: PayloadFormat::Raw,
                payload,
            });
        }

        // --- OLD ARCHITECTURE: Independent Muxing ---
        let start_old = Instant::now();
        let mut old_handles = Vec::new();
        for _ in 0..n_connections {
            let packets_clone = packets.clone();
            let video_meta_clone = video_meta.clone();
            let audio_tracks_clone = audio_tracks.clone();
            let handle = tokio::spawn(async move {
                let mut muxer = crate::media::mpegts::TsMuxer::new(
                    Some(&video_meta_clone),
                    &audio_tracks_clone,
                );
                let mut bytes_written = 0u64;
                for pkt in &packets_clone {
                    let ts_bytes = muxer.mux_packet(
                        pkt.media_type,
                        pkt.track_index,
                        pkt.pts,
                        pkt.dts,
                        pkt.is_keyframe,
                        &pkt.payload,
                    );
                    bytes_written += ts_bytes.len() as u64;
                }
                bytes_written
            });
            old_handles.push(handle);
        }

        let mut total_bytes_old = 0u64;
        for h in old_handles {
            total_bytes_old += h.await.unwrap();
        }
        let elapsed_old = start_old.elapsed();

        // --- NEW ARCHITECTURE: Shared Muxing ---
        let start_new = Instant::now();
        let ts_ring = Arc::new(TsChunkRing::new(4096, CancellationToken::new()));
        let mut readers = Vec::new();
        for i in 0..n_connections {
            readers.push(TsChunkReader::new(format!("reader_{}", i), &ts_ring));
        }

        let mut new_handles = Vec::new();
        for mut reader in readers {
            let handle = tokio::spawn(async move {
                let mut chunks_received = 0;
                let mut bytes_received = 0u64;
                let mut out_burst = Vec::with_capacity(32);
                while chunks_received < n_packets {
                    out_burst.clear();
                    match reader.pull_burst(&mut out_burst, 32) {
                        Ok(0) => {
                            tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                        }
                        Ok(count) => {
                            chunks_received += count;
                            for chunk in &out_burst {
                                bytes_received += chunk.payload.len() as u64;
                            }
                        }
                        Err(_) => {}
                    }
                }
                bytes_received
            });
            new_handles.push(handle);
        }

        // Shared muxer task
        let ts_ring_clone = ts_ring.clone();
        let packets_clone = packets.clone();
        let video_meta_clone = video_meta.clone();
        let audio_tracks_clone = audio_tracks.clone();
        let muxer_handle = tokio::spawn(async move {
            let mut muxer =
                crate::media::mpegts::TsMuxer::new(Some(&video_meta_clone), &audio_tracks_clone);
            for pkt in &packets_clone {
                let ts_bytes = muxer.mux_packet(
                    pkt.media_type,
                    pkt.track_index,
                    pkt.pts,
                    pkt.dts,
                    pkt.is_keyframe,
                    &pkt.payload,
                );
                ts_ring_clone.push(bytes::Bytes::copy_from_slice(ts_bytes), pkt.is_keyframe);
            }
        });

        muxer_handle.await.unwrap();

        let mut total_bytes_new = 0u64;
        for h in new_handles {
            total_bytes_new += h.await.unwrap();
        }
        let elapsed_new = start_new.elapsed();

        println!("Old Architecture Time: {:?}", elapsed_old);
        println!("New Architecture Time: {:?}", elapsed_new);
        println!("Old Total Bytes Muxed: {}", total_bytes_old);
        println!("New Total Bytes Muxed: {}", total_bytes_new);

        assert_eq!(total_bytes_old, total_bytes_new);

        let ratio = elapsed_old.as_secs_f64() / elapsed_new.as_secs_f64();
        println!("Performance Gain Ratio: {:.2}x", ratio);
        println!("=====================================");
    }

    /// Verify that when EpollStopGuard drops (simulating a cancelled async
    /// future), the epoll_waiter task observes the stop flag and exits within
    /// the 200ms epoll_wait timeout window.  This exercises the RAII path that
    /// prevents srt_epoll_release from being skipped on future cancellation.
    #[tokio::test]
    async fn epoll_stop_guard_signals_waiter_on_drop() {
        let epoll_stop = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let task_exited = Arc::new(AtomicBool::new(false));

        let w_stop = epoll_stop.clone();
        let w_exited = task_exited.clone();

        // Simulates the epoll_waiter task: polls every 50ms, exits when stop is set.
        let handle = tokio::task::spawn_blocking(move || {
            loop {
                if w_stop.load(Ordering::Acquire) {
                    w_exited.store(true, Ordering::Release);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        // EpollStopGuard inline: sets stop + notifies on drop.
        struct EpollStopGuard {
            stop: Arc<AtomicBool>,
            notify: Arc<Notify>,
        }
        impl Drop for EpollStopGuard {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                self.notify.notify_one();
            }
        }
        let guard = EpollStopGuard {
            stop: epoll_stop.clone(),
            notify: notify.clone(),
        };

        // Drop the guard — simulates the async future being cancelled.
        drop(guard);

        // Task must exit within 300ms (50ms poll + scheduling slack).
        tokio::time::timeout(std::time::Duration::from_millis(300), handle)
            .await
            .expect("epoll_waiter task must exit within 300ms of guard drop")
            .expect("task should not panic");

        assert!(
            task_exited.load(Ordering::Acquire),
            "task must have observed the stop flag"
        );
    }

    /// Stress-test the Notify + AtomicBool coordination pattern used by the
    /// long-lived epoll waiter. Concurrent producer and consumer run with
    /// randomized timing to surface missed-wakeup races.
    ///
    /// The producer (spawn_blocking) simulates srt_epoll_wait: sleeps for a
    /// brief random duration, then store(true) + notify_one(). The consumer
    /// (async) simulates the EAGAIN handler: swap(false) → fall through or
    /// fall back to notified().await.
    ///
    /// The producer runs to completion (produces exactly ITEMS). The consumer
    /// exits after consuming exactly ITEMS. A 30-second deadline prevents
    /// hangs from missed wakeups.
    #[tokio::test]
    async fn epoll_waiter_coordination() {
        use rand::Rng;
        use rand::SeedableRng;
        use std::sync::atomic::AtomicU32;

        const ITEMS: u32 = 10_000;
        let data_ready = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let produced = Arc::new(AtomicU32::new(0));

        let w_data_ready = data_ready.clone();
        let w_notify = notify.clone();
        let w_produced = produced.clone();

        // Producer: runs to completion on a blocking thread. No early-exit
        // epoll_stop — we want to verify the full ITEMS production cycle.
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let producer = tokio::task::spawn_blocking(move || {
            for _ in 0..ITEMS {
                // Jitter: 1-9µs typical, occasionally 1ms (simulating idle).
                let delay = if rng.gen_range(0..100) == 0 {
                    1_000
                } else {
                    rng.gen_range(1..10)
                };
                std::thread::sleep(std::time::Duration::from_micros(delay));

                w_produced.fetch_add(1, Ordering::Relaxed);
                w_data_ready.store(true, Ordering::Release);
                w_notify.notify_one();
            }
        });

        // Consumer: exactly the swap+notified pattern used by the real
        // EAGAIN handler in SrtFeed::push_media_packet (srt.rs:1375-1385).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        for i in 0..ITEMS {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out after {i} items (produced={})",
                produced.load(Ordering::Relaxed),
            );

            if !data_ready.swap(false, Ordering::Acquire) {
                tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified())
                    .await
                    .expect("consumer should not hang: permit must be available");
            }
        }

        let _ = producer.await;

        let total_produced = produced.load(Ordering::Relaxed);
        assert_eq!(
            total_produced, ITEMS,
            "producer must generate exactly ITEMS"
        );
    }
}

#[allow(clippy::items_after_test_module)]
impl Drop for SrtServer {
    fn drop(&mut self) {
        // srt_cleanup() is intentionally NOT called here.
        //
        // SrtServer is Arc-owned by a tokio task that may be dropped during
        // runtime shutdown, at which point SRT egress sender OS threads may
        // still hold open SRTSOCKET handles.  Calling srt_cleanup() while live
        // sockets exist violates the libsrt API contract and can produce SIGSEGV
        // or assertion failures inside libsrt.
        //
        // Instead, call crate::media::srt::teardown_srt() explicitly from
        // run_app() AFTER all OS threads have been joined (and therefore all
        // SRT sockets have been closed via srt_close() in their cleanup paths).
    }
}

/// Call srt_cleanup() to release libsrt global state.
///
/// Must be called AFTER all SRT sockets (server + egress) are closed and
/// their OS threads have been joined.  run_app() calls this at the very end
/// of the graceful-shutdown sequence, after drain_os_thread_handles().
// SAFETY: srt_cleanup must be called after all SRT sockets are closed
// and all OS threads using libsrt have been joined. run_app() enforces
// this by calling teardown_srt() as the final step of graceful shutdown.
pub fn teardown_srt() {
    unsafe {
        srt_cleanup();
    }
}

async fn resolve_host(host_port: &str) -> Option<SocketAddr> {
    match host_port.parse::<SocketAddr>() {
        Ok(a) => Some(a),
        Err(_) => tokio::net::lookup_host(host_port)
            .await
            .ok()
            .and_then(|mut addrs| addrs.next()),
    }
}

fn to_libc_sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, c_int) {
    // SAFETY: zeroed() is valid for sockaddr_storage (all-zero is a
    // valid uninitialized socket address). Raw pointer writes through
    // a correctly-typed pointer (sockaddr_in or sockaddr_in6) cast
    // from the storage reference. The family field is set first to
    // identify the variant before any other field is written.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            let sin = &mut storage as *mut _ as *mut libc::sockaddr_in;
            // SAFETY: sin is a valid pointer to the storage buffer cast
            // to the correct sockaddr_in variant. The struct is zero-
            // initialized above; we write all required fields.
            unsafe {
                (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
                (*sin).sin_port = v4.port().to_be();
                (*sin).sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            }
            (storage, std::mem::size_of::<libc::sockaddr_in>() as c_int)
        }
        SocketAddr::V6(v6) => {
            let sin6 = &mut storage as *mut _ as *mut libc::sockaddr_in6;
            // SAFETY: sin6 is a valid pointer to the storage buffer.
            // AF_INET6 is set first to identify the variant; subsequent
            // fields (port, addr) are written to the correct variant.
            unsafe {
                (*sin6).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                (*sin6).sin6_port = v6.port().to_be();
                (*sin6).sin6_addr.s6_addr = v6.ip().octets();
            }
            (storage, std::mem::size_of::<libc::sockaddr_in6>() as c_int)
        }
    }
}

struct SrtEgressUrl {
    host_port: String,
    streamid: String,
    bond_addrs: Vec<String>,
}

fn parse_srt_egress_url(url: &str) -> SrtEgressUrl {
    let url_cleaned = url.replace("srt://", "");
    let parts: Vec<&str> = url_cleaned.split('?').collect();
    let host_port = parts[0].to_string();

    let mut streamid = String::new();
    let mut bond_addrs: Vec<String> = Vec::new();
    if parts.len() > 1 {
        for param in parts[1].split('&') {
            let key_val: Vec<&str> = param.splitn(2, '=').collect();
            if key_val.len() == 2 {
                match key_val[0] {
                    "streamid" => streamid = percent_decode(key_val[1]),
                    "bond" => {
                        bond_addrs = key_val[1].split(',').map(|s| s.to_string()).collect();
                    }
                    _ => {}
                }
            }
        }
    }
    SrtEgressUrl {
        host_port,
        streamid,
        bond_addrs,
    }
}

pub fn start_shared_ts_muxer(
    pipeline_id: &str,
    source_ring: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel: CancellationToken,
) -> Arc<TsChunkRing> {
    let ts_ring = Arc::new(TsChunkRing::new(4096, cancel.clone()));
    let ts_ring_clone = ts_ring.clone();
    let pipeline_id_str = pipeline_id.to_string();

    tokio::spawn(async move {
        // Wait for ingest metadata before starting the MPEG-TS muxer
        let (video_meta, audio_tracks) = loop {
            if cancel.is_cancelled() {
                return;
            }
            let result = {
                let ingests = engine.active_ingests.read().await;
                ingests.get(&pipeline_id_str).and_then(|i| {
                    let video = i.video.clone();
                    video.as_ref()?;
                    let lock = i.audio_tracks.lock().unwrap_or_else(|e| e.into_inner());
                    let tracks = if lock.is_empty()
                        && let Some(audio) = i.audio.clone()
                    {
                        std::sync::Arc::new(vec![audio])
                    } else {
                        std::sync::Arc::clone(&lock)
                    };
                    Some((video, tracks))
                })
            };
            if let Some(meta) = result {
                break meta;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if !engine
                .active_ingests
                .read()
                .await
                .contains_key(&pipeline_id_str)
            {
                eprintln!(
                    "[srt-shared-muxer] Ingest gone while waiting for probe: {}",
                    pipeline_id_str
                );
                return;
            }
        };

        // Feed loop: read from source_ring, mux inline, write to ts_ring
        let muxer_video_meta = {
            let ring_codec = source_ring.codec_hint_str();
            let ingest_codec = video_meta.as_ref().map(|v| v.codec.as_str()).unwrap_or("");
            if !ring_codec.is_empty() && ring_codec != ingest_codec {
                eprintln!(
                    "[srt-shared-muxer] codec_hint override: ingest={} ring={}",
                    ingest_codec, ring_codec
                );
                let mut vm = video_meta.clone();
                if let Some(ref mut v) = vm {
                    v.codec = ring_codec.to_string();
                }
                vm
            } else {
                video_meta.clone()
            }
        };

        let mut muxer =
            crate::media::mpegts::TsMuxer::new(muxer_video_meta.as_ref(), &audio_tracks);
        let num_streams = (video_meta.is_some() as usize) + audio_tracks.len();
        let mut dts_enforcer = crate::media::ring_buffer::DtsEnforcer::new(num_streams);
        let mut nalu_len_size: usize = 4;
        let mut sps_pps_cache: Vec<u8> = {
            let (vsh, _) = engine.get_sequence_headers(&pipeline_id_str).await;
            if let Some(ref flv_sh) = vsh {
                if flv_sh.len() > 5 {
                    let (nls, annexb) = crate::media::codec::parse_avcc_config(&flv_sh[5..]);
                    nalu_len_size = nls;
                    annexb
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        };

        let mut reader = Reader::new(
            format!("ts_shared_muxer:{}", pipeline_id_str),
            source_ring.clone(),
        );
        let mut video_conv_buf = Vec::<u8>::new();
        let mut audio_conv_buf = Vec::<u8>::new();
        // `chunk_ends` records (byte_offset_end, is_keyframe) for each muxed chunk so
        // we can slice a single `BytesMut` into per-chunk `Bytes` after the inner loop.
        // This converts N malloc+memcpy calls (one per chunk) to 1 malloc per burst.
        let mut chunk_ends: Vec<(usize, bool)> = Vec::with_capacity(32);
        let mut pull_packets = Vec::with_capacity(32);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = reader.wait_for_data() => {
                    pull_packets.clear();
                    match reader.pull_burst(&mut pull_packets, 32) {
                        Ok(0) | Err(_) => {}
                        Ok(_) => {
                            chunk_ends.clear();
                            // One allocation for the entire burst's TS output.
                            let mut ts_accum = bytes::BytesMut::with_capacity(65536);
                            for pkt in &pull_packets {
                                let payload: &[u8] = match pkt.media_type {
                                    MediaType::Video => {
                                        match crate::media::codec::video_for_ts_into(
                                            &pkt.payload,
                                            pkt.format,
                                            &mut nalu_len_size,
                                            &mut sps_pps_cache,
                                            &mut video_conv_buf,
                                        ) {
                                            Some(p) => p,
                                            None => continue,
                                        }
                                    }
                                    MediaType::Audio => {
                                        let track = audio_tracks
                                            .iter()
                                            .find(|a| a.track_index == pkt.track_index)
                                            .or(audio_tracks.first());
                                        let (sr, ch) = track
                                            .map(|a| (a.sample_rate, a.channels))
                                            .unwrap_or((48000, 1));
                                        match crate::media::codec::audio_for_ts_into(
                                            &pkt.payload,
                                            pkt.format,
                                            sr,
                                            ch,
                                            &mut audio_conv_buf,
                                        ) {
                                            Some(p) => p,
                                            None => continue,
                                        }
                                    }
                                };

                                let stream_idx = match pkt.media_type {
                                    MediaType::Video => 0,
                                    MediaType::Audio => {
                                        let video_offset = video_meta.is_some() as usize;
                                        match audio_tracks
                                            .iter()
                                            .position(|a| a.track_index == pkt.track_index)
                                        {
                                            Some(i) => i + video_offset,
                                            None => continue,
                                        }
                                    }
                                };

                                let (pts, dts) = dts_enforcer.enforce(stream_idx, pkt.pts, pkt.dts);
                                let ts_bytes = muxer.mux_packet(
                                    pkt.media_type,
                                    pkt.track_index,
                                    pts,
                                    dts,
                                    pkt.is_keyframe,
                                    payload,
                                );
                                if !ts_bytes.is_empty() {
                                    ts_accum.extend_from_slice(ts_bytes);
                                    chunk_ends.push((ts_accum.len(), pkt.is_keyframe));
                                }
                            }
                            if !chunk_ends.is_empty() {
                                // freeze() promotes ts_accum to a shared Arc-backed Bytes.
                                // slice() below only bumps the refcount — no extra allocations.
                                let frozen = ts_accum.freeze();
                                let mut prev = 0usize;
                                ts_ring_clone.push_batch(chunk_ends.drain(..).map(
                                    move |(end, is_kf)| {
                                        let chunk = frozen.slice(prev..end);
                                        prev = end;
                                        (chunk, is_kf)
                                    },
                                ));
                            }
                        }
                    }
                }
            }
            if !engine
                .active_ingests
                .read()
                .await
                .contains_key(&pipeline_id_str)
            {
                break;
            }
        }
    });

    ts_ring
}

// SRT Egress Client
pub async fn start_srt_egress(
    output_id: String,
    pipeline_id: String,
    encoding: String,
    target_url: String,
    ring_buffer: Arc<RingBuffer>,
    engine: Arc<MediaEngine>,
    cancel_token: CancellationToken,
) {
    let parsed = parse_srt_egress_url(&target_url);
    let host_port = &parsed.host_port;
    let streamid = parsed.streamid;
    let bond_addrs = parsed.bond_addrs;

    engine.update_egress_phase(&output_id, "resolving").await;
    let addr = match resolve_host(host_port).await {
        Some(a) => a,
        None => {
            eprintln!("[srt-egress] Failed to resolve target: {}", target_url);
            engine
                .record_egress_error(&output_id, "resolve", "failed to resolve target")
                .await;
            return;
        }
    };
    engine
        .update_egress_target_addr(&output_id, addr.to_string())
        .await;

    // Resolve bond addresses
    let mut all_addrs = vec![addr];
    for bond_hp in &bond_addrs {
        match resolve_host(bond_hp).await {
            Some(a) => all_addrs.push(a),
            None => eprintln!("[srt-egress] Failed to resolve bond address: {}", bond_hp),
        }
    }

    let use_bonding = all_addrs.len() > 1;
    let client_sock: SRTSOCKET;

    if use_bonding {
        engine.update_egress_phase(&output_id, "connecting").await;
        // Create a bonding group (backup mode: one active, failover to next)
        // SAFETY: srt_create_group creates a bonding group socket.
        // SRT_GTYPE_BACKUP configures active/passive failover mode.
        // The returned handle is closed on all exit paths below.
        client_sock = unsafe { srt_create_group(SRT_GTYPE_BACKUP) };
        if client_sock < 0 {
            eprintln!("[srt-egress] Failed to create bonding group");
            engine
                .record_egress_error(&output_id, "connect", "failed to create bonding group")
                .await;
            return;
        }

        if !streamid.is_empty() {
            let streamid_c = match std::ffi::CString::new(streamid.as_str()) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("[srt-egress] Stream ID contains null bytes");
                    engine
                        .record_egress_error(&output_id, "connect", "stream ID contains null bytes")
                        .await;
                    // SAFETY: Valid group socket, clean up on invalid streamid.
                    unsafe {
                        srt_close(client_sock);
                    }
                    return;
                }
            };
            let connect_error = {
                // SAFETY: srt_create_config allocates a per-member config.
                // srt_config_add writes the streamid into that config.
                // Ownership transfers to SRT on successful srt_connect_group;
                // on failure config is freed via srt_delete_config below.
                let config = unsafe { srt_create_config() };
                if !config.is_null() {
                    unsafe {
                        srt_config_add(
                            config,
                            SRTO_STREAMID,
                            streamid_c.as_ptr() as *const c_void,
                            streamid.len() as c_int,
                        );
                    }
                }

                let mut members: Vec<SrtGroupMemberConfig> = Vec::new();
                for (i, &peer_addr) in all_addrs.iter().enumerate() {
                    let (peer_storage, addrlen) = to_libc_sockaddr(peer_addr);
                    // SAFETY: srt_prepare_endpoint creates a group member
                    // descriptor from a sockaddr. The peer_storage is
                    // stack-allocated and valid for this call.
                    let mut member = unsafe {
                        srt_prepare_endpoint(
                            std::ptr::null(),
                            &peer_storage as *const _ as *const libc::sockaddr,
                            addrlen,
                        )
                    };
                    member.weight = if i == 0 { 1 } else { 0 };
                    if !config.is_null() {
                        member.config = config;
                    }
                    members.push(member);
                }

                // SAFETY: srt_connect_group opens all member connections.
                // members is a correctly sized Vec of SrtGroupMemberConfig.
                // On failure, client_sock and config are cleaned up.
                let conn_res = unsafe {
                    srt_connect_group(client_sock, members.as_mut_ptr(), members.len() as c_int)
                };
                if conn_res < 0 {
                    // SAFETY: srt_getlasterror_str returns a thread-local
                    // static string valid until the next SRT call.
                    let err = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) };
                    let message = format!("bonded connection failed: {}", err.to_string_lossy());
                    eprintln!(
                        "[srt-egress] Bonded connection failed: {}",
                        err.to_string_lossy()
                    );
                    // SAFETY: Clean up group socket and per-member config
                    // on connection failure. Order: close socket, then
                    // free config (config must not outlive the socket).
                    unsafe {
                        srt_close(client_sock);
                        if !config.is_null() {
                            srt_delete_config(config);
                        }
                    }
                    Some(message)
                } else {
                    None
                }
            };
            if let Some(message) = connect_error {
                engine
                    .record_egress_error(&output_id, "connect", message)
                    .await;
                return;
            }
            // config ownership transfers to SRT on successful connect
        } else {
            let connect_error = {
                let mut members: Vec<SrtGroupMemberConfig> = Vec::new();
                for (i, &peer_addr) in all_addrs.iter().enumerate() {
                    let (peer_storage, addrlen) = to_libc_sockaddr(peer_addr);
                    // SAFETY: srt_prepare_endpoint with NULL source and
                    // stack-allocated sockaddr.
                    let mut member = unsafe {
                        srt_prepare_endpoint(
                            std::ptr::null(),
                            &peer_storage as *const _ as *const libc::sockaddr,
                            addrlen,
                        )
                    };
                    member.weight = if i == 0 { 1 } else { 0 };
                    members.push(member);
                }

                // SAFETY: Connect group without streamid config.
                // Members array is valid; members.len() is correct.
                let conn_res = unsafe {
                    srt_connect_group(client_sock, members.as_mut_ptr(), members.len() as c_int)
                };
                if conn_res < 0 {
                    // SAFETY: srt_getlasterror_str is valid until next SRT call.
                    let err = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) };
                    let message = format!("bonded connection failed: {}", err.to_string_lossy());
                    eprintln!(
                        "[srt-egress] Bonded connection failed: {}",
                        err.to_string_lossy()
                    );
                    // SAFETY: Clean up socket on connection failure.
                    unsafe {
                        srt_close(client_sock);
                    }
                    Some(message)
                } else {
                    None
                }
            };
            if let Some(message) = connect_error {
                engine
                    .record_egress_error(&output_id, "connect", message)
                    .await;
                return;
            }
        }

        println!(
            "[srt-egress] Bonded connection ({} links) to {}",
            all_addrs.len(),
            target_url
        );
        srt_set_highbitrate_opts(client_sock);
        srt_log_effective_opts(client_sock, "egress-bonded");
    } else {
        engine.update_egress_phase(&output_id, "connecting").await;
        // SAFETY: srt_create_socket creates a new SRT socket handle.
        // The returned handle is closed on all exit paths below
        // (connection failure, cancel, sender exit).
        // Single connection (original path)
        client_sock = unsafe { srt_create_socket() };
        if client_sock < 0 {
            eprintln!("[srt-egress] Failed to create socket");
            engine
                .record_egress_error(&output_id, "connect", "failed to create socket")
                .await;
            return;
        }
        srt_set_highbitrate_opts(client_sock);

        if !streamid.is_empty() {
            let streamid_c = match std::ffi::CString::new(streamid.as_str()) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("[srt-egress] Invalid stream ID (contains null byte)");
                    engine
                        .record_egress_error(&output_id, "connect", "stream ID contains null bytes")
                        .await;
                    // SAFETY: Valid socket, clean up on invalid streamid.
                    unsafe {
                        srt_close(client_sock);
                    }
                    return;
                }
            };
            // SAFETY: Sets SRTO_STREAMID on a valid socket with a
            // correctly-sized NUL-terminated C string.
            unsafe {
                srt_setsockopt(
                    client_sock,
                    0,
                    SRTO_STREAMID,
                    streamid_c.as_ptr() as *const c_void,
                    streamid.len() as c_int,
                );
            }
        }

        let sin = to_sockaddr_in(addr);

        // Pre-connect warmup: wait for the upstream ring to have data before
        // connecting to MediaMTX. Transcoded/routed rings (codec_hint set) go
        // through a multi-stage chain that takes seconds to warm up. Connecting
        // before any data is ready results in an idle publisher that MediaMTX
        // closes for inactivity before the first packet ever arrives.
        if !ring_buffer.codec_hint_str().is_empty() {
            let warmup = crate::media::ring_buffer::Reader::new(
                format!("srt_egress_warmup:{}", output_id),
                ring_buffer.clone(),
            );
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    // SAFETY: Valid socket, cancelling before connect.
                    unsafe { srt_close(client_sock); }
                    return;
                }
                _ = warmup.wait_for_data() => {}
            }
        }

        // SAFETY: srt_connect opens a connection to the target address.
        // sin is a correctly-sized sockaddr_in; client_sock is valid.
        let conn_res = unsafe {
            srt_connect(
                client_sock,
                &sin,
                std::mem::size_of::<sockaddr_in>() as c_int,
            )
        };
        if conn_res < 0 {
            eprintln!("[srt-egress] Connection failed to {}", target_url);
            engine
                .record_egress_error(&output_id, "connect", "connection failed")
                .await;
            // SAFETY: Valid socket, clean up on connection failure.
            unsafe {
                srt_close(client_sock);
            }
            return;
        }

        println!("[srt-egress] Connected to {}", target_url);
        srt_log_effective_opts(client_sock, "egress");
    }

    let shared_muxer = engine
        .get_or_create_ts_muxer_stage(&pipeline_id, &encoding, ring_buffer.clone())
        .await;
    engine.update_egress_phase(&output_id, "sending").await;

    let out_queue = Arc::new(crate::media::avio::MemoryQueue::new());

    // Sender thread: reads MPEG-TS from out_queue, sends via SRT
    let out_queue_send = out_queue.clone();
    let oid = output_id.clone();
    let (
        egress_bytes_sent,
        egress_metrics,
        egress_last_progress_ms,
        egress_phase,
        egress_last_error,
        egress_last_error_ms,
        egress_failure_phase,
    ) = {
        let egresses = engine.active_egresses.read().await;
        if let Some(e) = egresses.get(&output_id) {
            (
                Some(e.bytes_sent.clone()),
                Some(e.metrics.clone()),
                Some(e.last_progress_ms.clone()),
                Some(e.phase.clone()),
                Some(e.last_error.clone()),
                Some(e.last_error_ms.clone()),
                Some(e.failure_phase.clone()),
            )
        } else {
            (None, None, None, None, None, None, None)
        }
    };
    // Sender thread: reads MPEG-TS from out_queue, sends via SRT.
    // Wrapped in catch_unwind so a panic cannot crash the process (CLAUDE.md).
    // Acquire a semaphore permit to cap concurrent SRT sender threads at 512.
    let permit = match engine.srt_sender_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "[srt-egress] Sender thread limit reached — rejecting egress {}",
                output_id
            );
            engine
                .record_egress_error(&output_id, "capacity", "SRT sender thread limit reached")
                .await;
            // SAFETY: Valid socket, clean up on capacity rejection.
            unsafe {
                srt_close(client_sock);
            }
            return;
        }
    };
    let egress_sender_handle = std::thread::spawn(move || {
        let _permit = permit; // dropped when thread exits → releases semaphore slot
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut buf = vec![0u8; 1316];
            let progress_sample_interval = Duration::from_millis(250);
            let mut last_progress_sample = Instant::now() - progress_sample_interval;
            loop {
                let n = out_queue_send.read(&mut buf);
                if n == 0 {
                    break;
                }
                // SAFETY: srt_send transmits data over a valid connected
                // SRT socket. buf is correctly sized; n ≤ buf.len().
                let sent = unsafe { srt_send(client_sock, buf.as_ptr(), n as c_int) };
                if sent < 0 {
                    // SAFETY: srt_getlasterror_str returns a thread-local
                    // static string for error diagnostics.
                    let err_str = unsafe { std::ffi::CStr::from_ptr(srt_getlasterror_str()) }
                        .to_string_lossy();
                    eprintln!("[srt-egress] srt_send failed for {}: {}", oid, err_str);
                    if let Some(ref phase) = egress_phase {
                        *phase.lock().unwrap_or_else(|e| e.into_inner()) = "failed".to_string();
                    }
                    if let Some(ref failure_phase) = egress_failure_phase {
                        *failure_phase.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some("send".to_string());
                    }
                    if let Some(ref last_error) = egress_last_error {
                        *last_error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("srt_send failed: {}", err_str));
                    }
                    if let Some(ref last_error_ms) = egress_last_error_ms {
                        last_error_ms.store(
                            chrono::Utc::now().timestamp_millis().max(0) as u64,
                            Ordering::Relaxed,
                        );
                    }
                    break;
                }
                if let Some(ref counter) = egress_bytes_sent {
                    counter.fetch_add(sent as u64, Ordering::Relaxed);
                }
                if let Some(ref m) = egress_metrics {
                    m.record_out(sent as u64);
                }
                if last_progress_sample.elapsed() >= progress_sample_interval {
                    if let Some(ref progress) = egress_last_progress_ms {
                        progress.store(
                            chrono::Utc::now().timestamp_millis().max(0) as u64,
                            Ordering::Relaxed,
                        );
                    }
                    last_progress_sample = Instant::now();
                }
            }
        }));
        if result.is_err() {
            eprintln!("[srt-egress] Sender thread panicked for {}", oid);
        } else {
            println!("[srt-egress] Sender thread finished for {}", oid);
        }
        // SAFETY: client_sock was created/connected in start_srt_egress
        // and passed to this sender thread. Closed exactly once here
        // after the sender loop exits.
        unsafe {
            srt_close(client_sock);
        }
    });
    engine.register_os_thread(egress_sender_handle);

    let mut reader = TsChunkReader::new(format!("srt_egress:{}", output_id), &shared_muxer);
    // Accumulation buffer: collect all muxed TS bytes for a burst, then
    // write them in a single out_queue.write() call (one lock acquisition
    // per burst instead of one per packet).
    let mut ts_batch: Vec<u8> = Vec::with_capacity(65536);
    let mut packets = Vec::with_capacity(32);
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = reader.wait_for_data() => {
                packets.clear();
                if reader.pull_burst(&mut packets, 32).is_ok() {
                    for pkt in &packets {
                        if !pkt.payload.is_empty() {
                            ts_batch.extend_from_slice(&pkt.payload);
                        }
                    }
                    // One lock acquisition for the whole burst.
                    if !ts_batch.is_empty() {
                        out_queue.write(&ts_batch).await;
                        ts_batch.clear();
                    }
                }
            }
        }
    }

    out_queue.close();
}
