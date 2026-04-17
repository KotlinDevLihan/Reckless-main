use std::time::{Duration, Instant};

use crate::thread::ThreadData;

#[derive(Clone, Debug)]
pub enum Limits {
    Infinite,
    Depth(i32),
    Time(u64),
    Nodes(u64),
    Fischer(u64, u64),
    Cyclic(u64, u64, u64),
    Ponder(Box<Limits>),
}

const TIME_OVERHEAD_MS: u64 = 15;

/// Maximum percentage of remaining time to use for a single move
const MAX_TIME_RATIO: f64 = 0.25;

/// Minimum thinking time to ensure we don't move instantly
const MIN_THINK_TIME_MS: u64 = 50;

#[derive(Clone)]
pub struct TimeManager {
    limits: Limits,
    start_time: Instant,
    soft_bound: Duration,
    hard_bound: Duration,
    /// Total time allocated for this move (for more accurate reporting)
    allocated_time: Duration,
}

impl TimeManager {
    pub fn new(limits: Limits, fullmove_number: usize, move_overhead: u64) -> Self {
        let (soft, hard, real_limits) = Self::compute_bounds(&limits, fullmove_number, move_overhead);

        let soft_dur = Duration::from_millis(soft.saturating_sub(TIME_OVERHEAD_MS));
        let hard_dur = Duration::from_millis(hard.saturating_sub(TIME_OVERHEAD_MS));

        Self {
            limits: real_limits,
            start_time: Instant::now(),
            soft_bound: soft_dur,
            hard_bound: hard_dur,
            allocated_time: soft_dur,
        }
    }

    fn compute_bounds(limits: &Limits, fullmove_number: usize, move_overhead: u64) -> (u64, u64, Limits) {
        match limits {
            Limits::Time(ms) => (*ms, *ms, limits.clone()),
            Limits::Fischer(main, inc) => {
                let main = (*main).saturating_sub(move_overhead);
                let inc = *inc;

                // Improved time allocation with better scaling
                // Scale down as game progresses to save time for later
                let move_number_factor = if fullmove_number <= 10 {
                    1.2 // Use more time in opening
                } else if fullmove_number <= 40 {
                    1.0 // Standard time in middlegame
                } else {
                    0.85 - 0.005 * (fullmove_number as f64 - 40.0).min(20.0) // Use less time in endgame
                };

                // Base time calculation: use a portion of remaining time plus increment
                let base_time = main as f64 * 0.035 * move_number_factor;
                let inc_bonus = inc as f64 * 0.6; // Use 60% of increment

                let soft_bound = (base_time + inc_bonus).max(MIN_THINK_TIME_MS as f64) as u64;
                let hard_bound = (soft_bound as f64 * 4.0).min(main as f64 * MAX_TIME_RATIO) as u64;

                // Cap soft bound to ensure we don't use too much time
                let soft = soft_bound.min(main);
                let hard = hard_bound.max(soft + 100).min(main);

                (soft, hard, limits.clone())
            }
            Limits::Cyclic(main, inc, moves_to_go) => {
                let main = (*main).saturating_sub(move_overhead);
                let inc = *inc;
                let moves = *moves_to_go;

                // Calculate base time per move with safety margin
                let base_per_move = if moves > 0 {
                    main as f64 / moves as f64
                } else {
                    main as f64 * 0.05
                };

                // Use a portion of base time plus increment
                let soft_bound = (base_per_move * 0.8 + inc as f64 * 0.6).max(MIN_THINK_TIME_MS as f64) as u64;
                let hard_bound = (base_per_move * 3.5 + inc as f64 * 0.8).min(main as f64 * MAX_TIME_RATIO) as u64;

                let soft = soft_bound.min(main);
                let hard = hard_bound.max(soft + 100).min(main);

                (soft, hard, limits.clone())
            }
            Limits::Ponder(inner) => {
                // During pondering, we use infinite time until ponderhit
                // Keep the Ponder wrapper but return infinite time bounds
                let (_, _, inner_limits) = Self::compute_bounds(inner, fullmove_number, move_overhead);
                (u64::MAX, u64::MAX, Limits::Ponder(Box::new(inner_limits)))
            }
            _ => (u64::MAX, u64::MAX, limits.clone()),
        }
    }

    pub fn ponderhit(&mut self, limits: Limits, fullmove_number: usize, move_overhead: u64) {
        // Reset the timer with actual time limits when ponderhit is received
        if matches!(self.limits, Limits::Ponder(_)) {
            let (soft, hard, real_limits) = Self::compute_bounds(&Limits::Ponder(Box::new(limits)), fullmove_number, move_overhead);
            // Extract the real limits without the Ponder wrapper
            let (real_soft, real_hard, real_limits) = match &real_limits {
                Limits::Ponder(inner) => Self::compute_bounds(inner, fullmove_number, move_overhead),
                _ => (soft, hard, real_limits),
            };
            self.limits = real_limits;
            self.start_time = Instant::now();
            self.soft_bound = Duration::from_millis(real_soft.saturating_sub(TIME_OVERHEAD_MS));
            self.hard_bound = Duration::from_millis(real_hard.saturating_sub(TIME_OVERHEAD_MS));
            self.allocated_time = self.soft_bound;
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    pub fn soft_limit(&self, td: &ThreadData, multiplier: impl Fn() -> f32) -> bool {
        match self.limits {
            Limits::Infinite | Limits::Depth(_) => false,
            Limits::Nodes(maximum) => td.shared.nodes.aggregate() >= maximum,
            Limits::Time(maximum) => self.start_time.elapsed() >= Duration::from_millis(maximum),
            _ => self.start_time.elapsed() >= Duration::from_secs_f32(self.soft_bound.as_secs_f32() * multiplier()),
        }
    }

    pub fn check_time(&self, td: &ThreadData) -> bool {
        if td.completed_depth == 0 {
            return false;
        }

        match self.limits {
            Limits::Infinite | Limits::Depth(_) => false,
            Limits::Nodes(maximum) => td.shared.nodes.aggregate() > maximum,
            _ => td.nodes() & 2047 == 2047 && self.start_time.elapsed() >= self.hard_bound,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits.clone()
    }

    /// Returns the ratio of elapsed time to allocated time (0.0 to 1.0+)
    pub fn time_usage_ratio(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        let allocated = self.allocated_time.as_secs_f64();
        if allocated > 0.0 {
            elapsed / allocated
        } else {
            0.0
        }
    }

    /// Returns true if we're in time trouble (using more than 80% of allocated time)
    pub fn in_time_trouble(&self) -> bool {
        self.time_usage_ratio() > 0.8
    }

    /// Returns the remaining time in the soft bound
    pub fn remaining_time(&self) -> Duration {
        self.soft_bound.saturating_sub(self.start_time.elapsed())
    }
}
