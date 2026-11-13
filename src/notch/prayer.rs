//! Low-power prayer schedule and reminders.
//!
//! The schedule is calculated locally once per day. Windows Location is read
//! only when the user presses the detect button in Settings.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator};
use windows::Foundation::TimeSpan;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

use crate::config::PrayerConfig;
use crate::notch::notify::{self, NotificationLevel};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrayerTime {
    pub name: &'static str,
    pub minute: i32,
}

struct Watch {
    last_poll: Option<Instant>,
    announced: Option<(i32, u16, u16, usize)>,
}

static WATCH: Mutex<Watch> = Mutex::new(Watch {
    last_poll: None,
    announced: None,
});

type ScheduleKey = (i32, u16, u16, u64, u64, u64, i32);
static TODAY_CACHE: Mutex<Option<(ScheduleKey, [PrayerTime; 5])>> = Mutex::new(None);
static LOCATING: AtomicBool = AtomicBool::new(false);
#[derive(Debug, Clone)]
pub struct DetectedLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub label: Option<String>,
}

static DETECTED: Mutex<Option<Result<DetectedLocation, String>>> = Mutex::new(None);

/// Ask for permission on the foreground settings thread, then perform the
/// potentially slow sensor read on a worker.
pub fn request_auto_location() -> Result<(), String> {
    if LOCATING.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let access = match Geolocator::RequestAccessAsync().and_then(|op| op.get()) {
        Ok(access) => access,
        Err(error) => {
            LOCATING.store(false, Ordering::Release);
            return Err(error.message());
        }
    };
    if access != GeolocationAccessStatus::Allowed {
        LOCATING.store(false, Ordering::Release);
        return Err("Windows location permission was not granted".to_owned());
    }
    if let Err(error) = std::thread::Builder::new()
        .name("Nova location".to_owned())
        .spawn(|| {
            let result = (|| {
                let locator = Geolocator::new().map_err(|e| e.message())?;
                let position = locator
                    .GetGeopositionAsyncWithAgeAndTimeout(
                        TimeSpan {
                            Duration: 300_000_000,
                        },
                        TimeSpan {
                            Duration: 200_000_000,
                        },
                    )
                    .and_then(|op| op.get())
                    .map_err(|e| e.message())?;
                let coordinate = position.Coordinate().map_err(|e| e.message())?;
                let point = coordinate.Point().map_err(|e| e.message())?;
                let basic = point.Position().map_err(|e| e.message())?;
                let label = position.CivicAddress().ok().and_then(|address| {
                    let city = address.City().ok()?.to_string_lossy().trim().to_owned();
                    let country = address.Country().ok()?.to_string_lossy().trim().to_owned();
                    match (city.is_empty(), country.is_empty()) {
                        (false, false) => Some(format!("{city}, {country}")),
                        (false, true) => Some(city),
                        (true, false) => Some(country),
                        (true, true) => None,
                    }
                });
                Ok(DetectedLocation {
                    latitude: basic.Latitude,
                    longitude: basic.Longitude,
                    label,
                })
            })();
            *DETECTED.lock() = Some(result);
            LOCATING.store(false, Ordering::Release);
        })
    {
        LOCATING.store(false, Ordering::Release);
        return Err(error.to_string());
    }
    Ok(())
}

pub fn location_status() -> String {
    if LOCATING.load(Ordering::Acquire) {
        "Detecting location…".to_owned()
    } else if let Some(result) = DETECTED.lock().as_ref() {
        match result {
            Ok(found) => found.label.clone().unwrap_or_else(|| {
                format!("Detected {:.4}, {:.4}", found.latitude, found.longitude)
            }),
            Err(error) => format!("Location unavailable: {error}"),
        }
    } else {
        "Uses Windows Location Services only when requested.".to_owned()
    }
}

pub fn take_detected_location() -> Option<Result<DetectedLocation, String>> {
    DETECTED.lock().take()
}

