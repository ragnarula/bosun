//! Small time helpers.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Seconds since the unix epoch for a timestamp; 0 before 1970.
pub fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn unix_secs_counts_from_the_epoch_and_clamps_before_it() {
        assert_eq!(unix_secs(UNIX_EPOCH + Duration::from_secs(5)), 5);
        assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_secs(1)), 0);
    }
}
