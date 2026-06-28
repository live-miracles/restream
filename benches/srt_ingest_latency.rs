use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use restream::media::srt::*;
use std::ffi::c_int;
use std::time::{Duration, Instant};

const BENCH_LATENCY_MS: c_int = 20;
const BENCH_PBKEYLEN_BYTES: c_int = 16;
const BENCH_PASSPHRASE: &str = "benchpass12";
const SRT_LIVE_PAYLOAD_BYTES: usize = 1316;
const PACKETS_PER_ITER: &[usize] = &[
    1,  // one MPEG-TS-over-SRT payload
    8,  // small burst
    64, // larger steady-state transfer
];

#[derive(Clone, Copy)]
enum CryptoMode {
    Plain,
    Encrypted,
}

impl CryptoMode {
    fn label(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Encrypted => "encrypted",
        }
    }
}

fn make_addr(port: u16) -> sockaddr_in {
    sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: u32::to_be(0x7f000001),
        sin_zero: [0; 8],
    }
}

fn os_port(sock: SRTSOCKET) -> u16 {
    let mut name = unsafe { std::mem::zeroed::<sockaddr_in>() };
    let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
    unsafe {
        srt_getsockname(sock, &mut name, &mut len);
    }
    u16::from_be(name.sin_port)
}

fn srt_error() -> String {
    let p = unsafe { srt_getlasterror_str() };
    if p.is_null() {
        "unknown".into()
    } else {
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into()
    }
}

fn set_latency(sock: SRTSOCKET) {
    let ret = unsafe {
        srt_setsockopt(
            sock,
            0,
            SRTO_LATENCY,
            &BENCH_LATENCY_MS as *const _ as *const _,
            std::mem::size_of::<c_int>() as c_int,
        )
    };
    assert_eq!(ret, 0, "set latency: {}", srt_error());
}

fn set_encryption(sock: SRTSOCKET) {
    let yes: bool = true;
    let passphrase = BENCH_PASSPHRASE.as_bytes();
    let ret = unsafe {
        srt_setsockopt(
            sock,
            0,
            SRTO_ENFORCEDENCRYPTION,
            &yes as *const _ as *const _,
            std::mem::size_of::<bool>() as c_int,
        )
    };
    assert_eq!(ret, 0, "set enforced encryption: {}", srt_error());

    let ret = unsafe {
        srt_setsockopt(
            sock,
            0,
            SRTO_PBKEYLEN,
            &BENCH_PBKEYLEN_BYTES as *const _ as *const _,
            std::mem::size_of::<c_int>() as c_int,
        )
    };
    assert_eq!(ret, 0, "set pbkeylen: {}", srt_error());

    let ret = unsafe {
        srt_setsockopt(
            sock,
            0,
            SRTO_PASSPHRASE,
            passphrase.as_ptr() as *const _,
            passphrase.len() as c_int,
        )
    };
    assert_eq!(ret, 0, "set passphrase: {}", srt_error());
}

fn configure_socket(sock: SRTSOCKET, crypto: CryptoMode) {
    set_latency(sock);
    if matches!(crypto, CryptoMode::Encrypted) {
        set_encryption(sock);
    }
}

fn make_srt_pair(crypto: CryptoMode) -> (SRTSOCKET, SRTSOCKET) {
    let listener = unsafe { srt_create_socket() };
    assert!(listener >= 0, "listener: {}", srt_error());
    configure_socket(listener, crypto);

    let addr = make_addr(0);
    assert_eq!(
        unsafe { srt_bind(listener, &addr, std::mem::size_of::<sockaddr_in>() as c_int) },
        0,
        "bind: {}",
        srt_error()
    );
    assert_eq!(
        unsafe { srt_listen(listener, 1) },
        0,
        "listen: {}",
        srt_error()
    );

    let actual_port = os_port(listener);

    let connect_thread = std::thread::spawn(move || {
        let sender = unsafe { srt_create_socket() };
        assert!(sender >= 0, "sender: {}", srt_error());
        configure_socket(sender, crypto);
        let dst = make_addr(actual_port);
        let ret = unsafe { srt_connect(sender, &dst, std::mem::size_of::<sockaddr_in>() as c_int) };
        assert_eq!(ret, 0, "connect: {}", srt_error());
        sender
    });

    let mut client_sin = unsafe { std::mem::zeroed::<sockaddr_in>() };
    let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
    let receiver = unsafe { srt_accept(listener, &mut client_sin, &mut len) };
    assert!(receiver >= 0, "accept: {}", srt_error());

    unsafe {
        srt_close(listener);
    }
    let sender = connect_thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    (sender, receiver)
}

