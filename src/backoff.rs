use rand::Rng;

// Implementation from python implementation: https://github.com/GoogleCloudPlatform/cloud-profiler-python/blob/main/googlecloudprofiler/backoff.py
// Skips error based backoff - just backsoff no matter what

#[derive(Debug)]
pub struct Backoff {
    max_envelope_sec: f64,
    multiplier: f64,
    current_envelope_sec: f64,
}

impl Backoff {
    pub fn new(min_envelope_sec: f64, max_envelope_sec: f64, multiplier: f64) -> Self {
        Backoff {
            max_envelope_sec,
            multiplier,
            current_envelope_sec: min_envelope_sec,
        }
    }

    pub fn next_backoff(&mut self) -> f64 {
        let mut rng = rand::thread_rng();

        let duration = rng.gen_range(0.0..self.current_envelope_sec);
        self.current_envelope_sec = self
            .max_envelope_sec
            .min(self.current_envelope_sec * self.multiplier);

        duration
    }
}
