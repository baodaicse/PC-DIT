//! One visual contract for every active DIT engine.

use super::{
    CYAN, GRAY, GREEN, RED, RESET, TerminalSize, YELLOW, bar, sanitize_and_clip, unicode_terminal,
};

/// State badges: the state word on a coloured background.
const BADGE_RUN: &str = "\x1b[30;46m";
const BADGE_PASS: &str = "\x1b[30;42m";
const BADGE_FAIL: &str = "\x1b[97;41m";
const BADGE_WARN: &str = "\x1b[30;43m";

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardMeter {
    pub label: String,
    pub percent: Option<f64>,
    pub suffix: String,
    pub detail: Option<String>,
}

impl DashboardMeter {
    #[must_use]
    pub fn new(label: impl Into<String>, percent: Option<f64>) -> Self {
        Self {
            label: label.into(),
            percent,
            suffix: String::new(),
            detail: None,
        }
    }

    #[must_use]
    pub fn suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = suffix.into();
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnifiedDashboard {
    pub engine: String,
    pub report_abi: String,
    pub profile: String,
    pub hardware_rows: Vec<String>,
    pub live_meters: Vec<DashboardMeter>,
    pub state: String,
    pub progress_percent: Option<f64>,
    pub elapsed: String,
    pub target: String,
    pub active_test: String,
    pub process: String,
    pub calculations_completed: u64,
    pub failures: u64,
    pub running_rows: Vec<String>,
    pub footer: String,
}

/// Render the stable DIT shell. Engines may only vary the meter list and the
/// RUNNING rows; geometry, headings, colors and progress semantics stay common.
#[must_use]
pub fn render_unified_dashboard(
    dashboard: &UnifiedDashboard,
    size: TerminalSize,
    color: bool,
) -> String {
    let width = usize::from(size.cols).clamp(40, 240);
    let height = usize::from(size.rows).saturating_sub(1).max(1);
    let mut lines = if width >= 110 && size.rows >= 29 {
        render_wide(dashboard, width)
    } else {
        render_compact(dashboard, width)
    };
    if lines.len() > height {
        lines.truncate(height.saturating_sub(2));
        lines.push(full_row(&dashboard.footer, width));
        lines.push(border("", width));
    } else {
        while lines.len() < height {
            let insert_at = lines.len().saturating_sub(2);
            lines.insert(insert_at, full_row("", width));
        }
    }
    debug_assert!(lines.len() <= height);
    if color {
        paint(
            &lines,
            dashboard.failures,
            &dashboard.footer,
            unicode_terminal(),
        )
    } else {
        lines.join("\n")
    }
}

fn render_wide(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let gap = 2;
    let left_width = (width - gap) / 2;
    let right_width = width - gap - left_width;
    let right_inner = right_width.saturating_sub(4);
    let mut live_rows = Vec::new();
    for metric in &dashboard.live_meters {
        live_rows.push(meter(metric, right_inner));
        if let Some(detail) = &metric.detail {
            live_rows.push(detail.clone());
        }
    }
    let panel_rows = dashboard.hardware_rows.len().max(live_rows.len());
    let mut lines = vec![
        border("", width),
        full_row("DIT-01 / DATA INTEGRITY TEST ENGINE", width),
        full_row(
            &format!(
                "ENGINE {}    REPORT ABI {}    PROFILE {}",
                dashboard.engine, dashboard.report_abi, dashboard.profile
            ),
            width,
        ),
        pair(
            &border("SYSTEM HARDWARE", left_width),
            &border("LIVE UTILIZATION", right_width),
            gap,
        ),
    ];
    for index in 0..panel_rows {
        lines.push(pair(
            &panel_row(
                dashboard
                    .hardware_rows
                    .get(index)
                    .map_or("", String::as_str),
                left_width,
            ),
            &panel_row(live_rows.get(index).map_or("", String::as_str), right_width),
            gap,
        ));
    }
    lines.push(pair(&border("", left_width), &border("", right_width), gap));
    lines.extend(render_running(dashboard, width));
    lines
}

fn render_compact(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let mut lines = vec![
        border("", width),
        full_row("DIT-01 / DATA INTEGRITY TEST ENGINE", width),
        full_row(
            &format!(
                "ENGINE {} / ABI {} / PROFILE {}",
                dashboard.engine, dashboard.report_abi, dashboard.profile
            ),
            width,
        ),
        border("SYSTEM HARDWARE / LIVE UTILIZATION", width),
    ];
    for hardware in dashboard.hardware_rows.iter().take(3) {
        lines.push(full_row(hardware, width));
    }
    let inner = width.saturating_sub(4);
    for metric in &dashboard.live_meters {
        lines.push(full_row(&meter(metric, inner), width));
        if let Some(detail) = &metric.detail {
            lines.push(full_row(detail, width));
        }
    }
    lines.extend(render_running(dashboard, width));
    lines
}

fn render_running(dashboard: &UnifiedDashboard, width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let mut lines = vec![
        border("TESTING / PROCESS INFORMATION", width),
        full_row(
            &format!(
                "STATE: {}    PROGRESS: {}    TIME: {} / {}",
                dashboard.state,
                dashboard.progress_percent.map_or_else(
                    || "N/A".to_owned(),
                    |value| format!("{:.0}%", value.clamp(0.0, 100.0))
                ),
                dashboard.elapsed,
                dashboard.target
            ),
            width,
        ),
        full_row(
            &meter(
                &DashboardMeter::new("PROGRESS", dashboard.progress_percent),
                inner,
            ),
            width,
        ),
        full_row(&format!("ACTIVE TEST   {}", dashboard.active_test), width),
        full_row(&format!("PROCESS       {}", dashboard.process), width),
        full_row(
            &format!(
                "CALCULATIONS COMPLETED  {}    |    FAIL TOTAL  {}",
                dashboard.calculations_completed, dashboard.failures
            ),
            width,
        ),
    ];
    lines.extend(
        dashboard
            .running_rows
            .iter()
            .map(|line| full_row(line, width)),
    );
    lines.push(full_row(&dashboard.footer, width));
    lines.push(border("", width));
    lines
}

fn meter(metric: &DashboardMeter, width: usize) -> String {
    let percent = metric.percent.map(|value| value.clamp(0.0, 100.0));
    let percent_text = percent.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.0}%"));
    let suffix = if metric.suffix.is_empty() {
        format!(" {percent_text}")
    } else {
        format!(" {}", metric.suffix)
    };
    let bar_width = width
        .saturating_sub(metric.label.chars().count() + suffix.chars().count() + 3)
        .max(1);
    format!(
        "{} [{}]{}",
        metric.label,
        bar(percent.unwrap_or(0.0), 100.0, bar_width),
        suffix
    )
}

