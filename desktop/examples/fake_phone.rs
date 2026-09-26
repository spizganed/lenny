//! cargo run -p lenny_desktop --example fake_phone -- [host] [port] [--portrait]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).map_or("127.0.0.1", |s| s.as_str());
    let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(lenny_core::LENNY_DEFAULT_PORT);
    let _phone = lenny_desktop::fake_phone::FakePhone::start(host, port, 0xFA, args.iter().any(|a| a == "--portrait"));
    println!("fake phone streaming to {host}:{port}; Ctrl+C to stop");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
