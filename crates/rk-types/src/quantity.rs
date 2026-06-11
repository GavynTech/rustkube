//! CPU and memory quantities with Kubernetes-style string forms.
//!
//! Supported subset of the Kubernetes quantity grammar:
//! - CPU: whole cores (`2`), fractional cores up to three decimals (`1.5`,
//!   `0.25`) or millicores (`500m`). Stored as millicores.
//! - Memory: plain bytes (`134217728`), decimal suffixes (`k`/`K`, `M`, `G`,
//!   `T`) or binary suffixes (`Ki`, `Mi`, `Gi`, `Ti`). Stored as bytes.
//!
//! Exponent notation (`1e9`) is intentionally not supported.

use std::fmt;
use std::str::FromStr;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid quantity {input:?}: {reason}")]
pub struct QuantityError {
    pub input: String,
    pub reason: &'static str,
}

impl QuantityError {
    fn new(input: &str, reason: &'static str) -> Self {
        QuantityError {
            input: input.to_owned(),
            reason,
        }
    }
}

/// CPU quantity in millicores (1000m = one core).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cpu(u64);

impl Cpu {
    pub const fn from_millis(millis: u64) -> Self {
        Cpu(millis)
    }

    pub const fn from_cores(cores: u64) -> Self {
        Cpu(cores * 1000)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

impl FromStr for Cpu {
    type Err = QuantityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if let Some(millis) = trimmed.strip_suffix('m') {
            let value: u64 = millis
                .parse()
                .map_err(|_| QuantityError::new(s, "millicores must be a whole number"))?;
            return Ok(Cpu(value));
        }
        cores_to_millis(trimmed)
            .map(Cpu)
            .map_err(|reason| QuantityError::new(s, reason))
    }
}

/// Parse a decimal core count ("1", "1.5", ".25") into millicores without
/// going through floating point.
fn cores_to_millis(s: &str) -> Result<u64, &'static str> {
    let (int_part, frac_part) = match s.split_once('.') {
        None => (s, ""),
        Some((int, frac)) => (int, frac),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("expected a number");
    }
    if frac_part.len() > 3 {
        return Err("at most three decimal places of a core (1m) are supported");
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err("expected digits");
    }
    let int: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| "number out of range")?
    };
    let mut frac: u64 = if frac_part.is_empty() {
        0
    } else {
        frac_part.parse().map_err(|_| "number out of range")?
    };
    for _ in frac_part.len()..3 {
        frac *= 10;
    }
    int.checked_mul(1000)
        .and_then(|millis| millis.checked_add(frac))
        .ok_or("number out of range")
}

impl fmt::Display for Cpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}m", self.0)
    }
}

impl Serialize for Cpu {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cpu {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CpuVisitor)
    }
}

struct CpuVisitor;

impl Visitor<'_> for CpuVisitor {
    type Value = Cpu;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a CPU quantity like \"500m\", \"1.5\" or 2")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        v.parse().map_err(E::custom)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        v.checked_mul(1000)
            .map(Cpu)
            .ok_or_else(|| E::custom("cpu core count out of range"))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        let cores = u64::try_from(v).map_err(|_| E::custom("cpu must be non-negative"))?;
        self.visit_u64(cores)
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        // The range check also rejects NaN and the infinities.
        if !(0.0..=1e15).contains(&v) {
            return Err(E::custom("cpu must be a non-negative finite number"));
        }
        Ok(Cpu((v * 1000.0).round() as u64))
    }
}

/// Memory quantity in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Memory(u64);

impl Memory {
    pub const fn from_bytes(bytes: u64) -> Self {
        Memory(bytes)
    }

    pub const fn from_mib(mib: u64) -> Self {
        Memory(mib << 20)
    }

    pub const fn from_gib(gib: u64) -> Self {
        Memory(gib << 30)
    }

    pub const fn as_bytes(self) -> u64 {
        self.0
    }
}

/// Two-character binary suffixes must be checked before one-character decimal
/// ones so `Ki` is not consumed as `K`-with-trailing-garbage.
const MEMORY_SUFFIXES: [(&str, u64); 9] = [
    ("Ki", 1 << 10),
    ("Mi", 1 << 20),
    ("Gi", 1 << 30),
    ("Ti", 1 << 40),
    ("k", 1_000),
    ("K", 1_000),
    ("M", 1_000_000),
    ("G", 1_000_000_000),
    ("T", 1_000_000_000_000),
];

