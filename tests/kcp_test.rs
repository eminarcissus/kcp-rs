extern crate kcp;
extern crate time as ctime;
extern crate rand;

use std::collections::VecDeque;
use std::io;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;
use rand::Rng;

use kcp::KCP;

#[inline]
fn clock() -> u32 {
    let timespec = ctime::get_time();
    let mills = timespec.sec * 1000 + timespec.nsec as i64 / 1000 / 1000;
    mills as u32
}

#[derive(Default)]
struct DelayPacket {
    data: Vec<u8>,
    ts: u32,
}

impl DelayPacket {
    fn new() -> DelayPacket {
        Default::default()
    }
}

#[derive(Default)]
struct LatencySimulator {
    tx: u32,
    current: u32,
    lost_rate: u32,
    rtt_min: u32,
    rtt_max: u32,
    nmax: u32,
    delay_tunnel: VecDeque<DelayPacket>,
}

impl LatencySimulator {
    fn new(lost_rate: u32, rtt_min: u32, rtt_max: u32, nmax: u32) -> LatencySimulator {
        LatencySimulator {
            current: clock(),
            lost_rate: lost_rate / 2,
            rtt_min: rtt_min / 2,
            rtt_max: rtt_max / 2,
            nmax: nmax,
            ..Default::default()
        }
    }
}

impl Write for LatencySimulator {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx += 1;
        if rand::thread_rng().gen_range(0, 100) < self.lost_rate {
            return Err(io::Error::new(io::ErrorKind::Other, "lost"));
        }
        if self.delay_tunnel.len() >= self.nmax as usize {
            return Err(io::Error::new(io::ErrorKind::Other,
                                      format!("exceeded nmax: {}", self.delay_tunnel.len())));
        }
        let mut pkt = DelayPacket::new();
        self.current = clock();
        let mut delay = self.rtt_min;
        if self.rtt_max > self.rtt_min {
            delay += rand::random::<u32>() % (self.rtt_max - self.rtt_min);
        }
        pkt.ts = self.current + delay;
        pkt.data.extend_from_slice(buf);
        self.delay_tunnel.push_back(pkt);

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for LatencySimulator {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len: usize;
        if let Some(pkt) = self.delay_tunnel.front() {
            self.current = clock();
            if self.current < pkt.ts {
                return Err(io::Error::new(io::ErrorKind::Other,
                                          format!("current({}) < ts({})", self.current, pkt.ts)));
            }
            len = pkt.data.len();
            if len > buf.len() {
                return Err(io::Error::new(io::ErrorKind::Other,
                                          format!("buf_size({}) < pkt_size({})", buf.len(), len)));
            }
            let buf = &mut buf[..len];
            buf.copy_from_slice(&pkt.data[..]);
        } else {
            return Err(io::Error::new(io::ErrorKind::Other, "empty"));
        }

        self.delay_tunnel.pop_front();
        Ok(len)
    }
}

#[test]
fn kcp_test() {
    test("default");
    // test("normal");
    test("fast");
}

fn test(mode: &str) {
    let alice_to_bob = Arc::new(Mutex::new(LatencySimulator::new(10, 60, 125, 1000)));
    let bob_to_alice = Arc::new(Mutex::new(LatencySimulator::new(10, 60, 125, 1000)));

    let alice_sends = alice_to_bob.clone();
    let bob_sends = bob_to_alice.clone();
    let mut alice = KCP::new(0x11223344, alice_sends);
    let mut bob = KCP::new(0x11223344, bob_sends);

    let mut current = clock();
    let mut slap = current + 20;
    let mut index: u32 = 0;
    let mut next: u32 = 0;
    let mut sumrtt: i64 = 0;
    let mut count = 0;
    let mut maxrtt = 0;

    alice.wndsize(128, 128);
    bob.wndsize(128, 128);

    match mode {
        "default" => {
            alice.nodelay(0, 10, 0, 0);
            bob.nodelay(0, 10, 0, 0);
        }
        "normal" => {
            alice.nodelay(0, 10, 0, 1);
            bob.nodelay(0, 10, 0, 1);
        }
        "fast" => {
            alice.nodelay(1, 10, 2, 1);
            bob.nodelay(1, 10, 2, 1);
            alice.nodelay_ex(10, 1);
        }
        _ => {}
    };

    let mut buffer: [u8; 2000] = [0; 2000];
    let mut ts1 = clock();

    loop {
        thread::sleep(time::Duration::from_millis(1));
        current = clock();

        alice.update(clock());
        bob.update(clock());

        while current >= slap {
            let mut p: usize = 0;
            encode32u(&mut buffer[..], &mut p, index);
            index += 1;
            encode32u(&mut buffer[..], &mut p, current);
            alice.send(&buffer[..8]);
            slap += 20;
        }

        let a2b = alice_to_bob.clone();
        let mut a2b = a2b.lock().unwrap();

        let b2a = bob_to_alice.clone();
        let mut b2a = b2a.lock().unwrap();

        loop {
            match a2b.read(&mut buffer[..]) {
                Ok(hr) => {
                    bob.input(&buffer[..hr]);
                }
                Err(e) => break,
            };
        }

        loop {
            match b2a.read(&mut buffer[..]) {
                Ok(hr) => {
                    alice.input(&buffer[..hr]);
                }
                Err(e) => break,
            }
        }

        loop {
            match bob.recv(&mut buffer[..10]) {
                Ok(hr) => {
                    bob.send(&buffer[..hr]);
                }
                Err(e) => break,
            }
        }

        loop {
            match alice.recv(&mut buffer[..10]) {
                Ok(hr) => {
                    let mut p: usize = 0;
                    let sn = decode32u(&buffer, &mut p);
                    let ts = decode32u(&buffer, &mut p);
                    let rtt = current - ts;
                    if sn != next {
                        println!("ERROR sn {}<->{}", count, next);
                        return;
                    }
                    next += 1;
                    sumrtt += rtt as i64;
                    count += 1;
                    if rtt > maxrtt {
                        maxrtt = rtt;
                    }
                    println!("[RECV] mode={} sn={} rtt={}", mode, sn, rtt);
                }
                Err(e) => break,
            }
        }
        if next > 1000 {
            break;
        }
    }

    ts1 = clock() - ts1;
    let alice_c = alice_to_bob.clone();
    let mut alice_c = alice_c.lock().unwrap();
    println!("{} mode result ({}ms):", mode, ts1);
    println!("avgrtt={} maxrtt={} tx={}",
             sumrtt / count,
             maxrtt,
             alice_c.tx);
}

#[inline]
fn decode32u(buf: &[u8], p: &mut usize) -> u32 {
    let n = (buf[*p] as u32) | (buf[*p + 1] as u32) << 8 | (buf[*p + 2] as u32) << 16 |
            (buf[*p + 3] as u32) << 24;
    *p += 4;
    u32::from_le(n)
}

#[inline]
fn encode32u(buf: &mut [u8], p: &mut usize, n: u32) {
    let n = n.to_le();
    buf[*p] = n as u8;
    buf[*p + 1] = (n >> 8) as u8;
    buf[*p + 2] = (n >> 16) as u8;
    buf[*p + 3] = (n >> 24) as u8;
    *p += 4;
}

fn to_hex_string(bytes: &[u8]) {
    let mut i = 0;
    for b in bytes {
        print!("0x{:02X} ", b);
        i = i + 1;
        if i % 4 == 0 {
            println!("");
        }
    }
}