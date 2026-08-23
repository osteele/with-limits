use std::{fmt, str::FromStr, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MemorySpec {
    Bytes(u64),
    AvailableFraction(f64),
}

impl MemorySpec {
    pub fn resolve(self, available: u64) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::AvailableFraction(fraction) => (available as f64 * fraction) as u64,
        }
    }
}

impl FromStr for MemorySpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("auto") {
            return Ok(Self::AvailableFraction(0.7));
        }
        if let Some(percent) = value.strip_suffix('%') {
            let percent = positive_number(percent, "memory percentage")?;
            if percent > 100.0 {
                return Err("memory percentage must not exceed 100%".into());
            }
            return Ok(Self::AvailableFraction(percent / 100.0));
        }
        let split = value
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(value.len());
        let number = positive_number(&value[..split], "memory size")?;
        let suffix = value[split..].trim().to_ascii_lowercase();
        let multiplier = match suffix.as_str() {
            "" | "b" => 1.0,
            "k" | "kb" => 1_000.0,
            "m" | "mb" => 1_000_000.0,
            "g" | "gb" => 1_000_000_000.0,
            "t" | "tb" => 1_000_000_000_000.0,
            "ki" | "kib" => 1024.0,
            "mi" | "mib" => 1024.0_f64.powi(2),
            "gi" | "gib" => 1024.0_f64.powi(3),
            "ti" | "tib" => 1024.0_f64.powi(4),
            _ => return Err(format!("unknown memory-size suffix {suffix:?}")),
        };
        let bytes = number * multiplier;
        if !bytes.is_finite() || bytes > u64::MAX as f64 {
            return Err("memory size is too large".into());
        }
        Ok(Self::Bytes(bytes as u64))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuLimit(pub f64);

impl FromStr for CpuLimit {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(positive_number(value, "CPU limit")?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HumanDuration(pub Duration);

impl FromStr for HumanDuration {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let split = value
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(value.len());
        let number = positive_number(&value[..split], "duration")?;
        let suffix = value[split..].trim().to_ascii_lowercase();
        let seconds = match suffix.as_str() {
            "ms" => number / 1_000.0,
            "" | "s" => number,
            "m" => number * 60.0,
            "h" => number * 3_600.0,
            "d" => number * 86_400.0,
            _ => return Err(format!("unknown duration suffix {suffix:?}")),
        };
        if !seconds.is_finite() || seconds > Duration::MAX.as_secs_f64() {
            return Err("duration is too large".into());
        }
        Ok(Self(Duration::from_secs_f64(seconds)))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0.as_secs_f64())
    }
}

fn positive_number(value: &str, label: &str) -> Result<f64, String> {
    let number = value
        .parse::<f64>()
        .map_err(|_| format!("invalid {label} {value:?}"))?;
    if !number.is_finite() || number <= 0.0 {
        return Err(format!("{label} must be positive"));
    }
    Ok(number)
}

pub fn format_bytes(bytes: u64) -> String {
    for (unit, divisor) in [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ] {
        if bytes >= divisor {
            return format!("{:.1} {unit}", bytes as f64 / divisor as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_memory_sizes() {
        assert_eq!(
            "2GiB".parse(),
            Ok(MemorySpec::Bytes(2 * 1024 * 1024 * 1024))
        );
        assert_eq!("1.5 GB".parse(), Ok(MemorySpec::Bytes(1_500_000_000)));
        assert_eq!("50%".parse(), Ok(MemorySpec::AvailableFraction(0.5)));
        assert_eq!("auto".parse(), Ok(MemorySpec::AvailableFraction(0.7)));
    }

    #[test]
    fn parses_gnu_style_durations() {
        assert_eq!(
            "250ms".parse(),
            Ok(HumanDuration(Duration::from_millis(250)))
        );
        assert_eq!("1.5m".parse(), Ok(HumanDuration(Duration::from_secs(90))));
        assert_eq!("2h".parse(), Ok(HumanDuration(Duration::from_secs(7200))));
    }

    #[test]
    fn rejects_non_positive_values() {
        assert!("0".parse::<CpuLimit>().is_err());
        assert!("101%".parse::<MemorySpec>().is_err());
        assert!("-1s".parse::<HumanDuration>().is_err());
    }
}