fn pair(left: &str, right: &str, gap: usize) -> String {
    format!("{left}{}{right}", " ".repeat(gap))
}

fn border(title: &str, width: usize) -> String {
    let inner = width.saturating_sub(2);
    if title.is_empty() {
        return format!("+{}+", "-".repeat(inner));
    }
    let title = sanitize_and_clip(&format!("-- {title} "), inner);
    format!("+{title}{}+", "-".repeat(inner.saturating_sub(title.len())))
}

fn panel_row(content: &str, width: usize) -> String {
    let inner = width.saturating_sub(4);
    let text = sanitize_and_clip(content, inner);
    format!("| {text:<inner$} |")
}

fn full_row(content: &str, width: usize) -> String {
    panel_row(content, width)
}

/// What the `FAIL` words of a line say: a counter above zero (`FAIL 3`, `FAIL TOTAL  3`), and
/// whether a bare verdict word (`FAIL` not followed by a count) is there.
fn fail_counters(line: &str) -> (bool, bool) {
    let (mut positive, mut bare) = (false, false);
    let mut rest = line;
    while let Some(at) = rest.find("FAIL") {
        let after = rest[at + 4..].trim_start();
        let after = after.strip_prefix("TOTAL").map_or(after, str::trim_start);
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            bare = true;
        } else if digits.parse::<u64>().map_or(true, |count| count > 0) {
            positive = true;
        }
        rest = &rest[at + 4..];
    }
    (positive, bare)
}

