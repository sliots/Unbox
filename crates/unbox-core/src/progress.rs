//! Throughput estimation and the copy-dialog style area graph model.

use std::collections::VecDeque;

/// Seconds of history the throughput is averaged over.
const WINDOW: f64 = 2.0;

/// Throughput in MB/s, averaged over a sliding time window so bursty progress reports
/// (one per file, or one per 7-Zip callback) do not make it jump around.
#[derive(Debug, Default)]
pub struct SpeedMeter {
    samples: VecDeque<(f64, u64)>,
    last_display_time: f64,
    /// Windowed speed.
    pub current: f64,
    /// Speed shown as text, refreshed at most every 500 ms to avoid flicker.
    pub displayed: f64,
}

impl SpeedMeter {
    pub fn reset(&mut self, now: f64, bytes: u64) {
        *self = SpeedMeter { samples: VecDeque::from([(now, bytes)]), last_display_time: now, current: 0.0, displayed: 0.0 };
    }

    /// Restart the rate window without losing the shown speed (after resume).
    pub fn rebase(&mut self, now: f64, bytes: u64) {
        self.samples = VecDeque::from([(now, bytes)]);
        self.last_display_time = now;
    }

    pub fn sample(&mut self, now: f64, completed: u64, paused: bool) {
        match self.samples.back() {
            Some(&(_, bytes)) if completed < bytes => self.samples = VecDeque::from([(now, completed)]),
            Some(&(time, _)) if now - time < 0.03 => {}
            _ => self.samples.push_back((now, completed)),
        }
        // Keep exactly one sample at or before the window start.
        while self.samples.len() > 2 && now - self.samples[1].0 >= WINDOW {
            self.samples.pop_front();
        }
        if let Some(&(start, bytes)) = self.samples.front() {
            let elapsed = now - start;
            if elapsed >= 0.25 {
                self.current = (completed - bytes) as f64 / (1024.0 * 1024.0) / elapsed;
            }
        }
        if now - self.last_display_time >= 0.5 || self.displayed <= 0.0 || paused {
            self.displayed = self.current;
            self.last_display_time = now;
        }
    }
}

/// Seconds for the remaining-time estimate to take in a change of speed.
const ETA_SETTLE: f64 = 4.0;
/// The shown remaining time changes at most this often (seconds).
const ETA_REFRESH: f64 = 1.0;

/// Remaining time that counts down steadily instead of following every swing of the
/// speed: between reports the estimate runs down with the clock, and a new speed only
/// pulls it gradually toward the new figure.
#[derive(Debug, Default)]
pub struct EtaEstimator {
    estimate: Option<f64>,
    last: f64,
    shown: f64,
    shown_at: f64,
}

impl EtaEstimator {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Resume after a pause: the paused time does not count down the estimate.
    pub fn rebase(&mut self, now: f64) {
        self.last = now;
        self.shown_at = now;
    }

    /// `raw`: remaining work divided by the current speed, in seconds. Returns the
    /// seconds to show.
    pub fn update(&mut self, now: f64, raw: f64) -> f64 {
        let estimate = match self.estimate {
            None => raw,
            Some(previous) => {
                let elapsed = (now - self.last).max(0.0);
                let predicted = (previous - elapsed).max(0.0);
                predicted + (raw - predicted) * (1.0 - (-elapsed / ETA_SETTLE).exp())
            }
        };
        let first = self.estimate.is_none();
        self.estimate = Some(estimate);
        self.last = now;
        if first || now - self.shown_at >= ETA_REFRESH {
            self.shown = estimate;
            self.shown_at = now;
        }
        self.shown
    }
}

const BUCKETS: usize = 120;

/// Horizontal "stacked area" graph: x is overall progress, y is throughput. Each
/// bucket holds the average speed over its slice of progress; finished buckets never
/// change, and the scale only grows, so the curve settles instead of twitching.
#[derive(Debug, Clone)]
pub struct SpeedGraph {
    buckets: [f64; BUCKETS],
    last_bucket: usize,
    bucket_sum: f64,
    bucket_samples: u32,
    peak: f64,
    pub max_scale: f64,
    pub progress: f64,
    pub speed: f64,
    pub display_speed: f64,
}

impl Default for SpeedGraph {
    fn default() -> Self {
        Self {
            buckets: [0.0; BUCKETS],
            last_bucket: 0,
            bucket_sum: 0.0,
            bucket_samples: 0,
            peak: 0.0,
            max_scale: 10.0,
            progress: 0.0,
            speed: 0.0,
            display_speed: 0.0,
        }
    }
}

fn nice_scale(value: f64) -> f64 {
    for step in [10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0] {
        if value <= step {
            return step;
        }
    }
    (value / 200.0).ceil() * 200.0
}

impl SpeedGraph {
    pub fn push(&mut self, progress: f64, speed: f64, display_speed: f64) {
        let p = progress.clamp(0.0, 1.0);
        let bucket = ((p * (BUCKETS - 1) as f64) as usize).max(self.last_bucket);
        if speed > 0.05 {
            if self.peak <= 0.0 {
                // The first real sample flattens the history so the graph never starts
                // with a slanted triangle.
                self.buckets[..=bucket].fill(speed);
                self.bucket_sum = 0.0;
                self.bucket_samples = 0;
            } else if bucket != self.last_bucket {
                // Progress skipped past buckets: they ran at the current speed. A new
                // bucket starts from the previous one's average so the tip does not jump.
                let previous = self.buckets[self.last_bucket];
                self.buckets[self.last_bucket + 1..=bucket].fill(speed);
                self.bucket_sum = previous;
                self.bucket_samples = 1;
            }
            self.bucket_sum += speed;
            self.bucket_samples += 1;
            self.buckets[bucket] = self.bucket_sum / self.bucket_samples as f64;
            self.last_bucket = bucket;
            self.peak = self.peak.max(self.buckets[bucket]).max(display_speed);
        }
        self.max_scale = self.max_scale.max(nice_scale((self.peak * 1.3).max(10.0)));
        self.speed = speed;
        self.display_speed = if display_speed > 0.0 { display_speed } else { speed };
        self.progress = p;
    }

