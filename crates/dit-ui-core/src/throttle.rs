//! Live throttle of the devices a test watches, for the running screens: each device's slowdowns
//! since the test started on a `THROTTLE` row, and a footer alert for a while after a new one.
//! The finished test's result shows the same row counted from the reports.

use std::time::{Duration, Instant};

/// How long the footer alert stays after a new throttle event.
pub const THROTTLE_ALERT_FOR: Duration = Duration::from_secs(30);

/// One device on a `THROTTLE` row: `3 (42 s)`, `3 (2 min 05 s)`, or the bare count when the
/// device gives no time (or never throttled).
#[must_use]
pub fn throttle_count(events: u64, seconds: Option<u64>) -> String {
    match seconds {
        Some(seconds) if events > 0 && seconds >= 60 => {
            format!("{events} ({} min {:02} s)", seconds / 60, seconds % 60)
        }
        Some(seconds) if events > 0 => format!("{events} ({seconds} s)"),
        _ => events.to_string(),
    }
}

#[derive(Debug)]
struct Device {
    label: String,
    /// Events and seconds since the test started; `None` without throttle counters.
    reading: Option<(u64, Option<u64>)>,
}

/// The running screens' throttle row and alert.
#[derive(Debug, Default)]
pub struct ThrottleLive {
    devices: Vec<Device>,
    alert: Option<(String, Instant)>,
}

impl ThrottleLive {
    /// `label`'s throttling since the test started (events, seconds), `None` without counters.
    /// More events than before raise the footer alert, stamped with `clock` (the test's time).
    pub fn update(&mut self, label: &str, reading: Option<(u64, Option<u64>)>, clock: &str) {
        let position = self
            .devices
            .iter()
            .position(|device| device.label == label)
            .unwrap_or_else(|| {
                self.devices.push(Device {
                    label: label.to_owned(),
                    reading: None,
                });
                self.devices.len() - 1
            });
        let device = &mut self.devices[position];
        let before = device.reading.map_or(0, |(events, _)| events);
        if let Some((events, _)) = reading
            && events > before
        {
            self.alert = Some((
                format!(
                    "! THROTTLE {label} +{} at {clock} - it slowed itself down for heat or a hardware limit",
                    events - before
                ),
                Instant::now(),
            ));
        }
        if reading.is_some() || device.reading.is_none() {
            device.reading = reading;
        }
    }

    /// `THROTTLE      CPU 2 (14 s) | GPU1 0 | SSD1 N/A`; `None` before any device is watched.
    #[must_use]
    pub fn row(&self) -> Option<String> {
        (!self.devices.is_empty()).then(|| {
            let parts: Vec<String> = self
                .devices
                .iter()
                .map(|device| {
                    device.reading.map_or_else(
                        || format!("{} N/A", device.label),
                        |(events, seconds)| {
                            format!("{} {}", device.label, throttle_count(events, seconds))
                        },
                    )
                })
                .collect();
            format!("THROTTLE      {}", parts.join(" | "))
        })
    }

    /// The footer alert while it lasts ([`THROTTLE_ALERT_FOR`] after the last new event).
    #[must_use]
    pub fn alert(&self) -> Option<&str> {
        self.alert
            .as_ref()
            .filter(|(_, at)| at.elapsed() < THROTTLE_ALERT_FOR)
            .map(|(text, _)| text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Devices keep their order; a rise in events raises the alert with the test's time; a
    /// device without counters reads N/A.
    #[test]
    fn the_live_row_counts_per_device_and_alerts_on_a_new_event() {
        let mut live = ThrottleLive::default();
        assert_eq!(live.row(), None);
        live.update("CPU", Some((0, Some(0))), "00:00:05");
        live.update("GPU1", Some((0, None)), "00:00:05");
        live.update("SSD1", None, "00:00:05");
        assert_eq!(live.alert(), None);
        assert_eq!(
            live.row().unwrap(),
            "THROTTLE      CPU 0 | GPU1 0 | SSD1 N/A"
        );
        live.update("CPU", Some((2, Some(75))), "00:12:31");
        assert_eq!(
            live.row().unwrap(),
            "THROTTLE      CPU 2 (1 min 15 s) | GPU1 0 | SSD1 N/A"
        );
        assert!(
            live.alert()
                .unwrap()
                .starts_with("! THROTTLE CPU +2 at 00:12:31")
        );
    }
}
