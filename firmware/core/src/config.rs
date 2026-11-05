//! Build-time constants (the user-changeable ones are in `settings`).

/// Password for the setup Wi-Fi and the web interface (user `admin`).
/// Set it at build time: `ARTNET_PASSWORD='…' cargo build --release`.
pub const PASSWORD: &str = match option_env!("ARTNET_PASSWORD") {
    Some(p) => p,
    None => "artnet-setup",
};

/// True when the firmware was built without `ARTNET_PASSWORD`.
pub const DEFAULT_PASSWORD: bool = option_env!("ARTNET_PASSWORD").is_none();

/// User name for the web interface.
pub const WEB_USER: &str = "admin";

// WPA2 needs 8 to 63 characters.
const _: () = assert!(PASSWORD.len() >= 8 && PASSWORD.len() <= 63);

/// Address of the board on its own Wi-Fi network. Clients get 192.168.4.50+.
pub const AP_IP: core::net::Ipv4Addr = core::net::Ipv4Addr::new(192, 168, 4, 1);

/// Wi-Fi channel of the setup network.
pub const AP_CHANNEL: u8 = 6;

/// The setup network starts after the Ethernet link has been down this long.
pub const AP_AFTER_LINK_DOWN_MS: u64 = 10_000;

/// Holding the BOOT button this long starts the setup network.
pub const AP_BUTTON_HOLD_MS: u64 = 3_000;

/// A new image is kept only when someone confirms it (`POST /confirm`, the
/// button on the status page), after it has run at least this long.
pub const CONFIRM_AFTER_MS: u64 = 30_000;

/// A new image not confirmed within this time restarts, which rolls it back.
pub const TRIAL_DEADLINE_MS: u64 = 900_000;

/// Without a DHCP answer this long after the link comes up, Ethernet falls
/// back to the usual Art-Net address 2.x.y.z/8 (from the MAC). DHCP is tried
/// again after the next link loss.
pub const DHCP_TIMEOUT_MS: u64 = 15_000;
