//! Lifetime usage: every token, every working minute and every run across all chats, kept in
//! `~/.rhea/usage.json` so deleting or archiving a chat never takes its usage with it.
//!
//! The file holds small daily totals (tokens, time worked, runs) and running counts, not any chat
//! content. Settings > Usage is drawn from it: the profile numbers, the 3-month activity grid, where
//! the tokens went (system prompt, tools, memory, messages, replies) and a few habits (fast mode,
//! reasoning level, skills).

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard, OnceLock},
};

use serde::{Deserialize, Serialize};

const FILE: &str = "usage.json";

/// Where the tokens of the requests went. System prompt, tools and memory are estimates of what was
/// sent with every request; messages is the rest of the input; output is what the model wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parts {
    pub system: u64,
    pub tools: u64,
    pub memory: u64,
    pub messages: u64,
    pub output: u64,
}

impl Parts {
    /// Split one request: `input` and `output` as the service reported them, and the sizes of the
    /// fixed pieces that were sent in it.
    pub fn of_request(input: u64, output: u64, system: u64, tools: u64, memory: u64) -> Self {
        let system = system.min(input);
        let tools = tools.min(input - system);
        let memory = memory.min(input - system - tools);
        Self { system, tools, memory, messages: input - system - tools - memory, output }
    }

    pub fn total(&self) -> u64 {
        self.system + self.tools + self.memory + self.messages + self.output
    }

    fn add(&mut self, other: Parts) {
        self.system += other.system;
        self.tools += other.tools;
        self.memory += other.memory;
        self.messages += other.messages;
        self.output += other.output;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    pub tokens: u64,
    pub work_ms: u64,
    pub runs: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens used before this file existed, read once from the chats that were already there.
    #[serde(default)]
    pub before_tracking: u64,
    #[serde(default)]
    pub counted_before: bool,
    /// Per local day, keyed `YYYY-MM-DD`.
    #[serde(default)]
    pub days: BTreeMap<String, Day>,
    #[serde(default)]
    pub parts: Parts,
    #[serde(default)]
    pub longest_task_ms: u64,
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub fast_runs: u64,
    #[serde(default)]
    pub efforts: BTreeMap<String, u64>,
    #[serde(default)]
    pub skills: BTreeMap<String, u64>,
}

impl Usage {
    pub fn record_request(&mut self, day: &str, input: u64, output: u64, parts: Parts) {
        self.days.entry(day.to_owned()).or_default().tokens += input + output;
        self.parts.add(parts);
    }

    /// A finished run (answered, failed or stopped): the time it took and how it was set.
    pub fn record_run(&mut self, day: &str, elapsed_ms: u64, effort: &str, fast: bool) {
        let entry = self.days.entry(day.to_owned()).or_default();
        entry.work_ms += elapsed_ms;
        entry.runs += 1;
        self.longest_task_ms = self.longest_task_ms.max(elapsed_ms);
        self.runs += 1;
        self.fast_runs += u64::from(fast);
        if !effort.is_empty() {
            *self.efforts.entry(effort.to_owned()).or_default() += 1;
        }
    }

    pub fn record_skill(&mut self, name: &str) {
        *self.skills.entry(name.to_owned()).or_default() += 1;
    }

    pub fn tracked_tokens(&self) -> u64 {
        self.days.values().map(|day| day.tokens).sum()
    }

    pub fn lifetime_tokens(&self) -> u64 {
        self.before_tracking + self.tracked_tokens()
    }

    /// The day with the most tokens.
    pub fn peak_day(&self) -> Option<(&str, u64)> {
        self.days.iter().filter(|(_, day)| day.tokens > 0).max_by_key(|(_, day)| day.tokens).map(|(key, day)| (key.as_str(), day.tokens))
    }

    pub fn worked_total_ms(&self) -> u64 {
        self.days.values().map(|day| day.work_ms).sum()
    }

    pub fn most_worked_day_ms(&self) -> u64 {
        self.days.values().map(|day| day.work_ms).max().unwrap_or(0)
    }

