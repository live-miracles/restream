use criterion::{black_box, criterion_group, criterion_main, Criterion};
use restream::media::srt::*;
use std::ffi::c_int;
use std::time::{Duration, Instant};

unsafe fn make_addr(port: u16) -> sockaddr_in {
    sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: u32::to_be(0x7f000001),
        sin_zero: [0; 8],
    }
}

unsafe fn os_port(sock: SRTSOCKET) -> u16 {
    let mut name = std::mem::zeroed::<sockaddr_in>();
    let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
    srt_getsockname(sock, &mut name, &mut len);
    u16::from_be(name.sin_port)
}

fn srt_error() -> String {
    let p = unsafe { srt_getlasterror_str() };
    if p.is_null() { "unknown".into() } else {
        unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into()
    }
}

unsafe fn make_srt_pair() -> (SRTSOCKET, SRTSOCKET) {
    let listener = srt_create_socket();
    assert!(listener >= 0, "listener: {}", srt_error());

    let latency: c_int = 20;
    srt_setsockopt(listener, 0, SRTO_LATENCY, &latency as *const _ as *const _,
        std::mem::size_of::<c_int>() as c_int);

    let addr = make_addr(0);
    assert_eq!(srt_bind(listener, &addr, std::mem::size_of::<sockaddr_in>() as c_int), 0,
        "bind: {}", srt_error());
    assert_eq!(srt_listen(listener, 1), 0, "listen: {}", srt_error());

    let actual_port = os_port(listener);

    let connect_thread = std::thread::spawn(move || {
        let sender = srt_create_socket();
        assert!(sender >= 0, "sender: {}", srt_error());
        let slatency: c_int = 20;
        srt_setsockopt(sender, 0, SRTO_LATENCY, &slatency as *const _ as *const _,
            std::mem::size_of::<c_int>() as c_int);
        let dst = make_addr(actual_port);
        let ret = srt_connect(sender, &dst, std::mem::size_of::<sockaddr_in>() as c_int);
        assert_eq!(ret, 0, "connect: {}", srt_error());
        sender
    });

    let mut client_sin = std::mem::zeroed::<sockaddr_in>();
    let mut len = std::mem::size_of::<sockaddr_in>() as c_int;
    let receiver = srt_accept(listener, &mut client_sin, &mut len);
    assert!(receiver >= 0, "accept: {}", srt_error());

    srt_setsockopt(receiver, 0, SRTO_LATENCY, &latency as *const _ as *const _,
        std::mem::size_of::<c_int>() as c_int);

    srt_close(listener);
    let sender = connect_thread.join().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    (sender, receiver)
}

fn bench_srt_ingest_latency(c: &mut Criterion) {
    unsafe { srt_startup(); }

    let payload = vec![0x47u8; 1316];
    let mut buf = vec![0u8; 1316];

    let mut group = c.benchmark_group("srt_ingest_latency");
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(50);
    group.throughput(criterion::Throughput::Bytes(1316));

    group.bench_function("blocking_hot", |b| {
        b.iter_custom(|iters| {
            let (sender, receiver) = unsafe { make_srt_pair() };
            let start = Instant::now();
            for _ in 0..iters {
                unsafe { srt_send(sender, payload.as_ptr(), payload.len() as c_int) };
                let n = unsafe { srt_recv(receiver, buf.as_mut_ptr(), buf.len() as c_int) };
                assert!(n > 0, "recv: {}", srt_error());
                black_box(n);
            }
            let elapsed = start.elapsed();
            unsafe { srt_close(sender); srt_close(receiver); }
            elapsed
        });
    });

    group.bench_function("polling_1ms", |b| {
        b.iter_custom(|iters| {
            let (sender, receiver) = unsafe { make_srt_pair() };
            let zero: c_int = 0;
            unsafe {
                srt_setsockopt(receiver, 0, SRTO_RCVSYN, &zero as *const _ as *const _,
                    std::mem::size_of::<c_int>() as c_int);
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time().build().unwrap();
            let start = Instant::now();
            rt.block_on(async {
                for _ in 0..iters {
                    unsafe { srt_send(sender, payload.as_ptr(), payload.len() as c_int) };
                    loop {
                        let n = unsafe { srt_recv(receiver, buf.as_mut_ptr(), buf.len() as c_int) };
                        if n > 0 { black_box(n); break; }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            });
            let elapsed = start.elapsed();
            drop(rt);
            unsafe { srt_close(sender); srt_close(receiver); }
            elapsed
        });
    });

    group.bench_function("epoll_spawn", |b| {
        b.iter_custom(|iters| {
            let (sender, receiver) = unsafe { make_srt_pair() };
            let zero: c_int = 0;
            unsafe {
                srt_setsockopt(receiver, 0, SRTO_RCVSYN, &zero as *const _ as *const _,
                    std::mem::size_of::<c_int>() as c_int);
            }
            let eid = unsafe { srt_epoll_create() };
            assert!(eid >= 0);
            let events = 0x1i32;
            assert_eq!(unsafe { srt_epoll_add_usock(eid, receiver, &events) }, 0);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time().build().unwrap();
            let start = Instant::now();
            rt.block_on(async {
                for _ in 0..iters {
                    unsafe { srt_send(sender, payload.as_ptr(), payload.len() as c_int) };
                    loop {
                        let n = unsafe { srt_recv(receiver, buf.as_mut_ptr(), buf.len() as c_int) };
                        if n > 0 { black_box(n); break; }
                        let _ = tokio::task::spawn_blocking(move || {
                            let mut rd = [0i32; 1];
                            let mut rn = 1i32;
                            unsafe {
                                srt_epoll_wait(eid, rd.as_mut_ptr(), &mut rn,
                                    std::ptr::null_mut(), std::ptr::null_mut(), -1,
                                    std::ptr::null_mut(), std::ptr::null_mut(),
                                    std::ptr::null_mut(), std::ptr::null_mut());
                            };
                        }).await;
                    }
                }
            });
            let elapsed = start.elapsed();
            drop(rt);
            unsafe { srt_epoll_release(eid); srt_close(sender); srt_close(receiver); }
            elapsed
        });
    });

    group.finish();
    unsafe { srt_cleanup(); }
}

criterion_group!(benches, bench_srt_ingest_latency);
criterion_main!(benches);
