use gource_core::{PlaybackClock, Rational, RationalError, ReplayConfig, SIMULATION_HZ};

fn rational(numerator: i128, denominator: i128) -> Rational {
    Rational::new(numerator, denominator).expect("valid test rational")
}

fn sampled_rational_tick(seconds: Rational) -> u64 {
    PlaybackClock::sample_tick_rational(seconds)
        .expect("non-negative test time")
        .get()
}

fn sampled_f64_tick(seconds: f64) -> u64 {
    PlaybackClock::sample_tick(seconds)
        .expect("non-negative finite test time")
        .get()
}

#[test]
fn sample_tick_rational_floors_public_boundaries() {
    let hz = SIMULATION_HZ as i128;

    assert_eq!(sampled_rational_tick(Rational::ZERO), 0);
    assert_eq!(
        PlaybackClock::sample_tick_rational(rational(-1, 1)),
        Err(RationalError::NonFinite)
    );
    assert_eq!(sampled_rational_tick(rational(1, hz)), 1);
    assert_eq!(sampled_rational_tick(rational(119, hz)), 119);
    assert_eq!(sampled_rational_tick(Rational::ONE), SIMULATION_HZ);
}

#[test]
fn sample_tick_f64_wrapper_preserves_adjacent_boundary_floor() {
    let boundary = 1.0 / SIMULATION_HZ as f64;
    let predecessor = f64::from_bits(boundary.to_bits() - 1);
    let successor = f64::from_bits(boundary.to_bits() + 1);

    assert_eq!(sampled_f64_tick(0.0), 0);
    assert_eq!(
        PlaybackClock::sample_tick(-f64::MIN_POSITIVE),
        Err(RationalError::NonFinite)
    );
    assert_eq!(sampled_f64_tick(predecessor), 0);
    assert_eq!(sampled_f64_tick(boundary), 1);
    assert_eq!(sampled_f64_tick(successor), 1);
    assert_eq!(sampled_f64_tick(119.0 / SIMULATION_HZ as f64), 119);
    assert_eq!(sampled_f64_tick(1.0), SIMULATION_HZ);
}

#[test]
fn sample_tick_rational_floors_large_finite_values_without_epsilon() {
    let hz = SIMULATION_HZ as i128;
    let large_ticks = 1_i128 << 50;

    assert_eq!(
        sampled_rational_tick(rational(large_ticks - 1, hz)),
        (large_ticks - 1) as u64
    );
    assert_eq!(
        sampled_rational_tick(rational(large_ticks, hz)),
        large_ticks as u64
    );
    assert_eq!(
        sampled_rational_tick(rational(1_000_000_000_000, 1)),
        120_000_000_000_000
    );
    assert_eq!(sampled_f64_tick(1_000_000_000_000.0), 120_000_000_000_000);
}

#[test]
fn file_idle_ticks_ceil_is_separate_from_sample_floor() {
    let hz = SIMULATION_HZ as f64;
    let boundary = 1.0 / hz;
    let predecessor = f64::from_bits(boundary.to_bits() - 1);
    let successor = f64::from_bits(boundary.to_bits() + 1);
    let boundary_119 = 119.0 / hz;
    let predecessor_119 = f64::from_bits(boundary_119.to_bits() - 1);
    let successor_119 = f64::from_bits(boundary_119.to_bits() + 1);
    let half_tick = 1.0 / (2.0 * hz);
    let mut config = ReplayConfig {
        file_idle_seconds: Some(half_tick),
        ..ReplayConfig::default()
    };
    assert_eq!(config.file_idle_ticks(), Ok(Some(1)));
    assert_eq!(sampled_f64_tick(half_tick), 0);

    config.file_idle_seconds = Some(predecessor);
    assert_eq!(config.file_idle_ticks(), Ok(Some(1)));

    config.file_idle_seconds = Some(boundary);
    assert_eq!(config.file_idle_ticks(), Ok(Some(1)));

    config.file_idle_seconds = Some(successor);
    assert_eq!(config.file_idle_ticks(), Ok(Some(2)));

    assert_eq!(sampled_f64_tick(predecessor_119), 118);
    assert_eq!(sampled_f64_tick(boundary_119), 119);
    assert_eq!(sampled_f64_tick(successor_119), 119);

    config.file_idle_seconds = Some(predecessor_119);
    assert_eq!(config.file_idle_ticks(), Ok(Some(119)));

    config.file_idle_seconds = Some(boundary_119);
    assert_eq!(config.file_idle_ticks(), Ok(Some(119)));

    config.file_idle_seconds = Some(successor_119);
    assert_eq!(config.file_idle_ticks(), Ok(Some(120)));

    config.file_idle_seconds = Some(1.0);
    assert_eq!(config.file_idle_ticks(), Ok(Some(SIMULATION_HZ)));

    config.file_idle_seconds = Some(86_400.0 * 365.0);
    assert_eq!(config.file_idle_ticks(), Ok(Some(3_784_320_000)));
}