/// Char index of `needle` in `chars`.
fn find_chars(chars: &[char], needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    chars
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
}

/// Colours of single parts of a line, by char index, where a line carries several results:
/// the cells of `FAIL BY PART` with a count above zero (red), the events of `HW EVENTS`
/// (yellow) and the `[FAIL]` phases of `PHASES` (red). A FAIL stays red where it happened.
fn part_colors(line: &str) -> Vec<Option<&'static str>> {
    let chars: Vec<char> = line.chars().collect();
    let mut colors = vec![None; chars.len()];
    // Content ends at the panel's closing frame.
    let end = chars.iter().rposition(|c| *c == '|').unwrap_or(chars.len());
    let mut color_range = |from: usize, to: usize, color: &'static str| {
        for slot in colors.iter_mut().take(to.min(end)).skip(from) {
            *slot = Some(color);
        }
    };
    let segments = |from: usize| -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut start = from;
        let mut index = from;
        while index + 2 < end {
            if chars[index] == ' ' && chars[index + 1] == '|' && chars[index + 2] == ' ' {
                found.push((start, index));
                start = index + 3;
                index += 3;
            } else {
                index += 1;
            }
        }
        found.push((start, end));
        found
    };
    if let Some(at) = find_chars(&chars, "STATE: ") {
        let from = at + "STATE: ".len();
        let to = (from..end).find(|&i| chars[i] == ' ').unwrap_or(end);
        let word: String = chars[from..to].iter().collect();
        color_range(from, to, state_badge(&word));
    }
    if let Some(at) = find_chars(&chars, "FAIL BY PART") {
        for (from, to) in segments(at + "FAIL BY PART".len()) {
            let text: String = chars[from..to].iter().collect();
            let count = text
                .split_whitespace()
                .find(|token| token.chars().all(|c| c.is_ascii_digit()))
                .and_then(|token| token.parse::<u64>().ok())
                .unwrap_or(0);
            if count > 0 {
                color_range(from, to, RED);
            }
        }
    } else if let Some(at) = find_chars(&chars, "HW EVENTS") {
        if find_chars(&chars, "none recorded").is_none() {
            for (from, to) in segments(at + "HW EVENTS".len()) {
                color_range(from, to, YELLOW);
            }
        }
    } else if find_chars(&chars, "PHASES").is_some() {
        // Each step in its colour: done, failed, running, waiting, cut short.
        for (tag, color) in [
            ("[OK]", GREEN),
            ("[FAIL]", RED),
            ("[>>]", YELLOW),
            ("[--]", GRAY),
            ("[!!]", YELLOW),
        ] {
            let mut index = 0;
            while let Some(offset) = find_chars(&chars[index..], tag) {
                let from = index + offset;
                let to = (from + 1..end)
                    .find(|&i| chars[i] == ' ' && chars.get(i + 1) == Some(&' '))
                    .unwrap_or(end);
                color_range(from, to, color);
                index = to;
            }
        }
    }
    colors
}

/// Whether the `+` at `index` is a frame corner (next to a frame line), not a plus in text.
fn frame_corner(chars: &[char], index: usize, line: &str) -> bool {
    line.starts_with('+')
        || index
            .checked_sub(1)
            .and_then(|i| chars.get(i))
            .is_some_and(|c| matches!(c, '-' | '+'))
        || chars.get(index + 1).is_some_and(|c| matches!(c, '-' | '+'))
}

/// Badge of a state word: running states cyan, PASS green, FAIL red, anything stopped, cut
/// short or waiting yellow.
fn state_badge(word: &str) -> &'static str {
    let upper = word.to_ascii_uppercase();
    if upper.starts_with("PASS") {
        BADGE_PASS
    } else if upper.starts_with("FAIL") {
        BADGE_FAIL
    } else if [
        "INVALID",
        "ABORT",
        "STOP",
        "INTERRUPT",
        "FINALIZ",
        "DRAIN",
        "WAIT",
        "INCOMPLETE",
    ]
    .iter()
    .any(|warn| upper.starts_with(warn))
    {
        BADGE_WARN
    } else {
        BADGE_RUN
    }
}

