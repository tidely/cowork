//! Token usage: how full a thread's context window is, and the activity
//! chart and counts on the profile page.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike as _, Days, Local, Months, NaiveTime, TimeDelta, Timelike as _};
use gpui::{Hsla, SharedString};
use gpui_component::Theme;
use rig::completion::Usage;

use crate::Thread;

/// The tokens `usage` counts: the provider's total, or the sum of its input
/// and output counts when it reports no total.
pub(crate) fn usage_tokens(usage: Usage) -> u64 {
    usage
        .total_tokens
        .unwrap_or_else(|| usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0))
}

/// How much of a thread's context window is in use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ContextUsage {
    pub(crate) tokens: u64,
    pub(crate) max_tokens: u64,
}

impl ContextUsage {
    /// `thread`'s usage, or an empty window of `new_thread_max_tokens` for a
    /// thread not created yet.
    pub(crate) fn of(thread: Option<&Thread>, new_thread_max_tokens: u64) -> Self {
        match thread {
            Some(thread) => Self {
                tokens: thread.live_context_tokens().unwrap_or(0),
                max_tokens: thread.max_tokens(),
            },
            None => Self {
                tokens: 0,
                max_tokens: new_thread_max_tokens,
            },
        }
    }

    /// How full the window is. May exceed 100 while an estimate overshoots.
    pub(crate) fn percent(self) -> f32 {
        if self.max_tokens == 0 {
            return 0.;
        }
        self.tokens as f32 * 100. / self.max_tokens as f32
    }

    /// Muted until the window is nearly full, then warning, then danger.
    pub(crate) fn color(self, theme: &Theme) -> Hsla {
        match self.percent() {
            percent if percent >= 95. => theme.danger,
            percent if percent >= 80. => theme.warning,
            _ => theme.muted_foreground,
        }
    }
}

/// A duration in its two largest units, such as `12s`, `35m 16s`, or
/// `2h 5m`.
pub(crate) fn format_stat_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// A turn's tokens, dated when its response started generating.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TokenActivity {
    pub(crate) at: SystemTime,
    /// How long the response took, across which its tokens are spread.
    pub(crate) duration: Duration,
    pub(crate) tokens: u64,
}

impl TokenActivity {
    /// The share of this turn's tokens used in each of the periods starting
    /// at `starts`, in order. The last period is open-ended, so that a clock
    /// that moved back keeps a turn in it. What was used before the first
    /// period is left out.
    pub(crate) fn spread(&self, starts: &[DateTime<Local>]) -> impl Iterator<Item = (usize, f64)> {
        let start = DateTime::<Local>::from(self.at);
        // A duration too long to date is treated as an instant.
        let end = TimeDelta::from_std(self.duration)
            .ok()
            .and_then(|duration| start.checked_add_signed(duration))
            .unwrap_or(start);
        let tokens = self.tokens as f64;
        let length = (end - start).num_microseconds().unwrap_or(i64::MAX) as f64;
        let instant = length <= 0.;
        // An instant falls wholly in the period it starts in.
        let instant_index = starts
            .partition_point(|period| *period <= start)
            .checked_sub(1);
        (0..starts.len()).filter_map(move |index| {
            if instant {
                return (Some(index) == instant_index).then_some((index, tokens));
            }
            let from = start.max(starts[index]);
            let to = starts.get(index + 1).map_or(end, |next| end.min(*next));
            let overlap = (to - from).num_microseconds().unwrap_or(i64::MAX) as f64;
            (overlap > 0.).then(|| (index, tokens * overlap / length))
        })
    }
}