impl FromStr for Memory {
    type Err = QuantityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let (digits, multiplier) = MEMORY_SUFFIXES
            .iter()
            .find_map(|(suffix, multiplier)| {
                trimmed
                    .strip_suffix(suffix)
                    .map(|digits| (digits, *multiplier))
            })
            .unwrap_or((trimmed, 1));
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(QuantityError::new(s, "expected digits before the suffix"));
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| QuantityError::new(s, "number out of range"))?;
        value
            .checked_mul(multiplier)
            .map(Memory)
            .ok_or_else(|| QuantityError::new(s, "number out of range"))
    }
}

impl fmt::Display for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const BINARY: [(&str, u64); 4] = [
            ("Ti", 1 << 40),
            ("Gi", 1 << 30),
            ("Mi", 1 << 20),
            ("Ki", 1 << 10),
        ];
        for (suffix, multiplier) in BINARY {
            if self.0 != 0 && self.0.is_multiple_of(multiplier) {
                return write!(f, "{}{suffix}", self.0 / multiplier);
            }
        }
        write!(f, "{}", self.0)
    }
}

impl Serialize for Memory {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Memory {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(MemoryVisitor)
    }
}

struct MemoryVisitor;

impl Visitor<'_> for MemoryVisitor {
    type Value = Memory;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a memory quantity like \"256Mi\", \"1G\" or a byte count")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        v.parse().map_err(E::custom)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(Memory(v))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        u64::try_from(v)
            .map(Memory)
            .map_err(|_| E::custom("memory must be non-negative"))
    }
}

/// Named resource amounts, used for container requests/limits and node
/// capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Cpu>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<Memory>,
}

impl ResourceList {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none()
    }
}

/// Container resource requirements: `requests` drive scheduling, `limits`
/// become cgroup caps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Resources {
    #[serde(skip_serializing_if = "ResourceList::is_empty")]
    pub requests: ResourceList,
    #[serde(skip_serializing_if = "ResourceList::is_empty")]
    pub limits: ResourceList,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_parses_the_documented_forms() {
        let cases = [
            ("500m", 500),
            ("0m", 0),
            ("2", 2000),
            ("1.5", 1500),
            ("0.25", 250),
            (".5", 500),
            ("1.025", 1025),
        ];
        for (input, millis) in cases {
            assert_eq!(input.parse::<Cpu>().unwrap().as_millis(), millis, "{input}");
        }
        for bad in ["", "m", "1.2345", "-1", "two", "1.5m"] {
            assert!(bad.parse::<Cpu>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn memory_parses_the_documented_forms() {
        let cases = [
            ("128974848", 128_974_848),
            ("129M", 129_000_000),
            ("123Mi", 123 << 20),
            ("1Gi", 1 << 30),
            ("2G", 2_000_000_000),
            ("64Ki", 64 << 10),
            ("1k", 1_000),
            ("1T", 1_000_000_000_000),
            ("1Ti", 1 << 40),
        ];
        for (input, bytes) in cases {
            assert_eq!(
                input.parse::<Memory>().unwrap().as_bytes(),
                bytes,
                "{input}"
            );
        }
        for bad in ["", "Mi", "1.5Gi", "-5M", "12Q"] {
            assert!(bad.parse::<Memory>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn display_round_trips() {
        for input in ["500m", "2", "1.5"] {
            let parsed: Cpu = input.parse().unwrap();
            let again: Cpu = parsed.to_string().parse().unwrap();
            assert_eq!(parsed, again);
        }
        for input in ["123Mi", "1Gi", "999", "1000"] {
            let parsed: Memory = input.parse().unwrap();
            let again: Memory = parsed.to_string().parse().unwrap();
            assert_eq!(parsed, again);
        }
    }

    #[test]
    fn quantities_deserialize_from_yaml_strings_and_numbers() {
        #[derive(Deserialize)]
        struct Spec {
            cpu: Cpu,
            memory: Memory,
        }
        let from_strings: Spec = serde_yaml::from_str("cpu: \"500m\"\nmemory: 256Mi\n").unwrap();
        assert_eq!(from_strings.cpu.as_millis(), 500);
        assert_eq!(from_strings.memory.as_bytes(), 256 << 20);

        let from_numbers: Spec = serde_yaml::from_str("cpu: 2\nmemory: 1048576\n").unwrap();
        assert_eq!(from_numbers.cpu.as_millis(), 2000);
        assert_eq!(from_numbers.memory.as_bytes(), 1 << 20);

        let fractional: Spec = serde_yaml::from_str("cpu: 1.5\nmemory: 1Ki\n").unwrap();
        assert_eq!(fractional.cpu.as_millis(), 1500);
    }
}