    /// `(longest streak, current streak)` in days with any usage. The current streak is still alive
    /// when the last active day is today or yesterday.
    pub fn streaks(&self, today: &str) -> (u32, u32) {
        let mut active: Vec<i64> = self.days.iter().filter(|(_, day)| day.tokens > 0).filter_map(|(key, _)| day_number(key)).collect();
        active.sort_unstable();
        active.dedup();
        let (mut longest, mut run) = (0_u32, 0_u32);
        let mut previous: Option<i64> = None;
        for day in &active {
            run = if previous == Some(day - 1) { run + 1 } else { 1 };
            longest = longest.max(run);
            previous = Some(*day);
        }
        let today = day_number(today).unwrap_or(0);
        let current = match previous {
            Some(last) if last == today || last == today - 1 => run,
            _ => 0,
        };
        (longest, current)
    }

    /// Share of runs that used fast mode, in whole percent.
    pub fn fast_percent(&self) -> Option<u32> {
        (self.runs > 0).then(|| ((self.fast_runs * 100 + self.runs / 2) / self.runs) as u32)
    }

    /// The reasoning level used for most runs, and its share in whole percent.
    pub fn top_effort(&self) -> Option<(String, u32)> {
        let total: u64 = self.efforts.values().sum();
        let (name, count) = self.efforts.iter().max_by_key(|(_, count)| **count)?;
        Some((name.clone(), ((count * 100 + total / 2) / total) as u32))
    }

    pub fn skills_explored(&self) -> usize {
        self.skills.len()
    }

    pub fn skills_used(&self) -> u64 {
        self.skills.values().sum()
    }

    /// The last `days` days ending today, as month blocks for the activity grid.
    pub fn heatmap(&self, today: &str, days: usize) -> Vec<MonthBlock> {
        let Some(end) = day_number(today) else { return Vec::new() };
        let start = end - days as i64 + 1;
        let mut blocks: Vec<MonthBlock> = Vec::new();
        let mut month_start = start;
        for number in start..=end {
            let (year, month, day) = civil_from_days(number);
            if blocks.last().is_none_or(|block| block.month != (year, month)) {
                blocks.push(MonthBlock { month: (year, month), label: MONTHS[month as usize - 1], columns: Vec::new() });
                month_start = number;
            }
            let first_weekday = weekday(month_start) as i64;
            let offset = first_weekday + (number - month_start);
            let (column, row) = ((offset / 7) as usize, (offset % 7) as usize);
            let block = blocks.last_mut().expect("a block was just pushed");
            while block.columns.len() <= column {
                block.columns.push(Default::default());
            }
            let key = key_of(year, month, day);
            let entry = self.days.get(&key);
            block.columns[column][row] = Some(Cell { tokens: entry.map_or(0, |d| d.tokens), work_ms: entry.map_or(0, |d| d.work_ms), key });
        }
        blocks
    }
}

pub const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub key: String,
    pub tokens: u64,
    pub work_ms: u64,
}

/// One calendar month of the grid: week columns (Sunday to Saturday as rows 0 to 6), empty where a
/// day belongs to another month or to the future.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonthBlock {
    month: (i64, u32),
    pub label: &'static str,
    pub columns: Vec<[Option<Cell>; 7]>,
}

// ---- dates (proleptic Gregorian, days since 1970-01-01) ---------------------------------------

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = (if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// 0 for Sunday through 6 for Saturday.
pub fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

pub fn key_of(year: i64, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn day_number(key: &str) -> Option<i64> {
    let mut parts = key.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day) && parts.next().is_none()).then(|| days_from_civil(year, month, day))
}

fn utc_today() -> String {
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    key_of(year, month, day)
}

/// Today's date on this computer's clock (the local day, not UTC).
pub fn today() -> String {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
        // SAFETY: GetLocalTime only writes into the SYSTEMTIME it is given.
        let time = unsafe {
            let mut time: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut time);
            time
        };
        key_of(i64::from(time.wYear), u32::from(time.wMonth), u32::from(time.wDay))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // `date` knows the local time zone; asked at most once a minute.
        static CACHE: Mutex<Option<(std::time::Instant, String)>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, day)) = cache.as_ref() {
            if at.elapsed().as_secs() < 60 {
                return day.clone();
            }
        }
        let day = std::process::Command::new("date")
            .arg("+%Y-%m-%d")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|text| text.trim().to_owned())
            .filter(|text| day_number(text).is_some())
            .unwrap_or_else(utc_today);
        *cache = Some((std::time::Instant::now(), day.clone()));
        day
    }
}