/// `values` rounded to whole numbers that add up to their rounded sum, by
/// rounding up those with the largest fractions.
fn round_preserving_total(values: &mut [f64]) {
    let total = values.iter().sum::<f64>().round();
    let mut fractions: Vec<_> = values
        .iter_mut()
        .enumerate()
        .map(|(index, value)| {
            let fraction = *value - value.floor();
            *value = value.floor();
            (index, fraction)
        })
        .collect();
    let missing = (total - values.iter().sum::<f64>()).max(0.) as usize;
    fractions.sort_by(|(a_index, a), (b_index, b)| b.total_cmp(a).then(a_index.cmp(b_index)));
    for &(index, _) in fractions.iter().take(missing) {
        values[index] += 1.;
    }
}

/// The periods the token activity chart can show. Each rolls, ending now,
/// so the chart never has periods still to come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ActivityRange {
    Lifetime,
    Year,
    Month,
    Day,
    #[default]
    Hour,
}

impl ActivityRange {
    pub(crate) const ALL: [Self; 5] = [
        Self::Lifetime,
        Self::Year,
        Self::Month,
        Self::Day,
        Self::Hour,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Lifetime => "All time",
            Self::Year => "Year",
            Self::Month => "Month",
            Self::Day => "Day",
            Self::Hour => "Hour",
        }
    }
}

/// One point of the token activity chart: the tokens of the turns whose
/// responses started in one period.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActivityBucket {
    /// Names the period when hovered, uniquely within the chart.
    pub(crate) label: SharedString,
    pub(crate) tokens: f64,
}

/// A label under the token activity chart, centered on bucket `index`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisLabel {
    pub(crate) index: usize,
    pub(crate) text: SharedString,
}

/// What the token activity chart shows for one [`ActivityRange`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActivityChart {
    /// Oldest first.
    pub(crate) buckets: Vec<ActivityBucket>,
    pub(crate) axis: Vec<AxisLabel>,
}

impl ActivityChart {
    /// The bucket with the most tokens, the latest of equals, unless no
    /// bucket has any.
    pub(crate) fn peak(&self) -> Option<(usize, &ActivityBucket)> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, bucket)| bucket.tokens > 0.)
            .max_by(|(_, a), (_, b)| a.tokens.total_cmp(&b.tokens))
    }
}

/// Which buckets [`ActivityChart::axis`] labels, counted back from the
/// current one, which is on the right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxisLabels {
    /// Every `every` units back, how long ago, such as `5m`. The current
    /// bucket is left unlabeled; there, it is now.
    Ago { every: u32, suffix: &'static str },
    /// The current bucket and every `every` units back, the date in the
    /// `strftime` pattern `format`.
    Date { every: u32, format: &'static str },
}

/// The calendar unit one [`ActivityBucket`] spans, in local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BucketUnit {
    Minute,
    Hour,
    Day,
    Month,
}

impl BucketUnit {
    /// The start of the unit containing `time`.
    fn floor(self, time: DateTime<Local>) -> DateTime<Local> {
        let minute = time
            .with_nanosecond(0)
            .and_then(|time| time.with_second(0))
            .unwrap_or(time);
        match self {
            Self::Minute => minute,
            Self::Hour => minute.with_minute(0).unwrap_or(minute),
            Self::Day => local_midnight(time.date_naive()).unwrap_or(time),
            Self::Month => time
                .date_naive()
                .with_day(1)
                .and_then(local_midnight)
                .unwrap_or(time),
        }
    }

    /// The start of the unit `count` units before the one starting at
    /// `start`.
    pub(crate) fn back(self, start: DateTime<Local>, count: u32) -> DateTime<Local> {
        let date = start.date_naive();
        let moved = match self {
            Self::Minute => Some(start - TimeDelta::minutes(count.into())),
            Self::Hour => Some(start - TimeDelta::hours(count.into())),
            Self::Day => date
                .checked_sub_days(Days::new(count.into()))
                .and_then(local_midnight),
            Self::Month => date
                .checked_sub_months(Months::new(count))
                .and_then(local_midnight),
        };
        moved.unwrap_or(start)
    }
}

fn local_midnight(date: chrono::NaiveDate) -> Option<DateTime<Local>> {
    date.and_time(NaiveTime::MIN)
        .and_local_timezone(Local)
        .earliest()
}

