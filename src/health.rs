use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ResourceUsage {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

impl ResourceUsage {
    pub(crate) fn new(used_bytes: u64, total_bytes: u64) -> Option<Self> {
        (total_bytes > 0).then_some(Self {
            used_bytes: used_bytes.min(total_bytes),
            total_bytes,
        })
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct BatteryHealth {
    pub percentage: u8,
    #[serde(default)]
    pub charging: Option<bool>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct HealthSnapshot {
    // These options also travel over the internal bincode snapshot. Keep absent
    // values encoded as None/null; skipping fields would corrupt that stream.
    #[serde(default)]
    pub cpu_usage_percentage: Option<u8>,
    #[serde(default)]
    pub temperature_celsius: Option<i16>,
    #[serde(default)]
    pub battery: Option<BatteryHealth>,
    #[serde(default)]
    pub ram: Option<ResourceUsage>,
    #[serde(default)]
    pub disk: Option<ResourceUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CpuTimes {
    pub(crate) busy: u64,
    pub(crate) total: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlatformHealthProbe {
    pub(crate) cpu_times: Option<CpuTimes>,
    pub(crate) temperature_celsius: Option<i16>,
    pub(crate) battery: Option<BatteryHealth>,
    pub(crate) ram: Option<ResourceUsage>,
    pub(crate) disk: Option<ResourceUsage>,
}

#[derive(Debug, Default)]
pub(crate) struct HealthSampler {
    previous_cpu_times: Option<CpuTimes>,
}

impl HealthSampler {
    pub(crate) fn sample(&mut self) -> HealthSnapshot {
        let probe = crate::platform::health_probe();
        let cpu_usage_percentage = cpu_usage_percentage(self.previous_cpu_times, probe.cpu_times);
        if probe.cpu_times.is_some() {
            self.previous_cpu_times = probe.cpu_times;
        }

        HealthSnapshot {
            cpu_usage_percentage,
            temperature_celsius: probe.temperature_celsius,
            battery: probe.battery,
            ram: probe.ram,
            disk: probe.disk,
        }
    }
}

pub(crate) fn celsius_from_scaled(
    value: i64,
    millidegrees_per_unit: i64,
    zero_offset_millidegrees: i64,
) -> Option<i16> {
    if millidegrees_per_unit <= 0 {
        return None;
    }
    let millidegrees = value
        .checked_mul(millidegrees_per_unit)?
        .checked_add(zero_offset_millidegrees)?;
    let rounded = if millidegrees >= 0 {
        millidegrees.saturating_add(500) / 1_000
    } else {
        millidegrees.saturating_sub(500) / 1_000
    };
    valid_temperature_celsius(rounded)
}

fn valid_temperature_celsius(value: i64) -> Option<i16> {
    (-100..=200)
        .contains(&value)
        .then(|| i16::try_from(value).ok())
        .flatten()
}

fn cpu_usage_percentage(previous: Option<CpuTimes>, current: Option<CpuTimes>) -> Option<u8> {
    let previous = previous?;
    let current = current?;
    let total = current.total.checked_sub(previous.total)?;
    let busy = current.busy.checked_sub(previous.busy)?.min(total);
    if total == 0 {
        return None;
    }
    Some(((busy.saturating_mul(100) + total / 2) / total).min(100) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percentage_uses_delta_between_samples() {
        assert_eq!(
            cpu_usage_percentage(
                Some(CpuTimes {
                    busy: 1_000,
                    total: 2_000,
                }),
                Some(CpuTimes {
                    busy: 1_075,
                    total: 2_100,
                }),
            ),
            Some(75)
        );
    }

    #[test]
    fn cpu_percentage_needs_two_monotonic_samples() {
        assert_eq!(
            cpu_usage_percentage(
                None,
                Some(CpuTimes {
                    busy: 10,
                    total: 20,
                })
            ),
            None
        );
        assert_eq!(
            cpu_usage_percentage(
                Some(CpuTimes {
                    busy: 10,
                    total: 20,
                }),
                Some(CpuTimes { busy: 5, total: 10 })
            ),
            None
        );
    }

    #[test]
    fn resource_usage_clamps_used_bytes_to_total() {
        assert_eq!(
            ResourceUsage::new(12, 10),
            Some(ResourceUsage {
                used_bytes: 10,
                total_bytes: 10,
            })
        );
        assert_eq!(ResourceUsage::new(0, 0), None);
    }

    #[test]
    fn temperature_conversions_round_and_reject_implausible_values() {
        assert_eq!(celsius_from_scaled(42_499, 1, 0), Some(42));
        assert_eq!(celsius_from_scaled(42_500, 1, 0), Some(43));
        assert_eq!(celsius_from_scaled(-10_500, 1, 0), Some(-11));
        assert_eq!(celsius_from_scaled(3_073, 100, -273_150), Some(34));
        assert_eq!(celsius_from_scaled(250_000, 1, 0), None);
        assert_eq!(celsius_from_scaled(0, 100, -273_150), None);
        assert_eq!(celsius_from_scaled(42, 0, 0), None);
    }
}
