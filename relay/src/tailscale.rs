use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Node {
    public_key: String,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
    #[serde(default)]
    online: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(rename = "Self")]
    local: Option<Node>,
    #[serde(default)]
    peer: HashMap<String, Node>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalDevice {
    pub address: Ipv4Addr,
    pub public_key: String,
}

pub fn is_tailnet_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

pub fn parse_local_status(json: &str) -> Result<LocalDevice, String> {
    let status: Status = serde_json::from_str(json).map_err(|_| "Invalid Tailscale status JSON")?;
    if status.backend_state != "Running" {
        return Err(format!(
            "Tailscale is not connected ({})",
            status.backend_state
        ));
    }
    let local = status.local.ok_or("Tailscale has no local device")?;
    let address = local
        .tailscale_ips
        .into_iter()
        .find_map(|address| match address {
            IpAddr::V4(address) if is_tailnet_ipv4(address) => Some(address),
            _ => None,
        })
        .ok_or("Tailscale has no usable Tailnet IPv4 address")?;
    if local.public_key.is_empty() || local.public_key.len() > 256 {
        return Err("Tailscale local device key is unavailable".into());
    }
    Ok(LocalDevice {
        address,
        public_key: local.public_key,
    })
}

pub fn verify_peer_status(json: &str, address: Ipv4Addr, public_key: &str) -> Result<(), String> {
    if !is_tailnet_ipv4(address) || public_key.is_empty() {
        return Err("Invalid Tailnet endpoint".into());
    }
    let status: Status = serde_json::from_str(json).map_err(|_| "Invalid Tailscale status JSON")?;
    if status.backend_state != "Running" {
        return Err("Tailscale is not connected".into());
    }
    if status.peer.values().any(|peer| {
        peer.public_key == public_key
            && peer.tailscale_ips.contains(&IpAddr::V4(address))
            && peer.online
    }) {
        Ok(())
    } else {
        Err("Invited host is not an online, permitted Tailscale peer".into())
    }
}

fn tailscale_exe() -> Result<PathBuf, String> {
    let program_files =
        std::env::var_os("ProgramFiles").ok_or("Program Files path is unavailable")?;
    let exe = PathBuf::from(program_files)
        .join("Tailscale")
        .join("tailscale.exe");
    if !exe.is_file() {
        return Err("Tailscale is not installed at its standard Windows location".into());
    }
    Ok(exe)
}

pub fn status_json() -> Result<String, String> {
    let output = Command::new(tailscale_exe()?)
        .args(["status", "--json"])
        .output()
        .map_err(|error| format!("Tailscale status failed: {error}"))?;
    if !output.status.success() || output.stdout.len() > 2_000_000 {
        return Err("Tailscale status is unavailable or too large".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "Invalid Tailscale status encoding".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{"BackendState":"Running","Self":{"PublicKey":"nodekey:self","TailscaleIPs":["100.75.2.3","fd7a:115c:a1e0::1"],"Online":true},"Peer":{"a":{"PublicKey":"nodekey:friend","TailscaleIPs":["100.98.3.4"],"Online":true}}}"#;

    #[test]
    fn accepts_only_running_tailnet_device() {
        let local = parse_local_status(STATUS).unwrap();
        assert_eq!(local.address, Ipv4Addr::new(100, 75, 2, 3));
        assert_eq!(local.public_key, "nodekey:self");
        assert!(parse_local_status(&STATUS.replace("Running", "Stopped")).is_err());
        assert!(parse_local_status(&STATUS.replace("100.75.2.3", "192.0.2.1")).is_err());
    }

    #[test]
    fn invitation_endpoint_requires_exact_online_peer_key_and_address() {
        let friend = Ipv4Addr::new(100, 98, 3, 4);
        assert!(verify_peer_status(STATUS, friend, "nodekey:friend").is_ok());
        assert!(verify_peer_status(STATUS, friend, "nodekey:attacker").is_err());
        assert!(
            verify_peer_status(STATUS, Ipv4Addr::new(100, 98, 3, 5), "nodekey:friend").is_err()
        );
        assert!(
            verify_peer_status(
                &STATUS.replace("\"Online\":true}}", "\"Online\":false}}"),
                friend,
                "nodekey:friend"
            )
            .is_err()
        );
    }
}
