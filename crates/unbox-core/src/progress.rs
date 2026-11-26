//! Throughput estimation and the copy-dialog style area graph model.

/// Smoothed throughput in MB/s.
#[derive(Debug, Default)]
pub struct SpeedMeter {
    last_bytes: u64,
    last_time: f64,
    last_display_time: f64,
    /// Exponentially smoothed speed.
    pub current: f64,
    /// Speed shown as text, refreshed at most every 500 ms to avoid flicker.
    pub displayed: f64,
}

impl SpeedMeter {
    pub fn reset(&mut self, now: f64, bytes: u64) {
        *self = SpeedMeter { last_bytes: bytes, last_time: now, last_display_time: now, current: 0.0, displayed: 0.0 };
    }

    /// Restart the rate window without losing the shown speed (after resume).
    pub fn rebase(&mut self, now: f64, bytes: u64) {
        self.last_bytes = bytes;
        self.last_time = now;
        self.last_display_time = now;
    }

    pub fn sample(&mut self, now: f64, completed: u64, paused: bool) {
        let elapsed = now - self.last_time;
        if elapsed >= 0.03 {
            if completed >= self.last_bytes {
                let instant = (completed - self.last_bytes) as f64 / (1024.0 * 1024.0) / elapsed;
                self.current = if self.current > 0.0 { self.current * 0.75 + instant * 0.25 } else { instant };
            }
            self.last_bytes = completed;
            self.last_time = now;
        }
        if now - self.last_display_time >= 0.5 || self.displayed <= 0.0 || paused {
            self.displayed = self.current;
            self.last_display_time = now;
        }
    }
}

const BUCKETS: usize = 120;

/// Horizontal "stacked area" graph: x is overall progress, y is throughput.
#[derive(Debug, Clone)]
pub struct SpeedGraph {
    buckets: [f64; BUCKETS],
    last_bucket: usize,
    peak: f64,
    pub max_scale: f64,
    pub progress: f64,
    pub speed: f64,
    pub display_speed: f64,
}

impl Default for SpeedGraph {
    fn default() -> Self {
        Self { buckets: [0.0; BUCKETS], last_bucket: 0, peak: 0.0, max_scale: 10.0, progress: 0.0, speed: 0.0, display_speed: 0.0 }
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
        let bucket = (p * (BUCKETS - 1) as f64) as usize;
        if speed > 0.05 {
            // The first real sample (or one that dwarfs early noise) flattens the history
            // so the graph never starts with a slanted triangle.
            let from = if self.peak <= 0.0 || self.peak < speed * 0.4 { 0 } else { self.last_bucket };
            for b in &mut self.buckets[from..=bucket] {
                *b = speed;
            }
            self.last_bucket = bucket;
            self.peak = self.peak.max(speed);
        }
        let peak = speed.max(display_speed).max(self.peak);
        self.max_scale = nice_scale((peak * 1.3).max(10.0));
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
        let active = if self.speed > 0.0 { self.speed } else { self.peak };
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
        assert!((m.current - 12.5).abs() < 1e-9);
        assert!((m.displayed - 12.5).abs() < 1e-9);
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
    }
}
