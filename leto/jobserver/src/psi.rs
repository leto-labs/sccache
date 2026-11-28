use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Debug)]
pub struct PsiConfig {
    pub enabled: bool,
    pub psi_dir: PathBuf,
    pub interval_ms: u64,
    pub window: usize,
    pub high: f64,
    pub low: f64,
    pub step: usize,
    pub max_withhold_fraction: f64,
}

impl Default for PsiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            psi_dir: PathBuf::from("/proc/pressure"),
            interval_ms: 1000,
            window: 3,
            high: 20.0,
            low: 8.0,
            step: 4,
            max_withhold_fraction: 0.75,
        }
    }
}

impl PsiConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.interval_ms == 0 {
            return Err("psi-interval-ms must be > 0".into());
        }
        if self.window == 0 {
            return Err("psi-window must be > 0".into());
        }
        if !self.low.is_finite()
            || !self.high.is_finite()
            || self.low < 0.0
            || self.low >= self.high
            || self.high > 100.0
        {
            return Err("PSI thresholds must satisfy 0 <= low < high <= 100".into());
        }
        if self.step == 0 {
            return Err("psi-step must be >= 1".into());
        }
        if !self.max_withhold_fraction.is_finite()
            || self.max_withhold_fraction <= 0.0
            || self.max_withhold_fraction >= 1.0
        {
            return Err("psi-max-withhold-fraction must be between 0 and 1, exclusive".into());
        }
        Ok(())
    }

    pub fn withhold_cap(&self, capacity: usize) -> usize {
        if !self.enabled || capacity <= 1 {
            return 0;
        }
        let raw = ((capacity as f64) * self.max_withhold_fraction).floor() as usize;
        raw.min(capacity.saturating_sub(1))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PsiSnapshot {
    pub enabled: bool,
    pub io_full_avg10: Option<f64>,
    pub io_smoothed: Option<f64>,
    pub memory_some_avg10: Option<f64>,
    pub high: f64,
    pub low: f64,
    pub step: usize,
    pub max_withhold_fraction: f64,
    pub withhold_cap: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PsiAction {
    Unchanged,
    Withheld { delta: usize, new_withheld: usize },
    Released { delta: usize, new_withheld: usize },
}

pub struct PsiGovernor {
    config: PsiConfig,
    samples: VecDeque<f64>,
    withheld: usize,
    last_io: Option<f64>,
    last_smoothed: Option<f64>,
    last_memory: Option<f64>,
}

impl PsiGovernor {
    pub fn new(config: PsiConfig) -> Self {
        let cap = config.window.max(1);
        Self {
            config,
            samples: VecDeque::with_capacity(cap),
            withheld: 0,
            last_io: None,
            last_smoothed: None,
            last_memory: None,
        }
    }

    pub fn withheld(&self) -> usize {
        self.withheld
    }

    pub fn clamp_to_capacity(&mut self, capacity: usize) -> bool {
        let cap = self.config.withhold_cap(capacity);
        if self.withheld > cap {
            self.withheld = cap;
            true
        } else {
            false
        }
    }

    pub fn sample_from_disk(&mut self, capacity: usize) -> Result<PsiAction, String> {
        if !self.config.enabled {
            return Ok(PsiAction::Unchanged);
        }
        let io = read_avg10(&self.config.psi_dir.join("io"), "full")?;
        let memory = read_avg10(&self.config.psi_dir.join("memory"), "some").ok();
        Ok(self.record_sample(io, memory, capacity))
    }

    pub fn record_sample(
        &mut self,
        io_full_avg10: f64,
        memory_some_avg10: Option<f64>,
        capacity: usize,
    ) -> PsiAction {
        self.last_io = Some(io_full_avg10);
        self.last_memory = memory_some_avg10;
        if self.samples.len() == self.config.window {
            self.samples.pop_front();
        }
        self.samples.push_back(io_full_avg10);
        let smoothed = self.samples.iter().sum::<f64>() / (self.samples.len() as f64);
        self.last_smoothed = Some(smoothed);

        if self.samples.len() < self.config.window {
            return PsiAction::Unchanged;
        }

        let cap = self.config.withhold_cap(capacity);
        if self.withheld > cap {
            let delta = self.withheld - cap;
            self.withheld = cap;
            return PsiAction::Released {
                delta,
                new_withheld: self.withheld,
            };
        }

        if smoothed > self.config.high {
            let target = (self.withheld + self.config.step).min(cap);
            if target > self.withheld {
                let delta = target - self.withheld;
                self.withheld = target;
                return PsiAction::Withheld {
                    delta,
                    new_withheld: self.withheld,
                };
            }
        } else if smoothed < self.config.low {
            let target = self.withheld.saturating_sub(self.config.step);
            if target < self.withheld {
                let delta = self.withheld - target;
                self.withheld = target;
                return PsiAction::Released {
                    delta,
                    new_withheld: self.withheld,
                };
            }
        }
        PsiAction::Unchanged
    }

    pub fn snapshot(&self, capacity: usize) -> PsiSnapshot {
        PsiSnapshot {
            enabled: self.config.enabled,
            io_full_avg10: self.last_io,
            io_smoothed: self.last_smoothed,
            memory_some_avg10: self.last_memory,
            high: self.config.high,
            low: self.config.low,
            step: self.config.step,
            max_withhold_fraction: self.config.max_withhold_fraction,
            withhold_cap: self.config.withhold_cap(capacity),
        }
    }
}

pub fn parse_avg10(content: &str, kind: &str) -> Result<f64, String> {
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some(kind) {
            for field in fields {
                if let Some(val) = field.strip_prefix("avg10=")
                    && let Ok(parsed) = val.parse::<f64>()
                    && parsed.is_finite()
                    && (0.0..=100.0).contains(&parsed)
                {
                    return Ok(parsed);
                }
            }
        }
    }
    Err(format!("missing or invalid {kind} avg10"))
}

pub fn read_avg10(path: &Path, kind: &str) -> Result<f64, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_avg10(&text, kind).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_pressure_lines() {
        let sample = "some avg10=12.34 avg60=5.67 avg300=1.23 total=999\nfull avg10=21.50 avg60=8.00 avg300=2.00 total=444\n";
        assert_eq!(parse_avg10(sample, "some").unwrap(), 12.34);
        assert_eq!(parse_avg10(sample, "full").unwrap(), 21.50);
        assert!(parse_avg10(sample, "other").is_err());
        assert!(parse_avg10("full avg10=101.0\n", "full").is_err());
        assert!(parse_avg10("full avg10=-1.0\n", "full").is_err());
    }

    #[test]
    fn moving_average_hysteresis_and_cap() {
        let cfg = PsiConfig {
            enabled: true,
            window: 3,
            high: 20.0,
            low: 8.0,
            step: 4,
            max_withhold_fraction: 0.75,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.withhold_cap(10), 7);
        assert_eq!(cfg.withhold_cap(48), 36);
        assert_eq!(cfg.withhold_cap(1), 0);

        let mut gov = PsiGovernor::new(cfg);
        // Warmup: first 2 samples never act.
        assert_eq!(gov.record_sample(30.0, Some(1.0), 10), PsiAction::Unchanged);
        assert_eq!(gov.record_sample(30.0, Some(1.0), 10), PsiAction::Unchanged);
        // 3rd sample -> smoothed 30 > 20 -> withhold 4 (cap 7).
        assert_eq!(
            gov.record_sample(30.0, Some(2.0), 10),
            PsiAction::Withheld {
                delta: 4,
                new_withheld: 4
            }
        );
        // Still high -> withhold remaining 3 up to cap 7.
        assert_eq!(
            gov.record_sample(25.0, Some(2.0), 10),
            PsiAction::Withheld {
                delta: 3,
                new_withheld: 7
            }
        );
        // At cap -> unchanged.
        assert_eq!(gov.record_sample(50.0, Some(2.0), 10), PsiAction::Unchanged);
        // Hysteresis band (between 8 and 20) -> unchanged.
        assert_eq!(gov.record_sample(0.0, Some(0.0), 10), PsiAction::Unchanged); // (25+50+0)/3=25 still >20 but at cap
        assert_eq!(gov.record_sample(0.0, Some(0.0), 10), PsiAction::Unchanged); // (50+0+0)/3=16.67 in [8,20]
        // Drop below low=8 -> smoothed=0 -> release 4 (7 -> 3).
        assert_eq!(
            gov.record_sample(0.0, Some(0.0), 10),
            PsiAction::Released {
                delta: 4,
                new_withheld: 3
            }
        );
        // Release remaining 3 (3 -> 0).
        assert_eq!(
            gov.record_sample(0.0, Some(0.0), 10),
            PsiAction::Released {
                delta: 3,
                new_withheld: 0
            }
        );
        assert_eq!(gov.withheld(), 0);
    }

    #[test]
    fn clamp_to_smaller_capacity() {
        let mut gov = PsiGovernor::new(PsiConfig::default());
        gov.record_sample(50.0, None, 48);
        gov.record_sample(50.0, None, 48);
        gov.record_sample(50.0, None, 48);
        gov.record_sample(50.0, None, 48);
        assert_eq!(gov.withheld(), 8);
        assert!(gov.clamp_to_capacity(8));
        assert_eq!(gov.withheld(), 6);
    }
}