// ---- the process-wide record --------------------------------------------------------------------

static STORE: OnceLock<Mutex<Usage>> = OnceLock::new();

fn path() -> Option<std::path::PathBuf> {
    crate::chat_store::app_data_root().map(|root| root.join(FILE))
}

fn store() -> MutexGuard<'static, Usage> {
    STORE
        .get_or_init(|| Mutex::new(path().and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn save(usage: &Usage) {
    let Some(path) = path() else { return };
    let Ok(bytes) = serde_json::to_vec(usage) else { return };
    if let Err(error) = crate::chat_store::write_bytes_atomic(&path, &bytes) {
        crate::chat_store::report_save_error("your usage totals", &error);
    }
}

/// Run `change` on the record and save it. Tests never touch the real file.
fn update(change: impl FnOnce(&mut Usage, &str)) {
    if cfg!(test) {
        return;
    }
    let mut usage = store();
    change(&mut usage, &today());
    save(&usage);
}

/// One model request finished: its service-reported size and where it went.
pub fn record_request(input: u64, output: u64, parts: Parts) {
    update(|usage, day| usage.record_request(day, input, output, parts));
}

/// A run ended (answered, failed or stopped by the user).
pub fn record_run(elapsed_ms: u64, effort: &str, fast: bool) {
    update(|usage, day| usage.record_run(day, elapsed_ms, effort, fast));
}

pub fn record_skill(name: &str) {
    update(|usage, _| usage.record_skill(name));
}

/// Count the tokens the chats had used before this record existed. Done once.
pub fn backfill(tokens_before: u64) {
    update(|usage, _| {
        if !usage.counted_before {
            usage.before_tracking = tokens_before;
            usage.counted_before = true;
        }
    });
}

pub fn needs_backfill() -> bool {
    !cfg!(test) && !store().counted_before
}

pub fn snapshot() -> Usage {
    store().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(days: &[(&str, u64)]) -> Usage {
        let mut usage = Usage::default();
        for (day, tokens) in days {
            usage.record_request(day, *tokens, 0, Parts::default());
        }
        usage
    }

    #[test]
    fn dates_round_trip_and_weekdays_are_right() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(days_from_civil(2026, 11, 6)), (2026, 11, 6));
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(weekday(day_number("2026-11-06").unwrap()), 5, "a Friday");
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("nope"), None);
        assert_eq!(day_number("2026-11-06") .unwrap() - day_number("2026-11-05").unwrap(), 1);
    }

    #[test]
    fn today_is_a_valid_local_date() {
        assert!(day_number(&today()).is_some());
    }

    #[test]
    fn lifetime_keeps_what_came_before_and_the_peak_day_is_found() {
        let mut usage = used(&[("2026-11-01", 1_000), ("2026-11-02", 5_000), ("2026-11-03", 2_000)]);
        usage.before_tracking = 10_000;
        assert_eq!(usage.lifetime_tokens(), 18_000);
        assert_eq!(usage.tracked_tokens(), 8_000);
        assert_eq!(usage.peak_day(), Some(("2026-11-02", 5_000)));
        assert_eq!(Usage::default().peak_day(), None);
    }

    #[test]
    fn streaks_count_consecutive_days_and_survive_until_today_ends() {
        let usage = used(&[("2026-10-01", 1), ("2026-10-02", 1), ("2026-10-03", 1), ("2026-10-10", 1), ("2026-11-05", 1), ("2026-11-06", 1)]);
        assert_eq!(usage.streaks("2026-11-06"), (3, 2));
        assert_eq!(usage.streaks("2026-11-07"), (3, 2), "yesterday was active, so the streak is still alive");
        assert_eq!(usage.streaks("2026-11-08"), (3, 0), "two idle days end it");
        assert_eq!(Usage::default().streaks("2026-11-06"), (0, 0));
        let across_months = used(&[("2026-10-30", 1), ("2026-10-31", 1), ("2026-11-01", 1)]);
        assert_eq!(across_months.streaks("2026-11-01"), (3, 3));
    }

    #[test]
    fn runs_track_time_fast_mode_and_reasoning() {
        let mut usage = Usage::default();
        usage.record_run("2026-11-06", 60_000, "high", false);
        usage.record_run("2026-11-06", 9_000, "xhigh", true);
        usage.record_run("2026-11-05", 30_000, "xhigh", false);
        assert_eq!(usage.longest_task_ms, 60_000);
        assert_eq!(usage.worked_total_ms(), 99_000);
        assert_eq!(usage.most_worked_day_ms(), 69_000);
        assert_eq!(usage.fast_percent(), Some(33));
        assert_eq!(usage.top_effort(), Some(("xhigh".into(), 67)));
        assert_eq!(Usage::default().fast_percent(), None);
        assert_eq!(Usage::default().top_effort(), None);
    }

    #[test]
    fn skills_are_counted_by_name() {
        let mut usage = Usage::default();
        for name in ["taste", "unslop", "taste"] {
            usage.record_skill(name);
        }
        assert_eq!((usage.skills_explored(), usage.skills_used()), (2, 3));
    }

    #[test]
    fn a_request_is_split_into_where_it_went() {
        let parts = Parts::of_request(10_000, 500, 3_000, 4_000, 800);
        assert_eq!(parts, Parts { system: 3_000, tools: 4_000, memory: 800, messages: 2_200, output: 500 });
        assert_eq!(parts.total(), 10_500);
        // Estimates larger than the request are cut to fit, never negative.
        let small = Parts::of_request(1_000, 0, 3_000, 4_000, 800);
        assert_eq!((small.system, small.tools, small.memory, small.messages), (1_000, 0, 0, 0));
        let mut usage = Usage::default();
        usage.record_request("2026-11-06", 10_000, 500, parts);
        usage.record_request("2026-11-06", 10_000, 500, parts);
        assert_eq!(usage.parts.system, 6_000);
        assert_eq!(usage.days["2026-11-06"].tokens, 21_000);
    }

    #[test]
    fn the_grid_has_one_cell_per_day_in_month_blocks() {
        let usage = used(&[("2026-11-06", 9_000), ("2026-10-15", 100)]);
        let blocks = usage.heatmap("2026-11-06", 91);
        assert_eq!(blocks.iter().map(|b| b.label).collect::<Vec<_>>(), ["Aug", "Sep", "Oct", "Nov"], "four months, so three separators");
        let cells: Vec<&Cell> = blocks.iter().flat_map(|b| b.columns.iter().flatten().flatten()).collect();
        assert_eq!(cells.len(), 91);
        assert_eq!(cells.first().unwrap().key, "2026-08-08");
        assert_eq!(cells.last().unwrap().key, "2026-11-06");
        assert_eq!(cells.last().unwrap().tokens, 9_000);
        assert_eq!(cells.iter().find(|c| c.key == "2026-10-15").unwrap().tokens, 100);
        // The 1st of Nov 2026 is a Sunday, so it starts the first row of its block.
        let nov = &blocks[3];
        assert_eq!(nov.columns[0][0].as_ref().unwrap().key, "2026-11-01");
        // A day sits in the row of its weekday (Friday = 5).
        assert_eq!(nov.columns[0][5].as_ref().unwrap().key, "2026-11-06");
        assert!(Usage::default().heatmap("garbage", 91).is_empty());
    }

    #[test]
    fn the_usage_file_round_trips_and_reads_old_files() {
        let mut usage = used(&[("2026-11-06", 5)]);
        usage.record_skill("a");
        let again: Usage = serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap();
        assert_eq!(again.lifetime_tokens(), 5);
        assert_eq!(again.skills_used(), 1);
        let empty: Usage = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lifetime_tokens(), 0);
    }
}
