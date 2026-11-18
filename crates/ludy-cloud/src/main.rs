use std::net::SocketAddr;
use std::path::PathBuf;

use ludy_cloud::{serve, Config};

const USAGE: &str = "\
ludy-cloud: a simulated cloud object store for Ludy

USAGE:
    ludy-cloud [--data DIR] [--port N] [--latency-ms N] [--mbps N] [--quiet]

OPTIONS:
    --data DIR        where drives and blobs are kept        [default: ./data]
    --port N          listen on 127.0.0.1:N                  [default: 9000]
    --latency-ms N    delay before the first byte            [default: 30]
    --mbps N          shared bandwidth cap, 0 = unlimited    [default: 400]
    --quiet           do not print the request log

Latency and bandwidth can be changed while running:
    curl -X PUT 'http://127.0.0.1:9000/_config?latency_ms=80&mbps=100'
";

#[tokio::main]
async fn main() {
    let mut cfg = Config {
        data_dir: PathBuf::from("data"),
        addr: SocketAddr::from(([127, 0, 0, 1], 9000)),
        latency_ms: 30,
        mbps: 400,
        print_log: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("{name} needs a value\n\n{USAGE}");
                std::process::exit(2)
            })
        };
        match a.as_str() {
            "--data" => cfg.data_dir = PathBuf::from(value("--data")),
            "--port" => cfg.addr.set_port(parse(&value("--port"), "--port")),
            "--latency-ms" => cfg.latency_ms = parse(&value("--latency-ms"), "--latency-ms"),
            "--mbps" => cfg.mbps = parse(&value("--mbps"), "--mbps"),
            "--quiet" => cfg.print_log = false,
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("unknown argument {other:?}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let (addr, cloud, task) = match serve(cfg.clone()).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ludy-cloud: cannot start on {}: {e}", cfg.addr);
            std::process::exit(1);
        }
    };
    let drives = cloud.store.drives().await;
    println!(
        "ludy-cloud listening on http://{addr}  data={}  latency={}ms  bandwidth={}",
        cfg.data_dir.display(),
        cfg.latency_ms,
        if cfg.mbps == 0 { "unlimited".to_string() } else { format!("{} Mbit/s", cfg.mbps) }
    );
    println!("{} drive(s): {}", drives.len(), drives.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", "));
    let _ = task.await;
}

fn parse<T: std::str::FromStr>(s: &str, name: &str) -> T {
    s.parse().unwrap_or_else(|_| {
        eprintln!("{name}: cannot parse {s:?}");
        std::process::exit(2)
    })
}
