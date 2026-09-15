use std::{
    fmt,
    time::{Duration, Instant},
};

#[derive(Copy, Clone)]
pub struct HumanDuration(Duration);
impl HumanDuration {
    pub fn since(i: Instant) -> Self {
        Self(Instant::now().duration_since(i))
    }

    /// Get this duration as an f64 representing the duration in fractional seconds
    #[inline]
    pub fn as_secs_f64(&self) -> f64 {
        self.0.as_secs_f64()
    }

    /// Adds two [HumanDuration], using saturating arithmetic
    #[inline]
    pub fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}
impl core::ops::Add for HumanDuration {
    type Output = HumanDuration;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}
impl core::ops::Add<Duration> for HumanDuration {
    type Output = HumanDuration;

    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0 + rhs)
    }
}
impl core::ops::AddAssign for HumanDuration {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}
impl core::ops::AddAssign<Duration> for HumanDuration {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs;
    }
}
impl From<HumanDuration> for Duration {
    fn from(d: HumanDuration) -> Self {
        d.0
    }
}
impl From<Duration> for HumanDuration {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}
impl fmt::Debug for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.0.as_secs();
        let alt = f.alternate();
        macro_rules! try_unit {
            ($secs:expr, $sg:expr, $pl:expr, $s:expr) => {
                let cnt = t / $secs;
                if cnt == 1 {
                    if alt {
                        return write!(f, "{}{}", cnt, $s);
                    } else {
                        return write!(f, "{} {}", cnt, $sg);
                    }
                } else if cnt > 1 {
                    if alt {
                        return write!(f, "{}{}", cnt, $s);
                    } else {
                        return write!(f, "{} {}", cnt, $pl);
                    }
                }
            };
        }

        if t > 0 {
            try_unit!(365 * 24 * 60 * 60, "year", "years", "y");
            try_unit!(7 * 24 * 60 * 60, "week", "weeks", "w");
            try_unit!(24 * 60 * 60, "day", "days", "d");
            try_unit!(60 * 60, "hour", "hours", "h");
            try_unit!(60, "minute", "minutes", "m");
            try_unit!(1, "second", "seconds", "s");
        } else {
            // Time was too precise for the standard path, use millis
            let t = self.0.as_millis();
            if t > 0 {
                return write!(f, "{}{}", t, if alt { "ms" } else { " milliseconds" });
            }
        }
        write!(f, "0{}", if alt { "s" } else { " seconds" })
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::*;

    #[test]
    fn durations_render_singular_plural_and_abbreviated_units() {
        for (seconds, singular, plural, abbreviation) in [
            (1, "second", "seconds", "s"),
            (60, "minute", "minutes", "m"),
            (3600, "hour", "hours", "h"),
            (86400, "day", "days", "d"),
            (604800, "week", "weeks", "w"),
            (31536000, "year", "years", "y"),
        ] {
            let duration = HumanDuration::from(Duration::from_secs(seconds));
            assert_eq!(duration.to_string(), format!("1 {singular}"));
            assert_eq!(format!("{duration:#}"), format!("1{abbreviation}"));
            let duration = HumanDuration::from(Duration::from_secs(seconds * 2));
            assert_eq!(duration.to_string(), format!("2 {plural}"));
            assert_eq!(format!("{duration:#}"), format!("2{abbreviation}"));
            assert_eq!(format!("{duration:?}"), duration.to_string());
        }
        let milliseconds = HumanDuration::from(Duration::from_millis(42));
        assert_eq!(milliseconds.to_string(), "42 milliseconds");
        assert_eq!(format!("{milliseconds:#}"), "42ms");
        let zero = HumanDuration::from(Duration::ZERO);
        assert_eq!(zero.to_string(), "0 seconds");
        assert_eq!(format!("{zero:#}"), "0s");
    }

    #[test]
    fn duration_arithmetic_preserves_precision_and_saturates() {
        let duration = HumanDuration::from(Duration::from_millis(1500));
        assert_eq!(duration.as_secs_f64(), 1.5);
        assert_eq!(Duration::from(duration + duration), Duration::from_secs(3));
        let mut sum = duration + Duration::from_millis(500);
        sum += duration;
        sum += Duration::from_millis(500);
        assert_eq!(Duration::from(sum), Duration::from_secs(4));
        assert_eq!(
            Duration::from(HumanDuration::from(Duration::MAX).saturating_add(duration)),
            Duration::MAX
        );
        let instant = Instant::now();
        assert!(Duration::from(HumanDuration::since(instant)) <= instant.elapsed());
    }
}
