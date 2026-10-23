use chrono::{DateTime, Duration, Months, Utc};
use eros::{Context, Result, bail};
use std::str::FromStr;

/// Calendar months first, followed by fixed-length units.
#[derive(Clone, Debug, Default)]
pub struct Age {
    months: u32,
    seconds: i64,
}

impl Age {
    pub fn cutoff(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        Ok(now
            .checked_sub_months(Months::new(self.months))
            .and_then(|date| date.checked_sub_signed(Duration::try_seconds(self.seconds)?))
            .context("age is outside the supported calendar range")?)
    }
}

impl FromStr for Age {
    type Err = eros::ErrorUnion;

    fn from_str(input: &str) -> Result<Self> {
        let mut rest = input.trim();
        let mut age = Self::default();
        let mut terms = 0;
        while !rest.is_empty() {
            let count_end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if count_end == 0 {
                bail!("invalid age {input:?}: expected an unsigned integer and a unit");
            }
            let count: u64 = rest[..count_end]
                .parse::<u64>()
                .context("age value is too large")?;
            rest = rest[count_end..].trim_start();
            let unit_end = rest
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let unit = &rest[..unit_end];
            match unit {
                "yr" | "mo" => {
                    let months = count
                        .checked_mul(if unit == "yr" { 12 } else { 1 })
                        .and_then(|n| u32::try_from(n).ok())
                        .context("too many months")?;
                    age.months = age.months.checked_add(months).context("too many months")?;
                }
                "wk" | "d" | "h" | "min" | "s" => {
                    let multiplier = match unit {
                        "wk" => 604_800,
                        "d" => 86_400,
                        "h" => 3_600,
                        "min" => 60,
                        _ => 1,
                    };
                    let seconds = count
                        .checked_mul(multiplier)
                        .and_then(|n| i64::try_from(n).ok())
                        .context("age is too large")?;
                    age.seconds = age
                        .seconds
                        .checked_add(seconds)
                        .context("age is too large")?;
                }
                _ => bail!("unknown age unit {unit:?}; use yr, mo, wk, d, h, min, or s"),
            }
            terms += 1;
            rest = rest[unit_end..].trim_start();
        }
        if terms == 0 || (age.months == 0 && age.seconds == 0) {
            bail!("age must be nonzero, for example '2mo 3d'");
        }
        age.cutoff(Utc::now())?;
        Ok(age)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn compound_calendar_age_clamps_month_end() {
        let now = Utc.with_ymd_and_hms(2024, 3, 31, 12, 0, 0).unwrap();
        assert_eq!(
            "1mo 2d".parse::<Age>().unwrap().cutoff(now).unwrap(),
            Utc.with_ymd_and_hms(2024, 2, 27, 12, 0, 0).unwrap()
        );
        let age = "1yr 2mo 3wk 4d 5h 6min 7s".parse::<Age>().unwrap();
        assert_eq!(age.months, 14);
        assert_eq!(age.seconds, 25 * 86_400 + 5 * 3600 + 6 * 60 + 7);
        assert_eq!("2mo3d".parse::<Age>().unwrap().months, 2);
    }

    #[test]
    fn invalid_age_is_never_silently_accepted() {
        for input in [
            "",
            "0s",
            "2m",
            "-2d",
            "1.5h",
            "2days",
            "3",
            "1d junk",
            "1d -2h",
            "999999999999999999999999yr",
            "18446744073709551615wk",
        ] {
            assert!(input.parse::<Age>().is_err(), "{input}");
        }
    }
}