fn bench_ingest(c: &mut Criterion, payload: &[u8], packets_per_iter: usize, crypto: CryptoMode) {
    let mut group = c.benchmark_group(format!("srt_ingest/{}", crypto.label()));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(30);
    group.throughput(Throughput::Bytes(
        (payload.len() * packets_per_iter) as u64,
    ));

    group.bench_with_input(
        BenchmarkId::new(
            "recv_path",
            format!("{}pkts_{}b", packets_per_iter, payload.len() * packets_per_iter),
        ),
        &packets_per_iter,
        |b, _| {
        b.iter_custom(|iters| {
            let (sender, receiver) = make_srt_pair(crypto);
            let send_payload = payload.to_vec();
            let sender_thread = std::thread::spawn(move || {
                for _ in 0..iters {
                    for _ in 0..packets_per_iter {
                        let n = unsafe {
                            srt_send(sender, send_payload.as_ptr(), send_payload.len() as c_int)
                        };
                        assert_eq!(n, send_payload.len() as c_int, "send: {}", srt_error());
                    }
                }
                sender
            });

            let mut recv_buf = vec![0u8; payload.len()];
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..packets_per_iter {
                    let n = unsafe {
                        srt_recv(receiver, recv_buf.as_mut_ptr(), recv_buf.len() as c_int)
                    };
                    assert_eq!(n, recv_buf.len() as c_int, "recv: {}", srt_error());
                    black_box(n);
                }
            }
                let elapsed = start.elapsed();

                let sender = sender_thread.join().unwrap();
                unsafe {
                    srt_close(sender);
                    srt_close(receiver);
                }
                elapsed
            })
        },
    );

    group.finish();
}

fn bench_egress(c: &mut Criterion, payload: &[u8], packets_per_iter: usize, crypto: CryptoMode) {
    let mut group = c.benchmark_group(format!("srt_egress/{}", crypto.label()));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(30);
    group.throughput(Throughput::Bytes(
        (payload.len() * packets_per_iter) as u64,
    ));

    group.bench_with_input(
        BenchmarkId::new(
            "send_path",
            format!("{}pkts_{}b", packets_per_iter, payload.len() * packets_per_iter),
        ),
        &packets_per_iter,
        |b, _| {
        b.iter_custom(|iters| {
            let (sender, receiver) = make_srt_pair(crypto);
            let recv_len = payload.len();
            let receiver_thread = std::thread::spawn(move || {
                let mut recv_buf = vec![0u8; recv_len];
                for _ in 0..iters {
                    for _ in 0..packets_per_iter {
                        let n = unsafe {
                            srt_recv(receiver, recv_buf.as_mut_ptr(), recv_buf.len() as c_int)
                        };
                        assert_eq!(n, recv_buf.len() as c_int, "recv: {}", srt_error());
                    }
                }
                receiver
            });

            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..packets_per_iter {
                    let n = unsafe { srt_send(sender, payload.as_ptr(), payload.len() as c_int) };
                    assert_eq!(n, payload.len() as c_int, "send: {}", srt_error());
                    black_box(n);
                }
            }
                let elapsed = start.elapsed();

                let receiver = receiver_thread.join().unwrap();
                unsafe {
                    srt_close(sender);
                    srt_close(receiver);
                }
                elapsed
            })
        },
    );

    group.finish();
}

fn bench_srt_transport_latency(c: &mut Criterion) {
    unsafe {
        srt_startup();
    }

    for crypto in [CryptoMode::Plain, CryptoMode::Encrypted] {
        let payload = vec![0x47u8; SRT_LIVE_PAYLOAD_BYTES];
        for &packets_per_iter in PACKETS_PER_ITER {
            bench_ingest(c, &payload, packets_per_iter, crypto);
            bench_egress(c, &payload, packets_per_iter, crypto);
        }
    }

    unsafe {
        srt_cleanup();
    }
}

criterion_group!(benches, bench_srt_transport_latency);
criterion_main!(benches);
