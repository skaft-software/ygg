use std::fs::OpenOptions;
use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

static START_INSTANT: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START_INSTANT.get_or_init(Instant::now);
    let _ = std::fs::remove_file("ygg_instrument.log");
    log("process_start");
}

pub fn log(event: &str) {
    let now = Instant::now();
    let start = *START_INSTANT.get_or_init(Instant::now);
    let elapsed = now.duration_since(start).as_micros();
    
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("ygg_instrument.log")
    {
        let _ = writeln!(file, "[{elapsed:>12} μs] {event}");
    }
}
