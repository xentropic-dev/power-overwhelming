use anyhow::{Result, bail, ensure};
use chrono::{
    Local, TimeZone, Timelike,
    format::{Fixed, Item, Numeric, StrftimeItems},
};
use std::time::Duration;

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Clock {
    format: String,
    precision: Precision,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Precision {
    Day,
    Hour,
    Minute,
    Second,
}

impl Clock {
    pub(super) fn new(format: String) -> Result<Self> {
        ensure!(
            format.len() <= 128 && !format.is_empty() && !format.chars().any(char::is_control),
            "Invalid clock format"
        );
        let mut precision = Precision::Day;
        for item in StrftimeItems::new(&format) {
            precision = precision.max(match item {
                Item::Error => bail!("Invalid clock format {format:?}"),
                Item::Numeric(Numeric::Nanosecond, _)
                | Item::Fixed(
                    Fixed::Nanosecond
                    | Fixed::Nanosecond3
                    | Fixed::Nanosecond6
                    | Fixed::Nanosecond9
                    | Fixed::Internal(_),
                ) => {
                    bail!("Status clock supports second precision");
                }
                Item::Numeric(Numeric::Second | Numeric::Timestamp, _)
                | Item::Fixed(Fixed::RFC2822 | Fixed::RFC3339) => Precision::Second,
                Item::Numeric(Numeric::Minute, _) => Precision::Minute,
                Item::Numeric(Numeric::Hour | Numeric::Hour12, _)
                | Item::Fixed(
                    Fixed::LowerAmPm
                    | Fixed::UpperAmPm
                    | Fixed::TimezoneName
                    | Fixed::TimezoneOffset
                    | Fixed::TimezoneOffsetColon
                    | Fixed::TimezoneOffsetDoubleColon
                    | Fixed::TimezoneOffsetTripleColon
                    | Fixed::TimezoneOffsetZ
                    | Fixed::TimezoneOffsetColonZ,
                ) => Precision::Hour,
                _ => Precision::Day,
            });
        }
        Ok(Self { format, precision })
    }

    pub fn value(&self) -> String {
        Local::now().format(&self.format).to_string()
    }

    pub fn next_tick(&self) -> Duration {
        let now = Local::now();
        let seconds = match self.precision {
            Precision::Second => 1,
            Precision::Minute => 60 - now.second(),
            Precision::Hour => 3600 - now.minute() * 60 - now.second(),
            Precision::Day => {
                if let Some(midnight) = now
                    .date_naive()
                    .succ_opt()
                    .and_then(|day| day.and_hms_opt(0, 0, 0))
                    .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
                    && let Ok(delay) = (midnight - now).to_std()
                {
                    return delay;
                }
                3600
            }
        };
        Duration::from_secs(seconds.into()) - Duration::from_nanos(now.nanosecond().into())
    }
}