/// Kemenag-compatible schedule (20° Fajr, 18° Isha, standard Asr).
pub fn schedule(
    year: i32,
    month: u16,
    day: u16,
    latitude: f64,
    longitude: f64,
    timezone: f64,
    adjustment: i32,
) -> [PrayerTime; 5] {
    let jd = julian_date(year, month as i32, day as i32) - longitude / (15.0 * 24.0);
    let mut times = [5.0, 6.0, 12.0, 13.0, 18.0, 18.0, 18.0];
    for _ in 0..2 {
        let t = times.map(|value| value / 24.0);
        let noon = midday(jd, t[2]);
        times = [
            sun_angle_time(jd, latitude, 20.0, t[0], true),
            sun_angle_time(jd, latitude, 0.833, t[1], true),
            noon,
            asr_time(jd, latitude, 1.0, t[3]),
            sun_angle_time(jd, latitude, 0.833, t[4], false),
            sun_angle_time(jd, latitude, 0.833, t[5], false),
            sun_angle_time(jd, latitude, 18.0, t[6], false),
        ];
    }
    for time in &mut times {
        *time += timezone - longitude / 15.0;
    }
    let minute =
        |hours: f64| -> i32 { ((hours * 60.0).round() as i32 + adjustment).rem_euclid(24 * 60) };
    [
        PrayerTime {
            name: "Subuh",
            minute: minute(times[0]),
        },
        PrayerTime {
            name: "Zuhur",
            minute: minute(times[2] + 2.0 / 60.0),
        },
        PrayerTime {
            name: "Asar",
            minute: minute(times[3]),
        },
        PrayerTime {
            name: "Magrib",
            minute: minute(times[4]),
        },
        PrayerTime {
            name: "Isya",
            minute: minute(times[6]),
        },
    ]
}

/// Called every overlay tick; wakes only every 20 seconds.
pub fn tick(cfg: &PrayerConfig, toast_secs: f32) {
    if !cfg.enabled {
        return;
    }
    let mut watch = WATCH.lock();
    if watch
        .last_poll
        .is_some_and(|last| last.elapsed() < Duration::from_secs(20))
    {
        return;
    }
    watch.last_poll = Some(Instant::now());
    let local = unsafe { GetLocalTime() };
    let utc = unsafe { GetSystemTime() };
    let timezone = utc_offset_hours(&local, &utc);
    let prayers = schedule(
        local.wYear as i32,
        local.wMonth,
        local.wDay,
        cfg.latitude,
        cfg.longitude,
        timezone,
        cfg.adjustment_minutes,
    );
    let now_minute = local.wHour as i32 * 60 + local.wMinute as i32;
    let due = prayers.iter().enumerate().find(|(_, prayer)| {
        let elapsed = (now_minute - prayer.minute).rem_euclid(24 * 60);
        elapsed <= 1
    });
    let Some((index, prayer)) = due else { return };
    let key = (local.wYear as i32, local.wMonth, local.wDay, index);
    if watch.announced == Some(key) {
        return;
    }
    watch.announced = Some(key);
    drop(watch);

    let place = cfg.location_name.trim();
    let body = if place.is_empty() {
        "Waktu salat telah masuk.".to_owned()
    } else {
        format!("Waktu salat untuk {place} telah masuk.")
    };
    notify::global_store().write().push(
        "Prayer Times",
        &format!("Waktu {}", prayer.name),
        &body,
        NotificationLevel::Info,
        toast_secs.max(6.0),
        &[],
    );
}

pub fn today_schedule(cfg: &PrayerConfig) -> [PrayerTime; 5] {
    let local = unsafe { GetLocalTime() };
    let utc = unsafe { GetSystemTime() };
    let timezone = utc_offset_hours(&local, &utc);
    let key = (
        local.wYear as i32,
        local.wMonth,
        local.wDay,
        cfg.latitude.to_bits(),
        cfg.longitude.to_bits(),
        timezone.to_bits(),
        cfg.adjustment_minutes,
    );
    let mut cache = TODAY_CACHE.lock();
    if let Some((cached_key, prayers)) = *cache {
        if cached_key == key {
            return prayers;
        }
    }
    let prayers = schedule(
        local.wYear as i32,
        local.wMonth,
        local.wDay,
        cfg.latitude,
        cfg.longitude,
        timezone,
        cfg.adjustment_minutes,
    );
    *cache = Some((key, prayers));
    prayers
}

