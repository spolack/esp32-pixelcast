//! The status LED's states and colours (sent by the firmware's `statusled`).

/// What the LED shows, in priority order (first match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A firmware update is being written.
    Updating,
    /// A test pattern owns the LED output.
    TestPattern,
    /// Shortly after start-up, before any network is up.
    Starting,
    /// Data arrives (Art-Net or sACN).
    Receiving,
    /// Ethernet link up, no data.
    Ready,
    /// Only the setup Wi-Fi is up.
    SetupWifi,
    /// No Ethernet link and no setup Wi-Fi.
    NoNetwork,
    /// BOOT held past 3 s: releasing starts the setup Wi-Fi.
    ButtonWifi,
    /// BOOT held past 10 s: password and network were reset.
    ButtonReset,
    /// BOOT clicked three times: restarting into the other slot.
    StartingOther,
}

/// Colour (R, G, B) for `state` at time `ms`; blinking and flashes are
/// derived from the time. Kept dim: it sits next to the user.
pub fn colour(state: State, ms: u64) -> [u8; 3] {
    let phase = ms % 1000;
    match state {
        State::Updating => {
            if phase < 500 {
                [40, 0, 40]
            } else {
                [0, 0, 0]
            }
        }
        State::TestPattern => [40, 0, 40],
        State::Starting => [40, 30, 0],
        State::Receiving => {
            // A short white flash twice a second on green.
            if ms % 500 < 40 {
                [30, 30, 30]
            } else {
                [0, 40, 0]
            }
        }
        State::Ready => [0, 40, 0],
        State::SetupWifi => [0, 0, 40],
        State::NoNetwork => {
            if phase < 500 {
                [50, 0, 0]
            } else {
                [0, 0, 0]
            }
        }
        State::ButtonWifi => [0, 0, 60],
        State::StartingOther => [40, 40, 40],
        State::ButtonReset => {
            if phase % 200 < 100 {
                [60, 0, 0]
            } else {
                [0, 0, 0]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinks_and_flashes() {
        assert_eq!(colour(State::NoNetwork, 100), [50, 0, 0]);
        assert_eq!(colour(State::NoNetwork, 600), [0, 0, 0]);
        assert_eq!(colour(State::Receiving, 10), [30, 30, 30]);
        assert_eq!(colour(State::Receiving, 100), [0, 40, 0]);
        assert_eq!(colour(State::Ready, 123), [0, 40, 0]);
    }
}
