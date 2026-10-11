#[cfg(build_v1)]
use std::path::Path;
use std::time::Duration;
#[cfg(build_v1)]
use std::time::Instant;

fn main() {
    #[cfg(build_v1)]
    let version = "v1";
    #[cfg(not(build_v1))]
    let version = "v2";
    std::fs::write("launched.txt", version).unwrap();
    #[cfg(build_v1)]
    {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !Path::new("close-v1").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    #[cfg(not(build_v1))]
    std::thread::sleep(Duration::from_secs(5));
}
