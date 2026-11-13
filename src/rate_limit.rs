use std::error::Error;
use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

const BYTES_PER_SECOND: u128 = 16;
const BURST_BYTES: usize = 512;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const BURST_UNITS: u128 = BURST_BYTES as u128 * NANOS_PER_SECOND;

pub struct ByteRateLimiter {
    available_byte_nanos: u128,
    last_refill: Instant,
}

impl ByteRateLimiter {
    pub fn new() -> Self {
        Self {
            available_byte_nanos: BURST_UNITS,
            last_refill: Instant::now(),
        }
    }

    pub fn wait_for(&mut self, bytes: usize) -> Result<(), RateLimitError> {
        loop {
            let wait = self.reserve_at(bytes, Instant::now())?;
            if wait.is_zero() {
                return Ok(());
            }
            thread::sleep(wait);
        }
    }

    fn reserve_at(&mut self, bytes: usize, now: Instant) -> Result<Duration, RateLimitError> {
        if bytes > BURST_BYTES {
            return Err(RateLimitError { bytes });
        }

        let elapsed_nanos = now.saturating_duration_since(self.last_refill).as_nanos();
        self.available_byte_nanos =
            (self.available_byte_nanos + elapsed_nanos * BYTES_PER_SECOND).min(BURST_UNITS);
        self.last_refill = now;

        let cost = bytes as u128 * NANOS_PER_SECOND;
        if self.available_byte_nanos >= cost {
            self.available_byte_nanos -= cost;
            return Ok(Duration::ZERO);
        }

        let deficit = cost - self.available_byte_nanos;
        let wait_nanos = deficit.div_ceil(BYTES_PER_SECOND);
        Ok(Duration::from_nanos(wait_nanos as u64))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct RateLimitError {
    bytes: usize,
}

impl fmt::Display for RateLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "outbound command is {} bytes, larger than the {}-byte burst capacity",
            self.bytes, BURST_BYTES
        )
    }
}

impl Error for RateLimitError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_burst_and_sustained_rate_without_sleeping() {
        let start = Instant::now();
        let mut limiter = ByteRateLimiter {
            available_byte_nanos: BURST_UNITS,
            last_refill: start,
        };

        assert_eq!(limiter.reserve_at(512, start), Ok(Duration::ZERO));
        assert_eq!(
            limiter.reserve_at(1, start),
            Ok(Duration::from_millis(62) + Duration::from_micros(500))
        );
        assert_eq!(
            limiter.reserve_at(1, start + Duration::from_millis(62)),
            Ok(Duration::from_micros(500))
        );
        assert_eq!(
            limiter.reserve_at(1, start + Duration::from_millis(63)),
            Ok(Duration::ZERO)
        );
        assert_eq!(
            limiter.reserve_at(15, start + Duration::from_secs(1)),
            Ok(Duration::ZERO)
        );
        assert_eq!(
            limiter.reserve_at(1, start + Duration::from_secs(1)),
            Ok(Duration::from_millis(62) + Duration::from_micros(500))
        );
    }

    #[test]
    fn rejects_commands_larger_than_burst() {
        let mut limiter = ByteRateLimiter::new();
        assert_eq!(
            limiter.reserve_at(513, Instant::now()),
            Err(RateLimitError { bytes: 513 })
        );
    }
}