/// How [`token_activity_chart`] divides a range: `count` units ending with
/// the current one, each named with the `strftime` pattern `title` when
/// hovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BucketLayout {
    pub(crate) unit: BucketUnit,
    pub(crate) count: u32,
    pub(crate) title: &'static str,
    pub(crate) axis: AxisLabels,
}

impl BucketLayout {
    const HOUR: Self = Self {
        unit: BucketUnit::Minute,
        count: 60,
        title: "%H:%M",
        axis: AxisLabels::Ago {
            every: 5,
            suffix: "m",
        },
    };
    const DAY: Self = Self {
        unit: BucketUnit::Hour,
        count: 24,
        title: "%H:%M",
        axis: AxisLabels::Ago {
            every: 3,
            suffix: "h",
        },
    };
    const MONTH: Self = Self {
        unit: BucketUnit::Day,
        count: 30,
        title: "%b %-d",
        axis: AxisLabels::Date {
            every: 5,
            format: "%b %-d",
        },
    };
    const YEAR: Self = Self {
        unit: BucketUnit::Month,
        count: 12,
        title: "%b %Y",
        axis: AxisLabels::Date {
            every: 1,
            format: "%b",
        },
    };

    /// The layout for `range`. A lifetime takes the shortest fixed layout
    /// that reaches back to the first activity, or else one month per point
    /// since then.
    pub(crate) fn of(
        range: ActivityRange,
        activity: &[TokenActivity],
        now: DateTime<Local>,
    ) -> Self {
        match range {
            ActivityRange::Hour => Self::HOUR,
            ActivityRange::Day => Self::DAY,
            ActivityRange::Month => Self::MONTH,
            ActivityRange::Year => Self::YEAR,
            ActivityRange::Lifetime => {
                let Some(first) = activity
                    .iter()
                    .map(|activity| DateTime::<Local>::from(activity.at))
                    .min()
                else {
                    return Self::HOUR;
                };
                [Self::HOUR, Self::DAY, Self::MONTH, Self::YEAR]
                    .into_iter()
                    .find(|layout| layout.start(now) <= first)
                    .unwrap_or_else(|| {
                        let months = (now.year() - first.year()) * 12 + now.month() as i32
                            - first.month() as i32;
                        let count = u32::try_from(months + 1).unwrap_or(1);
                        Self {
                            unit: BucketUnit::Month,
                            count,
                            title: "%b %Y",
                            // About six labels.
                            axis: AxisLabels::Date {
                                every: count.div_ceil(6),
                                format: "%b %Y",
                            },
                        }
                    })
            }
        }
    }

    /// The start of the first bucket.
    pub(crate) fn start(self, now: DateTime<Local>) -> DateTime<Local> {
        self.unit
            .back(self.unit.floor(now), self.count.saturating_sub(1))
    }
}

/// The tokens used in each period of `range`, and how to label them.
pub(crate) fn token_activity_chart(
    activity: &[TokenActivity],
    range: ActivityRange,
    now: DateTime<Local>,
) -> ActivityChart {
    let layout = BucketLayout::of(range, activity, now);
    let current = layout.unit.floor(now);
    let starts: Vec<_> = (0..layout.count)
        .rev()
        .map(|back| layout.unit.back(current, back))
        .collect();
    let mut buckets: Vec<_> = starts
        .iter()
        .map(|start| ActivityBucket {
            label: start.format(layout.title).to_string().into(),
            tokens: 0.,
        })
        .collect();
    let (every, first) = match layout.axis {
        AxisLabels::Ago { every, .. } => (every, every),
        AxisLabels::Date { every, .. } => (every, 0),
    };
    let axis = (first..layout.count)
        .step_by(every.max(1) as usize)
        .map(|back| {
            let index = (layout.count - 1 - back) as usize;
            let text = match layout.axis {
                AxisLabels::Ago { suffix, .. } => format!("{back}{suffix}"),
                AxisLabels::Date { format, .. } => starts[index].format(format).to_string(),
            };
            AxisLabel {
                index,
                text: text.into(),
            }
        })
        .rev()
        .collect();
    let mut tokens = vec![0.; buckets.len()];
    for activity in activity {
        for (index, share) in activity.spread(&starts) {
            tokens[index] += share;
        }
    }
    // Whole tokens read better when hovered.
    round_preserving_total(&mut tokens);
    for (bucket, tokens) in buckets.iter_mut().zip(tokens) {
        bucket.tokens = tokens;
    }
    ActivityChart { buckets, axis }
}

