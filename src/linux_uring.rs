//! io_uring-backed UDP send path. Default ON on Linux; set `ST_IO_URING=0`
//! to force the `sendmmsg` fallback for debugging. When enabled, we submit
//! a batch of `sendmsg` SQEs per frame and wait for completions
//! synchronously so the buffer ownership matches the existing `sendmmsg`
//! semantics. On init failure or unsupported kernels, callers fall back to
//! the `sendmmsg`-backed `SendBatch` path that already exists.

use std::io;
use std::os::fd::RawFd;

use io_uring::{opcode, types, IoUring};

/// Default-on on Linux. `ST_IO_URING=0` (or `false`/`no`/`off`) is the
/// escape hatch that forces the `sendmmsg` fallback. See
/// `client/src/linux_uring.rs` for the full bug-fix history that cleared
/// the default-on bar; the server side stays symmetric with the client so
/// both halves of a session take the same path.
pub fn io_uring_requested() -> bool {
    match std::env::var("ST_IO_URING").ok().as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => true,
    }
}

pub struct UringSend {
    ring: IoUring,
    iovecs: Vec<libc::iovec>,
    hdrs: Vec<libc::msghdr>,
}

// msghdr holds raw pointers back into the caller's packet slices; the ring is
// owned by a single sender thread, so sending it between threads is safe.
unsafe impl Send for UringSend {}

impl UringSend {
    /// Build an io_uring suitable for batched sendmsg. Returns None on any
    /// kernel-level unavailability — caller must fall back to `sendmmsg`.
    pub fn new() -> Option<Self> {
        let ring = IoUring::builder().build(128).ok()?;
        Some(Self {
            ring,
            iovecs: Vec::with_capacity(64),
            hdrs: Vec::with_capacity(64),
        })
    }