/// Bar interiors of a row: `[` + at least four of `#` / `-` + `]`, as char index ranges.
fn bar_ranges(row: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < row.len() {
        if row[index] == '[' {
            let start = index + 1;
            let mut end = start;
            while end < row.len() && matches!(row[end], '#' | '-') {
                end += 1;
            }
            if end < row.len() && row[end] == ']' && end - start >= 4 {
                ranges.push((start, end));
                index = end;
            }
        }
        index += 1;
    }
    ranges
}

/// The ASCII frame of a screen in box-drawing characters: each `+` becomes the corner or tee its
/// neighbours call for, runs of `-` in a frame become `─`, `|` becomes `│`, bar fills `█` and
/// `░`. A dash inside a word (`CPU-RAM`) or a `+` in text (`[3/8+]`) is left as it is. Every
/// character stays one cell wide, so the layout does not move.
#[must_use]
pub fn boxify(lines: &[String]) -> Vec<Vec<char>> {
    let grid: Vec<Vec<char>> = lines.iter().map(|line| line.chars().collect()).collect();
    let cell = |row: usize, column: usize| grid.get(row).and_then(|r| r.get(column)).copied();
    let horizontal = |c: Option<char>| matches!(c, Some('-' | '+'));
    // A `+` is a frame corner only with a frame line beside it ("CPU + RAM" is text).
    let corner = |row: usize, column: usize| {
        cell(row, column) == Some('+')
            && (column
                .checked_sub(1)
                .is_some_and(|c| horizontal(cell(row, c)))
                || horizontal(cell(row, column + 1)))
    };
    let vertical =
        |row: usize, column: usize| cell(row, column) == Some('|') || corner(row, column);
    let mut shown = grid.clone();
    for (row, chars) in grid.iter().enumerate() {
        let bars = bar_ranges(chars);
        for (column, &character) in chars.iter().enumerate() {
            let in_bar = bars.iter().any(|&(a, b)| column >= a && column < b);
            let left = column.checked_sub(1).and_then(|c| cell(row, c));
            let right = cell(row, column + 1);
            shown[row][column] = match character {
                '#' if in_bar => '\u{2588}',
                '-' if in_bar => '\u{2591}',
                '|' => '\u{2502}',
                '-' if horizontal(left) || horizontal(right) => '\u{2500}',
                '+' if corner(row, column) => {
                    let up = row.checked_sub(1).is_some_and(|r| vertical(r, column));
                    let down = vertical(row + 1, column);
                    match (up, down, horizontal(left), horizontal(right)) {
                        (false, true, false, true) => '\u{250c}',
                        (false, true, true, false) => '\u{2510}',
                        (true, false, false, true) => '\u{2514}',
                        (true, false, true, false) => '\u{2518}',
                        (true, true, false, true) => '\u{251c}',
                        (true, true, true, false) => '\u{2524}',
                        (false, true, true, true) => '\u{252c}',
                        (true, false, true, true) => '\u{2534}',
                        (true, true, true, true) => '\u{253c}',
                        (false, false, true, true) => '\u{2500}',
                        (true, true, false, false) => '\u{2502}',
                        _ => '+',
                    }
                }
                other => other,
            };
        }
    }
    shown
}

/// Colours (and, on a Unicode terminal, box-drawing characters) for a frame that is not a
/// dashboard: menus, pickers.
#[must_use]
pub fn decorate(frame: &str) -> String {
    let lines: Vec<String> = frame.lines().map(str::to_owned).collect();
    paint(&lines, 0, "", unicode_terminal())
}