/// The next prayer is exposed on the resting clock only shortly before it is
/// due. Keeping this decision here makes the compact painter and its sizing
/// use exactly the same ten-minute window.
pub fn upcoming_within(cfg: &PrayerConfig, within_minutes: i32) -> Option<(PrayerTime, i32)> {
    if !cfg.enabled || !cfg.show_on_clock {
        return None;
    }
    let local = unsafe { GetLocalTime() };
    let now_minute = local.wHour as i32 * 60 + local.wMinute as i32;
    today_schedule(cfg)
        .into_iter()
        .filter_map(|prayer| {
            let remaining = prayer.minute - now_minute;
            (remaining >= 0 && remaining <= within_minutes).then_some((prayer, remaining))
        })
        .min_by_key(|(_, remaining)| *remaining)
}

fn utc_offset_hours(
    local: &windows::Win32::Foundation::SYSTEMTIME,
    utc: &windows::Win32::Foundation::SYSTEMTIME,
) -> f64 {
    let local_minutes = civil_days(local.wYear as i32, local.wMonth as i32, local.wDay as i32)
        * 1440
        + local.wHour as i64 * 60
        + local.wMinute as i64;
    let utc_minutes = civil_days(utc.wYear as i32, utc.wMonth as i32, utc.wDay as i32) * 1440
        + utc.wHour as i64 * 60
        + utc.wMinute as i64;
    (local_minutes - utc_minutes) as f64 / 60.0
}

fn civil_days(y: i32, m: i32, d: i32) -> i64 {
    let y = y as i64 - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d as i64 - 1
}

fn julian_date(year: i32, month: i32, day: i32) -> f64 {
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let a = (y as f64 / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (y + 4716) as f64).floor() + (30.6001 * (m + 1) as f64).floor() + day as f64 + b
        - 1524.5
}

fn sun_position(jd: f64) -> (f64, f64) {
    let d = jd - 2_451_545.0;
    let g = fix_angle(357.529 + 0.985_600_28 * d);
    let q = fix_angle(280.459 + 0.985_647_36 * d);
    let l = fix_angle(q + 1.915 * sin_deg(g) + 0.020 * sin_deg(2.0 * g));
    let e = 23.439 - 0.000_000_36 * d;
    let decl = asin_deg(sin_deg(e) * sin_deg(l));
    let ra = fix_hour(atan2_deg(cos_deg(e) * sin_deg(l), cos_deg(l)) / 15.0);
    (decl, q / 15.0 - ra)
}

fn midday(jd: f64, time: f64) -> f64 {
    fix_hour(12.0 - sun_position(jd + time).1)
}
fn sun_angle_time(jd: f64, lat: f64, angle: f64, time: f64, before_noon: bool) -> f64 {
    let decl = sun_position(jd + time).0;
    let noon = midday(jd, time);
    let ratio = (-sin_deg(angle) - sin_deg(decl) * sin_deg(lat)) / (cos_deg(decl) * cos_deg(lat));
    let delta = acos_deg(ratio.clamp(-1.0, 1.0)) / 15.0;
    noon + if before_noon { -delta } else { delta }
}
fn asr_time(jd: f64, lat: f64, factor: f64, time: f64) -> f64 {
    let decl = sun_position(jd + time).0;
    let angle = -acot_deg(factor + tan_deg((lat - decl).abs()));
    sun_angle_time(jd, lat, angle, time, false)
}
fn fix_angle(value: f64) -> f64 {
    value.rem_euclid(360.0)
}
fn fix_hour(value: f64) -> f64 {
    value.rem_euclid(24.0)
}
fn sin_deg(v: f64) -> f64 {
    v.to_radians().sin()
}
fn cos_deg(v: f64) -> f64 {
    v.to_radians().cos()
}
fn tan_deg(v: f64) -> f64 {
    v.to_radians().tan()
}
fn asin_deg(v: f64) -> f64 {
    v.asin().to_degrees()
}
fn acos_deg(v: f64) -> f64 {
    v.acos().to_degrees()
}
fn atan2_deg(y: f64, x: f64) -> f64 {
    y.atan2(x).to_degrees()
}
fn acot_deg(v: f64) -> f64 {
    (1.0 / v).atan().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jakarta_schedule_is_ordered_and_plausible() {
        let times = schedule(2026, 10, 30, -6.2, 106.816_666, 7.0, 0);
        assert!(times[0].minute > 3 * 60 && times[0].minute < 6 * 60);
        assert!(times[1].minute > 11 * 60 && times[1].minute < 13 * 60);
        assert!(times.windows(2).all(|pair| pair[0].minute < pair[1].minute));
    }
}