/// A statistic shortened to one decimal of its largest unit, such as `950`,
/// `12.3K`, `100.8M`, or `2B`.
pub(crate) fn format_stat_count(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    let units = [(1e3, "K"), (1e6, "M"), (1e9, "B"), (1e12, "T")];
    let (value, suffix) = units
        .iter()
        .map(|&(unit, suffix)| ((count as f64 / unit * 10.).round() / 10., suffix))
        // Rounding can carry into the next unit, as 999,960 does into 1M.
        .find(|&(value, _)| value < 1_000.)
        .unwrap_or_else(|| {
            let (unit, suffix) = units[units.len() - 1];
            ((count as f64 / unit * 10.).round() / 10., suffix)
        });
    let text = format!("{value:.1}");
    format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
}

/// A token count shortened to at most a few digits, such as `950`, `4.1k`,
/// `128k`, or `1M`.
pub(crate) fn format_token_count(tokens: u64) -> String {
    fn scaled(tokens: u64, unit: u64, suffix: &str) -> String {
        let value = tokens as f64 / unit as f64;
        if value < 9.95 {
            let text = format!("{value:.1}");
            format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
        } else {
            format!("{value:.0}{suffix}")
        }
    }

    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 999_500 {
        scaled(tokens, 1_000, "k")
    } else {
        scaled(tokens, 1_000_000, "M")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_are_shortened() {
        for (tokens, text) in [
            (0, "0"),
            (950, "950"),
            (1_000, "1k"),
            (4_096, "4.1k"),
            (9_949, "9.9k"),
            (9_950, "10k"),
            (128_000, "128k"),
            (131_072, "131k"),
            (999_499, "999k"),
            (999_500, "1M"),
            (1_000_000, "1M"),
            (1_500_000, "1.5M"),
            (20_000_000, "20M"),
        ] {
            assert_eq!(format_token_count(tokens), text, "{tokens} tokens");
        }
    }

    #[test]
    fn stat_counts_are_shortened() {
        for (count, text) in [
            (0, "0"),
            (950, "950"),
            (1_000, "1K"),
            (12_345, "12.3K"),
            (999_949, "999.9K"),
            (999_960, "1M"),
            (100_800_000, "100.8M"),
            (2_100_000_000, "2.1B"),
            (5_000_000_000_000_000, "5000T"),
        ] {
            assert_eq!(format_stat_count(count), text, "{count}");
        }
    }

    #[test]
    fn token_activity_is_bucketed_by_local_time() {
        use chrono::TimeZone as _;

        let now = Local
            .with_ymd_and_hms(2026, 5, 20, 14, 37, 12)
            .single()
            .expect("unambiguous local time");
        // Instants, which fall wholly in the period they start in.
        let activity = |ago: TimeDelta, tokens| TokenActivity {
            at: SystemTime::from(now - ago),
            duration: Duration::ZERO,
            tokens,
        };
        let recent = [
            activity(TimeDelta::minutes(5), 10),
            activity(TimeDelta::minutes(30), 20),
            activity(TimeDelta::hours(2), 40),
        ];
        let chart = |activity: &[TokenActivity], range| token_activity_chart(activity, range, now);
        let titles = |chart: &ActivityChart| -> Vec<String> {
            chart
                .buckets
                .iter()
                .map(|bucket| bucket.label.to_string())
                .collect()
        };
        let tokens = |chart: &ActivityChart| -> Vec<f64> {
            chart.buckets.iter().map(|bucket| bucket.tokens).collect()
        };
        let axis = |chart: &ActivityChart| -> Vec<(usize, String)> {
            chart
                .axis
                .iter()
                .map(|label| (label.index, label.text.to_string()))
                .collect()
        };

        // A minute each, ending with the current one; older turns are left
        // out. The axis counts back from now, on the right, every 5 minutes.
        let hour = chart(&recent, ActivityRange::Hour);
        let (titles_, tokens_) = (titles(&hour), tokens(&hour));
        assert_eq!(
            (titles_.len(), &*titles_[0], &*titles_[59]),
            (60, "13:38", "14:37")
        );
        assert_eq!(
            (tokens_[29], tokens_[54], tokens_.iter().sum()),
            (20., 10., 30.)
        );
        let labels = axis(&hour);
        assert_eq!(labels.len(), 11);
        assert_eq!(labels[0], (4, "55m".to_owned()));
        assert_eq!(labels[10], (54, "5m".to_owned()));
        assert_eq!(
            hour.peak().map(|(index, bucket)| (index, &*bucket.label)),
            Some((29, "14:07"))
        );

        let day = chart(&recent, ActivityRange::Day);
        let (titles_, tokens_) = (titles(&day), tokens(&day));
        assert_eq!(
            (titles_.len(), &*titles_[0], &*titles_[23]),
            (24, "15:00", "14:00")
        );
        assert_eq!((tokens_[21], tokens_[23]), (40., 30.));
        let labels = axis(&day);
        assert_eq!(
            (labels.len(), &labels[0], &labels[6]),
            (7, &(2, "21h".to_owned()), &(20, "3h".to_owned()))
        );

        let month = chart(&recent, ActivityRange::Month);
        let titles_ = titles(&month);
        assert_eq!(
            (titles_.len(), &*titles_[0], &*titles_[29]),
            (30, "Apr 21", "May 20")
        );
        assert_eq!(
            axis(&month)
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            ["Apr 25", "Apr 30", "May 5", "May 10", "May 15", "May 20"]
        );

        let year = chart(&recent, ActivityRange::Year);
        let titles_ = titles(&year);
        assert_eq!(
            (titles_.len(), &*titles_[0], &*titles_[11]),
            (12, "Jun 2025", "May 2026")
        );
        let labels = axis(&year);
        assert_eq!(
            (labels.len(), &labels[0], &labels[11]),
            (12, &(0, "Jun".to_owned()), &(11, "May".to_owned()))
        );

        // All time takes the shortest layout that reaches the first turn.
        assert_eq!(chart(&recent, ActivityRange::Lifetime), day);
        let empty = chart(&[], ActivityRange::Lifetime);
        assert_eq!((empty.buckets.len(), empty.peak()), (60, None));
        let old = [activity(TimeDelta::days(400), 5), recent[0]];
        let lifetime = chart(&old, ActivityRange::Lifetime);
        let (titles_, tokens_) = (titles(&lifetime), tokens(&lifetime));
        assert_eq!(
            (titles_.len(), &*titles_[0], &*titles_[13]),
            (14, "Apr 2025", "May 2026")
        );
        assert_eq!((tokens_[0], tokens_[13]), (5., 10.));
        assert_eq!(
            axis(&lifetime),
            [
                (1, "May 2025".to_owned()),
                (4, "Aug 2025".to_owned()),
                (7, "Nov 2025".to_owned()),
                (10, "Feb 2026".to_owned()),
                (13, "May 2026".to_owned()),
            ]
        );

        // Of equal peaks, the latest is marked.
        let tied = [
            activity(TimeDelta::minutes(20), 7),
            activity(TimeDelta::minutes(10), 7),
        ];
        let tied = chart(&tied, ActivityRange::Hour);
        assert_eq!(tied.peak().map(|(index, _)| index), Some(49));
    }

    #[test]
    fn token_activity_is_spread_across_each_response() {
        use chrono::TimeZone as _;

        let now = Local
            .with_ymd_and_hms(2026, 5, 20, 14, 37, 12)
            .single()
            .expect("unambiguous local time");
        let response = |ago: TimeDelta, duration: TimeDelta, tokens| TokenActivity {
            at: SystemTime::from(now - ago),
            duration: duration.to_std().expect("positive duration"),
            tokens,
        };
        let hour = |activity: &[TokenActivity]| -> Vec<f64> {
            token_activity_chart(activity, ActivityRange::Hour, now)
                .buckets
                .iter()
                .map(|bucket| bucket.tokens)
                .collect()
        };
        let minutes = TimeDelta::minutes;
        let seconds = TimeDelta::seconds;

        // 14:30:30 to 14:33:30: half a minute, two whole ones, and a half.
        let tokens = hour(&[response(minutes(6) + seconds(42), minutes(3), 600)]);
        assert_eq!(tokens[52..56], [100., 200., 200., 100.]);
        assert_eq!(tokens.iter().sum::<f64>(), 600.);

        // 13:36:12 to 13:40:12, of which only what falls in the hour counts.
        let tokens = hour(&[response(minutes(61), minutes(4), 240)]);
        assert_eq!(tokens[0..3], [60., 60., 12.]);
        assert_eq!(tokens.iter().sum::<f64>(), 132.);

        // A response still running past now stays in the current minute.
        let tokens = hour(&[response(seconds(12), minutes(2), 50)]);
        assert_eq!(tokens[59], 50.);

        // Shares are rounded to whole tokens without losing any: two
        // tokens over three minutes go to the first two.
        let tokens = hour(&[response(minutes(37) + seconds(12), minutes(3), 2)]);
        assert_eq!(tokens[22..25], [1., 1., 0.]);
        assert_eq!(tokens.iter().sum::<f64>(), 2.);
    }

    #[test]
    fn stat_durations_show_their_two_largest_units() {
        for (seconds, text) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m 0s"),
            (35 * 60 + 16, "35m 16s"),
            (3_600, "1h 0m"),
            (2 * 3_600 + 5 * 60 + 59, "2h 5m"),
        ] {
            assert_eq!(
                format_stat_duration(Duration::from_secs(seconds)),
                text,
                "{seconds}s"
            );
        }
        assert_eq!(format_stat_duration(Duration::from_millis(1_999)), "1s");
    }

    #[test]
    fn context_usage_percent_and_color() {
        let usage = |tokens| ContextUsage {
            tokens,
            max_tokens: 1_000,
        };
        let theme = Theme::default();
        assert_eq!(usage(130).percent(), 13.);
        assert_eq!(usage(130).color(&theme), theme.muted_foreground);
        assert_eq!(usage(799).color(&theme), theme.muted_foreground);
        assert_eq!(usage(800).color(&theme), theme.warning);
        assert_eq!(usage(949).color(&theme), theme.warning);
        assert_eq!(usage(950).color(&theme), theme.danger);
        assert_eq!(usage(1_200).color(&theme), theme.danger);
        let unknown = ContextUsage {
            tokens: 5,
            max_tokens: 0,
        };
        assert_eq!(unknown.percent(), 0.);
        assert_eq!(unknown.color(&theme), theme.muted_foreground);
    }

    #[test]
    fn usage_tokens_falls_back_to_input_and_output() {
        let usage = |input, output, total| Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: total,
            ..Default::default()
        };
        assert_eq!(usage_tokens(usage(Some(10), Some(5), Some(20))), 20);
        assert_eq!(usage_tokens(usage(Some(10), Some(5), None)), 15);
        assert_eq!(usage_tokens(usage(Some(10), None, None)), 10);
        assert_eq!(usage_tokens(Usage::default()), 0);
    }
}