    /// Submit all packets, wait for every completion, and report the first
    /// error observed (if any). The buffers are read by the kernel during the
    /// syscall and must remain valid until we return; since we wait for every
    /// CQE before returning, the slices `packets` lends us are safe.
    pub fn send_all(&mut self, fd: RawFd, packets: &[&[u8]]) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }

        // Rebuild iovec + msghdr lists stably. We reserve capacity up front so
        // Vec won't realloc mid-build (which would invalidate the pointers we
        // handed to the SQEs).
        self.iovecs.clear();
        self.hdrs.clear();
        self.iovecs.reserve(packets.len());
        self.hdrs.reserve(packets.len());

        for pkt in packets {
            self.iovecs.push(libc::iovec {
                iov_base: pkt.as_ptr() as *mut libc::c_void,
                iov_len: pkt.len(),
            });
        }

        let iov_base_ptr = self.iovecs.as_mut_ptr();
        for i in 0..packets.len() {
            let iov_ptr = unsafe { iov_base_ptr.add(i) };
            self.hdrs.push(libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: iov_ptr,
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            });
        }

        // Submit SQEs in chunks sized to the ring capacity. We track how many
        // SQEs we *actually* pushed each iteration, not the target chunk size.
        // That way an SQ-full condition can't cause us to wait for CQEs that
        // will never arrive.
        let total = packets.len();
        let mut submitted = 0usize;
        while submitted < total {
            let chunk_end = (submitted + 64).min(total);
            let mut pushed = 0usize;
            {
                let mut sq = self.ring.submission();
                for i in submitted..chunk_end {
                    let hdr_ptr = self.hdrs.as_ptr().wrapping_add(i) as *const libc::msghdr;
                    let sqe = opcode::SendMsg::new(types::Fd(fd), hdr_ptr)
                        .build()
                        .user_data(i as u64);
                    if unsafe { sq.push(&sqe).is_err() } {
                        break;
                    }
                    pushed += 1;
                }
            }
            if pushed == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "uring SQ full; could not push any SendMsg SQE",
                ));
            }

            self.ring
                .submit_and_wait(pushed)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("uring submit: {e}")))?;

            // Drain CQEs for this chunk. `submit_and_wait(pushed)` guarantees
            // at least `pushed` completions; we drain everything the ring has.
            let mut got = 0usize;
            let mut first_err: Option<io::Error> = None;
            {
                let mut cq = self.ring.completion();
                cq.sync();
                while let Some(cqe) = cq.next() {
                    got += 1;
                    let result = cqe.result();
                    if result < 0 && first_err.is_none() {
                        first_err = Some(io::Error::from_raw_os_error(-result));
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            if got < pushed {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("uring got {got} CQEs, expected at least {pushed}"),
                ));
            }
            submitted += pushed;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;

    /// Smoke-test: `UringSend` transmits a single packet to a localhost peer
    /// and the peer can read it back identically. Exercises the msghdr/iovec
    /// layout and the synchronous submit_and_wait path.
    #[test]
    fn uring_send_delivers_single_packet() {
        let Some(mut uring) = UringSend::new() else {
            eprintln!("io_uring unavailable on this kernel; skipping");
            return;
        };

        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let recv_addr = recv.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        send.connect(recv_addr).expect("connect");
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

        let payload = b"io_uring correctness check";
        uring
            .send_all(send.as_raw_fd(), &[&payload[..]])
            .expect("uring send_all");

        let mut buf = [0u8; 64];
        let (n, _) = recv.recv_from(&mut buf).expect("recv");
        assert_eq!(&buf[..n], payload);
    }

    /// Verify that a batch larger than a single chunk (64) completes
    /// correctly and all packets arrive.
    #[test]
    fn uring_send_handles_large_batch() {
        let Some(mut uring) = UringSend::new() else {
            return;
        };
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind");
        send.connect(recv_addr).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();

        let packets: Vec<Vec<u8>> = (0..128u32)
            .map(|i| i.to_be_bytes().to_vec())
            .collect();
        let refs: Vec<&[u8]> = packets.iter().map(|v| v.as_slice()).collect();
        uring.send_all(send.as_raw_fd(), &refs).expect("send_all");

        let mut seen = std::collections::HashSet::new();
        let mut buf = [0u8; 32];
        for _ in 0..packets.len() {
            let (n, _) = recv.recv_from(&mut buf).expect("recv");
            assert_eq!(n, 4);
            let idx = u32::from_be_bytes(buf[..4].try_into().unwrap());
            seen.insert(idx);
        }
        assert_eq!(seen.len(), 128);
    }

    /// Production-shape byte-integrity test: full-MTU packets (1400 bytes)
    /// containing a distinguishable per-packet pattern, batched larger than
    /// a chunk boundary, and verified byte-for-byte on the receive side.
    /// Previous tests only used 4-byte payloads, which would not catch any
    /// corruption that only shows up on longer iovec reads (e.g. if msg_iov
    /// pointers became stale mid-chunk).
    #[test]
    fn uring_send_preserves_full_mtu_payloads() {
        let Some(mut uring) = UringSend::new() else {
            return;
        };
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind");
        send.connect(recv_addr).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();

        // Must exceed recv socket buffer if we send blindly, so bump SO_RCVBUF
        // on the receiver to reduce the chance of kernel drops making the
        // test flaky. A real client tunes this to ~1 MiB.
        use std::os::fd::AsRawFd;
        let rcvbuf: libc::c_int = 4 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        const N: u32 = 200;
        const LEN: usize = 1400;
        let packets: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut v = vec![0u8; LEN];
                v[..4].copy_from_slice(&i.to_be_bytes());
                // Fill remainder with an index-derived pattern so any
                // cross-packet trampling shows up as a byte mismatch.
                for (j, b) in v[4..].iter_mut().enumerate() {
                    *b = ((i.wrapping_mul(31) as usize + j) & 0xff) as u8;
                }
                v
            })
            .collect();
        let refs: Vec<&[u8]> = packets.iter().map(|v| v.as_slice()).collect();
        uring.send_all(send.as_raw_fd(), &refs).expect("send_all");

        let mut seen: Vec<Option<Vec<u8>>> = (0..N).map(|_| None).collect();
        let mut buf = vec![0u8; LEN + 32];
        for _ in 0..N {
            let (n, _) = recv.recv_from(&mut buf).expect("recv");
            assert_eq!(n, LEN, "every packet must land at exactly its send length");
            let idx = u32::from_be_bytes(buf[..4].try_into().unwrap());
            assert!(idx < N, "corrupt index byte in payload");
            seen[idx as usize] = Some(buf[..n].to_vec());
        }
        for (i, got) in seen.iter().enumerate() {
            let got = got.as_ref().expect("every index must be received");
            assert_eq!(
                got,
                &packets[i],
                "packet {i} arrived with corrupted bytes — uring send is trampling buffers"
            );
        }
    }

    /// Two-batch sequence: first a large send that forces iovec/hdrs Vec to
    /// grow, then a smaller send that reuses the grown buffers. This checks
    /// that the second call's msg_iov pointers correctly point at the reset
    /// iovec slots rather than any stale pointer from the earlier batch.
    #[test]
    fn uring_send_handles_shrinking_batch_after_grow() {
        let Some(mut uring) = UringSend::new() else {
            return;
        };
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind");
        send.connect(recv_addr).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let rcvbuf: libc::c_int = 8 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        // First batch: 150 packets, 1400 bytes each — forces internal Vec
        // capacity past the initial 64.
        let first: Vec<Vec<u8>> = (0..150u32)
            .map(|i| {
                let mut v = vec![0u8; 1400];
                v[..4].copy_from_slice(&i.to_be_bytes());
                v[4] = 0xAA;
                v
            })
            .collect();
        let refs: Vec<&[u8]> = first.iter().map(|v| v.as_slice()).collect();
        uring.send_all(send.as_raw_fd(), &refs).expect("first send");
        let mut buf = vec![0u8; 2000];
        for _ in 0..first.len() {
            let (n, _) = recv.recv_from(&mut buf).expect("recv first");
            assert_eq!(n, 1400);
            assert_eq!(buf[4], 0xAA);
        }

        // Second batch: 10 packets with a different magic byte — bytes must
        // not contain any 0xAA at offset 4 if the pointer graph was rebuilt
        // correctly.
        let second: Vec<Vec<u8>> = (0..10u32)
            .map(|i| {
                let mut v = vec![0u8; 800];
                v[..4].copy_from_slice(&i.to_be_bytes());
                v[4] = 0x55;
                v
            })
            .collect();
        let refs: Vec<&[u8]> = second.iter().map(|v| v.as_slice()).collect();
        uring.send_all(send.as_raw_fd(), &refs).expect("second send");
        for _ in 0..second.len() {
            let (n, _) = recv.recv_from(&mut buf).expect("recv second");
            assert_eq!(n, 800);
            assert_eq!(buf[4], 0x55, "stale iovec/hdr pointer from first batch");
        }
    }
}
