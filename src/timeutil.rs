use anyhow::{anyhow, Result};

pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn format_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    if secs < MIN {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m", secs / MIN)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else if secs < DAY * 30 {
        format!("{}d", secs / DAY)
    } else if secs < DAY * 365 {
        format!("{}mo", secs / (DAY * 30))
    } else {
        format!("{}y", secs / (DAY * 365))
    }
}

pub fn age_label(last_active: Option<i64>, now: i64) -> String {
    match last_active {
        Some(ts) => format_age(now.saturating_sub(ts).max(0) as u64),
        None => "—".to_string(),
    }
}

pub fn format_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let tod = ts.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let h = tod / 3_600;
    let min = (tod % 3_600) / 60;
    let s = tod % 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}Z")
}

pub fn parse_duration(raw: &str) -> Result<i64> {
    let s = raw.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if split == 0 {
        return Err(anyhow!(
            "duration must start with a number, for example 14d, 48h, 2w"
        ));
    }
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow!("cannot parse number {num}"))?;
    let mult: i64 = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 86_400 * 7,
        _ => return Err(anyhow!("unknown unit {unit}; use s, m, h, d, or w")),
    };
    Ok(n.saturating_mul(mult))
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_unix_start() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_utc(86_400), "1970-01-02 00:00:00Z");
    }

    #[test]
    fn age_buckets() {
        assert_eq!(format_age(5), "5s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(3_600), "1h");
        assert_eq!(format_age(86_400 * 2), "2d");
        assert_eq!(format_age(86_400 * 40), "1mo");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("14d").unwrap(), 14 * 86_400);
        assert_eq!(parse_duration("48h").unwrap(), 48 * 3_600);
        assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400);
        assert!(parse_duration("x").is_err());
    }
}
