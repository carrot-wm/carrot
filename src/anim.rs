// -- animation math: curves, springs, the Anim scalar --

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl CubicBezier {
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> CubicBezier {
        // x control points stay in range so the t-for-x solve converges
        CubicBezier {
            x1: x1.clamp(0.0, 1.0),
            y1,
            x2: x2.clamp(0.0, 1.0),
            y2,
        }
    }

    // one axis of a cubic with P0=0, P3=1
    fn sample(a: f64, b: f64, t: f64) -> f64 {
        let omt = 1.0 - t;
        3.0 * omt * omt * t * a + 3.0 * omt * t * t * b + t * t * t
    }

    pub fn y_for_x(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        if x == 0.0 || x == 1.0 {
            return x;
        }
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        let mut t = x;
        for _ in 0..30 {
            let cur = Self::sample(self.x1, self.x2, t);
            if (cur - x).abs() < 1e-7 {
                break;
            }
            if cur < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        Self::sample(self.y1, self.y2, t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
    Bezier(CubicBezier),
}

impl Curve {
    pub fn y(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Linear => x,
            Curve::EaseOutQuad => 1.0 - (1.0 - x) * (1.0 - x),
            Curve::EaseOutCubic => 1.0 - (1.0 - x).powi(3),
            Curve::EaseOutExpo => {
                if x >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * x)
                }
            }
            Curve::Bezier(b) => b.y_for_x(x),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringK {
    pub beta: f64,   // damping / 2m
    pub omega0: f64, // sqrt(k/m)
    pub epsilon: f64,
}

impl SpringK {
    pub fn new(damping_ratio: f64, stiffness: f64, epsilon: f64) -> SpringK {
        // mass = 1: critical damping 2*sqrt(k), so beta = ratio*sqrt(k)
        let omega0 = stiffness.max(1.0).sqrt();
        SpringK {
            beta: damping_ratio.max(0.0) * omega0,
            omega0,
            epsilon,
        }
    }
}

// displacement from target at t seconds given start offset x0 and velocity
// v0: the analytic solution of x'' + 2*beta*x' + omega0^2 * x = 0. the
// branch test uses f32's epsilon - f64's is too tight to ever go critical
pub(crate) fn spring_pos(k: &SpringK, x0: f64, v0: f64, t: f64) -> f64 {
    let env = (-k.beta * t).exp();
    if (k.beta - k.omega0).abs() <= f32::EPSILON as f64 {
        env * (x0 + (k.beta * x0 + v0) * t)
    } else if k.beta < k.omega0 {
        let w1 = (k.omega0 * k.omega0 - k.beta * k.beta).sqrt();
        env * (x0 * (w1 * t).cos() + ((k.beta * x0 + v0) / w1) * (w1 * t).sin())
    } else {
        let w2 = (k.beta * k.beta - k.omega0 * k.omega0).sqrt();
        env * (x0 * (w2 * t).cosh() + ((k.beta * x0 + v0) / w2) * (w2 * t).sinh())
    }
}

pub(crate) fn spring_duration(k: &SpringK, x0: f64, v0: f64) -> f64 {
    let eps = k.epsilon.clamp(1e-7, 0.5);
    if k.beta <= k.omega0 {
        // the e^(-beta t) envelope crossing epsilon
        return -eps.ln() / k.beta.max(1e-6);
    }
    // overdamped decays slower than its envelope; grow from the envelope guess
    let mut t = -eps.ln() / k.beta.max(1e-6);
    for _ in 0..100 {
        if spring_pos(k, x0, v0, t).abs() <= eps * x0.abs().max(1e-9) {
            break;
        }
        t *= 1.5;
    }
    t
}

pub(crate) fn spring_clamped(k: &SpringK, x0: f64, v0: f64) -> f64 {
    // first touch of the target: millisecond stepping, capped at 3s. a
    // sign flip between grid points is a touch too - the crossing itself
    // can dodge the epsilon band entirely
    let dur = spring_duration(k, x0, v0);
    let eps = k.epsilon.clamp(1e-7, 0.5) * x0.abs().max(1e-9);
    let mut t = 0.0;
    let mut prev = x0;
    while t < dur.min(3.0) {
        let pos = spring_pos(k, x0, v0, t);
        if pos.abs() <= eps || pos.signum() != prev.signum() {
            return t;
        }
        prev = pos;
        t += 0.001;
    }
    dur.min(3.0)
}

use std::cell::Cell;

pub struct AnimClock {
    now_ns: Cell<u64>,
    off: Cell<bool>,
    slowdown: Cell<f64>,
}

impl AnimClock {
    pub fn new() -> AnimClock {
        AnimClock {
            now_ns: Cell::new(0),
            off: Cell::new(false),
            slowdown: Cell::new(1.0),
        }
    }
    pub fn freeze(&self, ns: u64) {
        self.now_ns.set(ns);
    }
    /// forward-only bump to real monotonic time; anim starts happen in
    /// event context where the compose-frozen stamp can be seconds stale
    pub fn touch(&self) {
        let now = crate::util::Time::now().nsec();
        self.now_ns.set(self.now_ns.get().max(now));
    }
    pub fn now(&self) -> u64 {
        self.now_ns.get()
    }
    pub fn set_global(&self, off: bool, slowdown: f64) {
        self.off.set(off);
        self.slowdown.set(slowdown.clamp(0.001, 100.0));
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Ease(Curve),
    Spring(SpringK, f64), // params, v0
}

#[derive(Clone, Debug)]
pub struct Anim {
    from: f64,
    to: f64,
    start: u64,
    dur_ns: u64,
    clamped_ns: u64,
    scale: f64, // 1/slowdown, captured at construction
    kind: Kind,
}

impl Anim {
    pub fn ease(clock: &AnimClock, from: f64, to: f64, ms: u32, curve: Curve) -> Anim {
        let dur = if clock.off.get() { 0 } else { ms as u64 * 1_000_000 };
        Anim {
            from,
            to,
            start: clock.now(),
            dur_ns: dur,
            clamped_ns: dur,
            scale: 1.0 / clock.slowdown.get(),
            kind: Kind::Ease(curve),
        }
    }

    pub fn spring(clock: &AnimClock, from: f64, to: f64, v0: f64, k: SpringK) -> Anim {
        let x0 = from - to;
        let (dur, clamped) = if clock.off.get() || x0 == 0.0 {
            (0.0, 0.0)
        } else {
            (spring_duration(&k, x0, v0), spring_clamped(&k, x0, v0))
        };
        Anim {
            from,
            to,
            start: clock.now(),
            dur_ns: (dur * 1e9) as u64,
            clamped_ns: (clamped * 1e9) as u64,
            scale: 1.0 / clock.slowdown.get(),
            kind: Kind::Spring(k, v0),
        }
    }

    pub fn to(&self) -> f64 {
        self.to
    }

    fn elapsed_ns(&self, now: u64) -> u64 {
        (now.saturating_sub(self.start) as f64 * self.scale) as u64
    }

    pub fn is_done(&self, now: u64) -> bool {
        self.elapsed_ns(now) >= self.dur_ns
    }

    pub fn value(&self, now: u64) -> f64 {
        let el = self.elapsed_ns(now);
        if el >= self.dur_ns {
            return self.to;
        }
        match &self.kind {
            Kind::Ease(c) => {
                let x = el as f64 / self.dur_ns.max(1) as f64;
                self.from + (self.to - self.from) * c.y(x)
            }
            Kind::Spring(k, v0) => {
                let t = el as f64 / 1e9;
                let v = self.to + spring_pos(k, self.from - self.to, *v0, t);
                // numerical guard: never fly further than 10x the range
                let r = (self.to - self.from).abs().max(1e-9) * 10.0;
                v.clamp(self.from.min(self.to) - r, self.from.max(self.to) + r)
            }
        }
    }

    pub fn clamped_value(&self, now: u64) -> f64 {
        if self.elapsed_ns(now) >= self.clamped_ns {
            self.to
        } else {
            self.value(now)
        }
    }

    pub fn offset(&mut self, d: f64) {
        self.from += d;
        self.to += d;
    }

    /// units per second, finite difference over 1ms
    pub fn velocity(&self, now: u64) -> f64 {
        (self.value(now + 1_000_000) - self.value(now)) * 1000.0
    }
}

// -- oklab color lerp --

fn srgb_to_lin(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn lin_to_srgb(c: f64) -> f64 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn to_oklab(rgb: [f32; 4]) -> [f64; 3] {
    let (r, g, b) = (
        srgb_to_lin(rgb[0] as f64),
        srgb_to_lin(rgb[1] as f64),
        srgb_to_lin(rgb[2] as f64),
    );
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

fn from_oklab(lab: [f64; 3], alpha: f32) -> [f32; 4] {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2]).powi(3);
    let r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s;
    let g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s;
    let b = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s;
    [
        lin_to_srgb(r.max(0.0)).clamp(0.0, 1.0) as f32,
        lin_to_srgb(g.max(0.0)).clamp(0.0, 1.0) as f32,
        lin_to_srgb(b.max(0.0)).clamp(0.0, 1.0) as f32,
        alpha,
    ]
}

pub fn lerp_oklab(a: [f32; 4], b: [f32; 4], t: f64) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    if t == 0.0 {
        return a;
    }
    if t == 1.0 {
        return b;
    }
    let (la, lb) = (to_oklab(a), to_oklab(b));
    let lab = [
        la[0] + (lb[0] - la[0]) * t,
        la[1] + (lb[1] - la[1]) * t,
        la[2] + (lb[2] - la[2]) * t,
    ];
    let alpha = (a[3] as f64 + (b[3] as f64 - a[3] as f64) * t) as f32;
    from_oklab(lab, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_endpoints() {
        for c in [
            Curve::Linear,
            Curve::EaseOutQuad,
            Curve::EaseOutCubic,
            Curve::EaseOutExpo,
            Curve::Bezier(CubicBezier::new(0.05, 0.9, 0.1, 1.05)),
        ] {
            assert_eq!(c.y(0.0), 0.0, "{c:?} at 0");
            assert!((c.y(1.0) - 1.0).abs() < 1e-9, "{c:?} at 1");
        }
    }

    #[test]
    fn bezier_diagonal_is_linear() {
        // control points on the diagonal collapse to y = x
        let b = CubicBezier::new(0.3, 0.3, 0.7, 0.7);
        for i in 0..=10 {
            let x = i as f64 / 10.0;
            assert!((b.y_for_x(x) - x).abs() < 1e-4, "x={x}");
        }
    }

    #[test]
    fn bezier_monotone_in_x() {
        let b = CubicBezier::new(0.05, 0.9, 0.1, 1.05);
        let mut prev = b.y_for_x(0.0);
        for i in 1..=1000 {
            let y = b.y_for_x(i as f64 / 1000.0);
            // y may overshoot 1.0 (that's the point of this curve) but the
            // x-solve must be stable and continuous; slope tops out ~18 at
            // the origin, so a 0.001 step moves y at most ~0.02
            assert!(y.is_finite());
            assert!((y - prev).abs() < 0.05);
            prev = y;
        }
    }

    #[test]
    fn spring_critical_monotone() {
        // damping-ratio 1.0, stiffness 800: beta == omega0 == sqrt(800)
        let k = SpringK::new(1.0, 800.0, 0.0001);
        assert!((k.beta - 800f64.sqrt()).abs() < 1e-9);
        let mut prev = spring_pos(&k, 1.0, 0.0, 0.0);
        assert_eq!(prev, 1.0);
        for i in 1..=200 {
            let x = spring_pos(&k, 1.0, 0.0, i as f64 * 0.005);
            assert!(x <= prev + 1e-12, "no overshoot when critically damped");
            assert!(x >= -1e-12);
            prev = x;
        }
    }

    #[test]
    fn spring_under_overshoots_over_does_not() {
        let under = SpringK::new(0.5, 800.0, 0.0001);
        let over = SpringK::new(2.0, 800.0, 0.0001);
        let crossed = (1..=400).any(|i| spring_pos(&under, 1.0, 0.0, i as f64 * 0.0025) < 0.0);
        assert!(crossed, "underdamped must cross the target");
        let crossed = (1..=400).any(|i| spring_pos(&over, 1.0, 0.0, i as f64 * 0.0025) < -1e-9);
        assert!(!crossed, "overdamped must not cross");
    }

    #[test]
    fn spring_durations() {
        let k = SpringK::new(1.0, 800.0, 0.0001);
        // envelope threshold: -ln(eps)/beta
        let expect = -(0.0001f64.ln()) / k.beta;
        let d = spring_duration(&k, 1.0, 0.0);
        assert!((d - expect).abs() < 1e-6);
        // the envelope estimate leaves the critical branch's (1 + beta*t)
        // polynomial as residual: exactly eps*(1 - ln eps). value() snaps
        // to the target at duration, so this never renders
        assert!(spring_pos(&k, 1.0, 0.0, d).abs() <= 0.0001 * (1.0 - 0.0001f64.ln()) * 1.001);
        assert!(spring_clamped(&k, 1.0, 0.0) <= d + 1e-9);
        // underdamped touches target well before it rests
        let u = SpringK::new(0.5, 800.0, 0.0001);
        assert!(spring_clamped(&u, 1.0, 0.0) < spring_duration(&u, 1.0, 0.0));
    }

    #[test]
    fn anim_ease_lifecycle() {
        let clock = AnimClock::new();
        clock.freeze(1_000_000_000);
        let a = Anim::ease(&clock, 10.0, 20.0, 100, Curve::Linear);
        assert_eq!(a.value(1_000_000_000), 10.0);
        assert!((a.value(1_050_000_000) - 15.0).abs() < 1e-9);
        assert_eq!(a.value(1_100_000_000), 20.0);
        assert!(!a.is_done(1_099_000_000));
        assert!(a.is_done(1_100_000_000));
        // before start clamps to from
        assert_eq!(a.value(900_000_000), 10.0);
    }

    #[test]
    fn anim_off_completes_instantly() {
        let clock = AnimClock::new();
        clock.freeze(0);
        clock.set_global(true, 1.0);
        let a = Anim::ease(&clock, 0.0, 5.0, 1000, Curve::Linear);
        assert!(a.is_done(0));
        assert_eq!(a.value(0), 5.0);
    }

    #[test]
    fn anim_slowdown_scales_time() {
        let clock = AnimClock::new();
        clock.freeze(0);
        clock.set_global(false, 2.0);
        let a = Anim::ease(&clock, 0.0, 1.0, 100, Curve::Linear);
        // halfway at 100ms of a 2x-slowed 100ms animation
        assert!((a.value(100_000_000) - 0.5).abs() < 1e-9);
        assert!(a.is_done(200_000_000));
    }

    #[test]
    fn anim_spring_clamped_value_never_overshoots() {
        let clock = AnimClock::new();
        clock.freeze(0);
        let a = Anim::spring(&clock, 0.0, 1.0, 0.0, SpringK::new(0.5, 800.0, 0.0001));
        let mut over = false;
        for i in 0..500u64 {
            let now = i * 2_000_000; // 2ms steps
            if a.value(now) > 1.0 {
                over = true;
            }
            assert!(a.clamped_value(now) <= 1.0 + 1e-9);
        }
        assert!(over, "raw value overshoots for an underdamped spring");
    }

    #[test]
    fn anim_offset_retargets_in_place() {
        let clock = AnimClock::new();
        clock.freeze(0);
        let mut a = Anim::ease(&clock, 0.0, 10.0, 100, Curve::Linear);
        a.offset(5.0);
        assert_eq!(a.value(0), 5.0);
        assert_eq!(a.value(100_000_000), 15.0);
    }

    #[test]
    fn oklab_endpoints_and_alpha() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 0.5];
        assert_eq!(lerp_oklab(red, blue, 0.0), red);
        let end = lerp_oklab(red, blue, 1.0);
        for i in 0..4 {
            assert!((end[i] - blue[i]).abs() < 1e-4, "channel {i}");
        }
        let mid = lerp_oklab(red, blue, 0.5);
        assert!((mid[3] - 0.75).abs() < 1e-6, "alpha lerps linearly");
    }

    #[test]
    fn easing_shapes() {
        // ease-out means faster than linear early on
        assert!(Curve::EaseOutQuad.y(0.3) > 0.3);
        assert!(Curve::EaseOutCubic.y(0.3) > Curve::EaseOutQuad.y(0.3));
        assert!(Curve::EaseOutExpo.y(0.3) > Curve::EaseOutCubic.y(0.3));
        // out-of-range input clamps
        assert_eq!(Curve::Linear.y(-1.0), 0.0);
        assert_eq!(Curve::Linear.y(2.0), 1.0);
    }
}