    fn y_for(&self, value: f64, h: f64) -> f64 {
        let v = value.clamp(0.0, self.max_scale);
        h - (v / self.max_scale * (h - 10.0)) - 4.0
    }

    /// SVG path commands for the filled area in a `w`×`h` box; empty before progress.
    pub fn area_path(&self, w: f64, h: f64) -> String {
        let prog_x = self.progress * w;
        if prog_x <= 0.5 {
            return String::new();
        }
        let current = (self.progress * (BUCKETS - 1) as f64) as usize;
        // The tip follows the averaged bucket, not the instantaneous rate.
        let active = if self.buckets[self.last_bucket] > 0.0 { self.buckets[self.last_bucket] } else { self.peak };
        let mut d = format!("M 0 {h:.2}");
        for i in 0..=current {
            let x = (i as f64 / (BUCKETS - 1) as f64 * w).min(prog_x);
            let speed = if self.buckets[i] > 0.0 { self.buckets[i] } else { active };
            // A subtle ±2.5% ripple gives the area a natural disk-throughput texture.
            let ripple = (i as f64 * 0.8).sin() * 0.025 * speed;
            d.push_str(&format!(" L {x:.2} {:.2}", self.y_for(speed + ripple, h)));
        }
        d.push_str(&format!(" L {prog_x:.2} {:.2} L {prog_x:.2} {h:.2} Z", self.y_for(active, h)));
        d
    }

    /// The shown speed (for the reference line and label).
    pub fn shown_speed(&self) -> f64 {
        let s = if self.display_speed > 0.0 {
            self.display_speed
        } else if self.speed > 0.0 {
            self.speed
        } else {
            self.peak
        };
        s.clamp(0.0, self.max_scale)
    }

    /// Y of the speed reference line in a box of height `h`.
    pub fn reference_y(&self, h: f64) -> f64 {
        self.y_for(self.shown_speed(), h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_smooths() {
        let mut m = SpeedMeter::default();
        m.reset(0.0, 0);
        m.sample(1.0, 10 << 20, false);
        assert!((m.current - 10.0).abs() < 1e-9);
        m.sample(2.0, 30 << 20, false);
        assert!((m.current - 15.0).abs() < 1e-9);
        assert!((m.displayed - 15.0).abs() < 1e-9);
        // Older samples leave the 2 s window.
        m.sample(3.0, 60 << 20, false);
        assert!((m.current - 25.0).abs() < 1e-9);
        // Bursty reports inside the window do not swing the rate.
        m.sample(3.05, 80 << 20, false);
        m.sample(3.10, 80 << 20, false);
        assert!(m.current > 30.0 && m.current < 37.0, "{}", m.current);
    }

    #[test]
    fn graph_scales_and_paths() {
        let mut g = SpeedGraph::default();
        assert!(g.area_path(100.0, 50.0).is_empty());
        g.push(0.5, 40.0, 40.0);
        assert_eq!(g.max_scale, 100.0);
        let path = g.area_path(100.0, 50.0);
        assert!(path.starts_with("M 0 50.00") && path.ends_with('Z'));
        assert!(g.reference_y(50.0) < 50.0);
        g.push(0.6, 2000.0, 2000.0);
        assert_eq!(g.max_scale, 2600.0);
        // The scale never shrinks back.
        g.push(0.7, 5.0, 5.0);
        assert_eq!(g.max_scale, 2600.0);
    }

    #[test]
    fn eta_counts_down_through_speed_swings() {
        let mut eta = EtaEstimator::default();
        let mut shown = Vec::new();
        let mut t = 0.0;
        // 100 s of work at an average of 1 unit/s, reported every 100 ms with ±40% noise.
        let mut remaining = 100.0;
        let mut seed = 7u64;
        while remaining > 10.0 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let noise = 0.6 + 0.8 * ((seed >> 33) as f64 / (1u64 << 31) as f64);
            t += 0.1;
            remaining -= 0.1;
            shown.push(eta.update(t, remaining / noise));
        }
        // The shown value changes at most once a second…
        let changes = shown.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(changes <= (t as usize) + 1, "{changes} changes in {t} s");
        // …and each change stays close to a steady countdown.
        let jumps = shown.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max);
        assert!(jumps < 8.0, "jump of {jumps} s");
    }

    #[test]
    fn tip_is_averaged_within_a_bucket() {
        let mut g = SpeedGraph::default();
        g.push(0.5, 100.0, 100.0);
        let y0 = g.area_path(120.0, 50.0);
        // A wild swing inside the same bucket moves the tip by its share only.
        g.push(0.5, 20.0, 100.0);
        let tip = |path: &str| path.rsplit(" L ").nth(1).unwrap().split(' ').nth(1).unwrap().parse::<f64>().unwrap();
        let (before, after) = (tip(&y0), tip(&g.area_path(120.0, 50.0)));
        assert!((after - before).abs() < (g.y_for(20.0, 50.0) - g.y_for(100.0, 50.0)) * 0.6);
    }
}