fn paint(lines: &[String], failures: u64, footer: &str, unicode: bool) -> String {
    let footer_key: String = footer.chars().take(24).collect();
    let shown: Vec<Vec<char>> = if unicode {
        boxify(lines)
    } else {
        lines.iter().map(|line| line.chars().collect()).collect()
    };
    lines
        .iter()
        .zip(shown)
        .map(|(line, shown)| {
            let (counter_above_zero, bare_fail) = fail_counters(line);
            // Lines with several results are coloured part by part (`part_colors`), never whole.
            let by_part = line.contains("FAIL BY PART")
                || line.contains("PHASES")
                || line.contains("HW EVENTS");
            let is_footer = !footer_key.is_empty() && line.contains(&footer_key);
            let overrides = part_colors(line);
            // A line is red for its own count, not for the run's: one layer's FAIL does not
            // turn the others red. A bare verdict word still follows the run's total.
            let text_color =
                if !by_part && (counter_above_zero || (bare_fail && failures > 0 && !is_footer)) {
                    RED
                } else if line.contains("PASS") {
                    GREEN
                } else if line.contains("INVALID")
                    || line.contains("ABORTED")
                    || line.contains("TEMP LIMIT")
                {
                    YELLOW
                } else if line.contains("ENGINE")
                    || line.contains("SYSTEM HARDWARE")
                    || line.contains("LIVE UTILIZATION")
                    || line.contains("TESTING / PROCESS")
                {
                    CYAN
                } else {
                    "\x1b[37m"
                };
            let mut result = String::new();
            let mut in_bar = false;
            let mut previous_color = "";
            let chars: Vec<char> = line.chars().collect();
            for (index, &character) in chars.iter().enumerate() {
                let current = if character == '[' {
                    in_bar = true;
                    CYAN
                } else if character == ']' {
                    in_bar = false;
                    CYAN
                } else if in_bar && character == '#' {
                    GREEN
                } else if in_bar && character == '-' {
                    GRAY
                } else if character == '|'
                    || (character == '+' && frame_corner(&chars, index, line))
                {
                    CYAN
                } else if let Some(color) = overrides[index] {
                    color
                } else if character == '-' && line.starts_with('+') {
                    // Borders only; a dash inside a word (CPU-RAM) takes the line's colour.
                    CYAN
                } else {
                    text_color
                };
                if current != previous_color {
                    // A badge's background must end with it.
                    if previous_color.contains(';') {
                        result.push_str(RESET);
                    }
                    result.push_str(current);
                    previous_color = current;
                }
                result.push(shown.get(index).copied().unwrap_or(character));
            }
            result.push_str(RESET);
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_counters_read_their_own_count() {
        assert_eq!(
            fail_counters("CPU-RAM  IDLE  FAIL 1    round 3"),
            (true, false)
        );
        assert_eq!(fail_counters("GPU-VRAM IDLE  FAIL 0"), (false, false));
        assert_eq!(
            fail_counters("CALCULATIONS 9  |  FAIL TOTAL  0"),
            (false, false)
        );
        assert_eq!(
            fail_counters("CALCULATIONS 9  |  FAIL TOTAL  2"),
            (true, false)
        );
        assert_eq!(fail_counters("STATE: FAIL"), (false, true));
    }

    /// A FAIL stays red where it happened: the layer that failed (whatever phase runs now), the
    /// cell of FAIL BY PART that counted it and the phase it happened in, and nothing else.
    #[test]
    fn red_marks_only_where_the_failure_is() {
        let mut dashboard = model();
        dashboard.failures = 1;
        dashboard.footer = "A layer FAIL is counted, the run continues".into();
        dashboard.running_rows = vec![
            "CPU-RAM  IDLE               FAIL 1".into(),
            "GPU-VRAM RUNNING            FAIL 0".into(),
            "FAIL BY PART  CPU 0 | RAM 1 (1 susp) | PCIe-GPU1 0 | ? PCIe-GPU1|GPU1|VRAM1 2".into(),
            "HW EVENTS     PCIe-GPU1 replay +4".into(),
            "PHASES  [OK] PREFILL  [FAIL] BASELINE CPU-RAM  [>>] BASELINE GPU-VRAM".into(),
        ];
        let frame = render_unified_dashboard(
            &dashboard,
            TerminalSize {
                cols: 160,
                rows: 50,
            },
            true,
        );
        let line = |needle: &str| {
            frame
                .lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle}"))
                .to_owned()
        };
        let layer = line("IDLE");
        assert!(
            layer.contains(&format!("{RED} CPU-RAM  IDLE")),
            "the failed layer stays red, dash included: {layer:?}"
        );
        assert!(!line("GPU-VRAM").contains(RED), "a clean layer is not red");
        let parts = line("BY PART");
        assert!(parts.contains(&format!("{RED}RAM 1 (1 susp)")), "{parts:?}");
        assert!(
            parts.contains(&format!("{RED}2")) || parts.contains(&format!("{RED}?")),
            "{parts:?}"
        );
        assert!(
            !parts.contains(&format!("{RED}CPU 0")) && !parts.contains(&format!("{RED}  CPU 0"))
        );
        assert!(line("HW EVENTS").contains(YELLOW));
        let phases = line("PHASES");
        assert!(phases.contains(&format!("{RED}FAIL")), "{phases:?}");
        assert!(!phases.contains(&format!("{RED}OK")));
        assert!(
            !line("is counted").contains(RED),
            "the footer is not a failure"
        );
        assert!(line("FAIL TOTAL").contains(RED));
    }

    fn model() -> UnifiedDashboard {
        UnifiedDashboard {
            engine: "TEST-1".to_owned(),
            report_abi: "9".to_owned(),
            profile: "CPU + RAM".to_owned(),
            hardware_rows: vec!["CPU: Example".to_owned(), "RAM: 32 GiB".to_owned()],
            live_meters: vec![
                DashboardMeter::new("CPU USAGE", Some(50.0))
                    .detail("Freq 4.50 GHz / Temp 70.0 C / Power 100.0 W"),
            ],
            state: "RUNNING".to_owned(),
            progress_percent: Some(25.0),
            elapsed: "00:01:00".to_owned(),
            target: "00:04:00".to_owned(),
            active_test: "TEST-A".to_owned(),
            process: "engine-specific running text".to_owned(),
            calculations_completed: 123,
            failures: 0,
            running_rows: vec!["SEED 0x1234".to_owned()],
            footer: "TEMP LIMIT OFF | Ctrl+C preserves report".to_owned(),
        }
    }

    #[test]
    fn wide_and_compact_share_the_same_fixed_contract() {
        for size in [
            TerminalSize {
                rows: 36,
                cols: 140,
            },
            TerminalSize { rows: 24, cols: 90 },
        ] {
            let frame = render_unified_dashboard(&model(), size, false);
            assert!(frame.contains("DIT-01 / DATA INTEGRITY TEST ENGINE"));
            assert!(frame.contains("SYSTEM HARDWARE"));
            assert!(frame.contains("LIVE UTILIZATION"));
            assert!(frame.contains("TESTING / PROCESS INFORMATION"));
            assert!(frame.contains("ACTIVE TEST   TEST-A"));
            assert!(frame.contains("CALCULATIONS COMPLETED  123"));
            assert!(
                frame
                    .lines()
                    .all(|line| line.chars().count() == usize::from(size.cols))
            );
            assert!(frame.lines().count() < usize::from(size.rows));
        }
    }

    #[test]
    fn engines_change_running_payload_without_changing_dashboard_shell() {
        let cpu = model();
        let mut gpu = model();
        gpu.profile = "GPU + VRAM".to_owned();
        gpu.active_test = "GPU-XFER-MATRIX".to_owned();
        gpu.process = "Vulkan matrix + CPU oracle".to_owned();
        let size = TerminalSize {
            rows: 36,
            cols: 140,
        };
        let cpu_frame = render_unified_dashboard(&cpu, size, false);
        let gpu_frame = render_unified_dashboard(&gpu, size, false);
        for heading in [
            "DIT-01 / DATA INTEGRITY TEST ENGINE",
            "SYSTEM HARDWARE",
            "LIVE UTILIZATION",
            "TESTING / PROCESS INFORMATION",
            "CALCULATIONS COMPLETED",
        ] {
            let cpu_row = cpu_frame
                .lines()
                .position(|line| line.contains(heading))
                .unwrap();
            let gpu_row = gpu_frame
                .lines()
                .position(|line| line.contains(heading))
                .unwrap();
            assert_eq!(cpu_row, gpu_row, "fixed heading moved: {heading}");
        }
        assert!(cpu_frame.contains("ACTIVE TEST   TEST-A"));
        assert!(gpu_frame.contains("ACTIVE TEST   GPU-XFER-MATRIX"));
    }
}
