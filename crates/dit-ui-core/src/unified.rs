//! One visual contract for every active DIT engine.

use super::{
    CYAN, GRAY, GREEN, RED, RESET, TerminalSize, YELLOW, bar, linux_console, sanitize_and_clip,
    unicode_terminal,
};

/// State badges: the state word on a coloured background.
const BADGE_RUN: &str = "\x1b[30;46m";
const BADGE_PASS: &str = "\x1b[30;42m";
const BADGE_FAIL: &str = "\x1b[97;41m";
const BADGE_WARN: &str = "\x1b[30;43m";
/// The title row, as a band across the screen.
const TITLE_BAND: &str = "\x1b[30;46m";
/// Bar fills. The Linux console has 16 colours: plain green (not the bright one) and its colour 3,
/// which it draws orange-brown. Other terminals get a softer green and a true orange.
const BAR_GREEN_CONSOLE: &str = "\x1b[32m";
const BAR_GREEN_256: &str = "\x1b[38;5;71m";
const ORANGE_CONSOLE: &str = "\x1b[33m";
const ORANGE_256: &str = "\x1b[38;5;208m";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    if color {
        remember(dashboard);
    }
    let width = usize::from(size.cols).clamp(40, 240);
    let height = usize::from(size.rows).saturating_sub(1).max(1);
    let layout = |spaced: bool| {
        if width >= 110 && size.rows >= 29 {
            render_wide(dashboard, width, spaced)
        } else {
            render_compact(dashboard, width, spaced)
        }
    };
    // Blank rows between groups when the screen has room for them; packed otherwise.
    let mut lines = layout(true);
    if lines.len() > height {
        lines = layout(false);
    }
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

/// Meter rows of the live panel: each meter and its detail, a blank row between meters when
/// `spaced`; every bar starts in the same column.
fn meter_rows(meters: &[DashboardMeter], width: usize, spaced: bool) -> Vec<String> {
    let label_width = meters
        .iter()
        .map(|metric| metric.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut rows = Vec::new();
    for (index, metric) in meters.iter().enumerate() {
        if spaced && index > 0 {
            rows.push(String::new());
        }
        rows.push(meter(metric, width, label_width));
        if let Some(detail) = &metric.detail {
            rows.push(format!("  {detail}"));
        }
    }
    rows
}

/// The title row: the engine's name, centred (painted as a band).
fn title_row(width: usize) -> String {
    let inner = width.saturating_sub(4);
    full_row(
        &format!("{:^inner$}", "DIT-01 / DATA INTEGRITY TEST ENGINE"),
        width,
    )
}

/// The last dashboard drawn on a terminal, kept for [`save_final_frame`].
static LAST: std::sync::Mutex<Option<UnifiedDashboard>> = std::sync::Mutex::new(None);

fn remember(dashboard: &UnifiedDashboard) {
    if let Ok(mut last) = LAST.lock() {
        *last = Some(dashboard.clone());
    }
}

/// Environment variable naming the file a test's last screen is written to when its live view
/// ends (`InPlaceRenderer::finish`). The launcher reads it back to show the verdict and the
/// report's place inside that screen.
pub const FINAL_FRAME_ENV: &str = "DIT_UI_FINAL_FRAME";

/// Writes the last dashboard drawn to the file `DIT_UI_FINAL_FRAME` names (nothing when unset or
/// nothing was drawn). Written whole, then renamed.
pub fn save_final_frame() {
    let Some(path) = std::env::var_os(FINAL_FRAME_ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    let Some(dashboard) = LAST.lock().ok().and_then(|last| last.clone()) else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let temporary = path.with_extension("tmp");
    if serde_json::to_vec(&dashboard).is_ok_and(|bytes| std::fs::write(&temporary, bytes).is_ok()) {
        let _ = std::fs::rename(&temporary, &path);
    }
}

/// A test's last screen as [`save_final_frame`] wrote it.
#[must_use]
pub fn load_final_frame(path: &std::path::Path) -> Option<UnifiedDashboard> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The last screen of a finished test with its result under the progress: `rows` (the verdict
/// as `RESULT: <VERDICT> ...`, where the report went, anything the screen did not show) after
/// the running rows, and `footer` in place of the running footer.
#[must_use]
pub fn render_result_frame(
    dashboard: &UnifiedDashboard,
    rows: &[String],
    footer: &str,
    size: TerminalSize,
    color: bool,
) -> String {
    let mut finished = dashboard.clone();
    "DONE".clone_into(&mut finished.state);
    // A pass means the planned work is complete, whatever the last running estimate showed.
    if rows
        .first()
        .is_some_and(|row| row.starts_with("RESULT: PASS"))
    {
        finished.progress_percent = Some(100.0);
    }
    if !rows.is_empty() {
        finished.running_rows.push(String::new());
        finished.running_rows.extend(wrap_result_rows(rows, size));
    }
    footer.clone_into(&mut finished.footer);
    render_unified_dashboard(&finished, size, color)
}

/// Result rows as the frame shows them: a row wider than the frame (a report path) continues on
/// the next rows, under its value column, instead of being clipped at the edge.
#[must_use]
pub fn wrap_result_rows(rows: &[String], size: TerminalSize) -> Vec<String> {
    let inner = usize::from(size.cols).clamp(40, 240).saturating_sub(4);
    let mut wrapped = Vec::new();
    for row in rows {
        let chars: Vec<char> = row.chars().collect();
        if chars.len() <= inner {
            wrapped.push(row.clone());
            continue;
        }
        let indent = value_column(&chars)
            .filter(|&column| column <= inner / 2)
            .unwrap_or(0);
        let mut rest = chars.as_slice();
        let mut first = true;
        while !rest.is_empty() {
            let room = if first { inner } else { inner - indent };
            let cut = if rest.len() <= room {
                rest.len()
            } else {
                // After the last `/` or space that fits, unless that leaves the row half empty.
                rest[..room]
                    .iter()
                    .rposition(|c| *c == '/' || *c == ' ')
                    .map(|at| at + 1)
                    .filter(|&at| at > room / 2)
                    .unwrap_or(room)
            };
            let piece: String = rest[..cut].iter().collect();
            wrapped.push(if first {
                piece
            } else {
                format!("{}{}", " ".repeat(indent), piece.trim_start())
            });
            rest = &rest[cut..];
            first = false;
        }
    }
    wrapped
}

/// Where a row's value starts: after its label and the gap of two or more spaces behind it.
fn value_column(chars: &[char]) -> Option<usize> {
    let gap = chars.windows(2).position(|pair| pair == [' ', ' '])?;
    (gap..chars.len()).find(|&column| chars[column] != ' ')
}

fn render_wide(dashboard: &UnifiedDashboard, width: usize, spaced: bool) -> Vec<String> {
    let gap = 2;
    let left_width = (width - gap) / 2;
    let right_width = width - gap - left_width;
    let right_inner = right_width.saturating_sub(4);
    let live_rows = meter_rows(&dashboard.live_meters, right_inner, spaced);
    let panel_rows = dashboard.hardware_rows.len().max(live_rows.len());
    let mut lines = vec![
        border("", width),
        title_row(width),
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
    lines.extend(render_running(dashboard, width, spaced));
    lines
}

fn render_compact(dashboard: &UnifiedDashboard, width: usize, spaced: bool) -> Vec<String> {
    let mut lines = vec![
        border("", width),
        title_row(width),
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
    for row in meter_rows(&dashboard.live_meters, inner, spaced) {
        lines.push(full_row(&row, width));
    }
    lines.extend(render_running(dashboard, width, spaced));
    lines
}

fn render_running(dashboard: &UnifiedDashboard, width: usize, spaced: bool) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let blank = || full_row("", width);
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
    ];
    if spaced {
        lines.push(blank());
    }
    lines.push(full_row(
        &meter(
            &DashboardMeter::new("PROGRESS", dashboard.progress_percent),
            inner,
            0,
        ),
        width,
    ));
    if spaced {
        lines.push(blank());
    }
    lines.extend([
        full_row(&format!("ACTIVE TEST   {}", dashboard.active_test), width),
        full_row(&format!("PROCESS       {}", dashboard.process), width),
        full_row(
            &format!(
                "CALCULATIONS COMPLETED  {}    |    FAIL TOTAL  {}",
                dashboard.calculations_completed, dashboard.failures
            ),
            width,
        ),
    ]);
    if spaced && !dashboard.running_rows.is_empty() {
        lines.push(blank());
    }
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

/// `LABEL [bar] suffix`, the label padded to `label_width` so bars of a panel line up.
fn meter(metric: &DashboardMeter, width: usize, label_width: usize) -> String {
    let percent = metric.percent.map(|value| value.clamp(0.0, 100.0));
    let percent_text = percent.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.0}%"));
    let suffix = if metric.suffix.is_empty() {
        format!(" {percent_text}")
    } else {
        format!(" {}", metric.suffix)
    };
    let label_width = label_width.max(metric.label.chars().count());
    let bar_width = width
        .saturating_sub(label_width + suffix.chars().count() + 3)
        .max(4);
    format!(
        "{:<label_width$} [{}]{}",
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
    for key in ["STATE: ", "RESULT: "] {
        if let Some(at) = find_chars(&chars, key) {
            let from = at + key.len();
            let to = (from..end).find(|&i| chars[i] == ' ').unwrap_or(end);
            let word: String = chars[from..to].iter().collect();
            color_range(from, to, state_badge(&word));
        }
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
    } else if upper.starts_with("FAIL") || upper.starts_with("ERROR") {
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
                // Upper half blocks: a solid bar half a row high, with the rest of its row as
                // space before the line below; the track is the same glyph in grey.
                '#' | '-' if in_bar => '\u{2580}',
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

/// A device's own load (CPU, RAM, GPU, VRAM): its bar turns orange from 70 % and red from 90 %.
/// Progress, targets and transfer rates stay green: being high is not a warning there.
fn usage_meter(label: &str) -> bool {
    label.contains("USAGE") || label.ends_with("VRAM") || label == "MEMORY"
}

/// The bars of a row with the colour of their fill: `(first, end, colour)` over the bar's interior.
fn bar_fills(chars: &[char], linux: bool) -> Vec<(usize, usize, &'static str)> {
    let green = if linux {
        BAR_GREEN_CONSOLE
    } else {
        BAR_GREEN_256
    };
    let orange = if linux { ORANGE_CONSOLE } else { ORANGE_256 };
    bar_ranges(chars)
        .into_iter()
        .map(|(start, end)| {
            let open = start.saturating_sub(1);
            let from = chars[..open]
                .iter()
                .rposition(|c| *c == '|')
                .map_or(0, |i| i + 1);
            let label: String = chars[from..open].iter().collect();
            // The percentage printed after the bar, else the bar's own fill.
            let after: String = chars[(end + 1).min(chars.len())..].iter().collect();
            let after = after.trim_start();
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            #[allow(clippy::cast_precision_loss)]
            let percent = if !digits.is_empty() && after[digits.len()..].starts_with('%') {
                digits.parse::<f64>().unwrap_or(0.0)
            } else {
                let filled = chars[start..end].iter().filter(|c| **c == '#').count();
                100.0 * filled as f64 / (end - start).max(1) as f64
            };
            let fill = if !usage_meter(label.trim()) {
                green
            } else if percent >= 90.0 {
                RED
            } else if percent >= 70.0 {
                orange
            } else {
                green
            };
            (start, end, fill)
        })
        .collect()
}

/// Whether a row is a screen's title (its first rows, with the engine's name or a `//` heading).
fn title_line(index: usize, line: &str) -> bool {
    index <= 2 && (line.contains("DATA INTEGRITY TEST ENGINE") || line.contains(" // "))
}

fn paint(lines: &[String], failures: u64, footer: &str, unicode: bool) -> String {
    let linux = linux_console();
    let footer_key: String = footer.chars().take(24).collect();
    let shown: Vec<Vec<char>> = if unicode {
        boxify(lines)
    } else {
        lines.iter().map(|line| line.chars().collect()).collect()
    };
    lines
        .iter()
        .zip(shown)
        .enumerate()
        .map(|(row, (line, shown))| {
            let (counter_above_zero, bare_fail) = fail_counters(line);
            // Lines with several results are coloured part by part (`part_colors`), never whole.
            let by_part = line.contains("FAIL BY PART")
                || line.contains("PHASES")
                || line.contains("HW EVENTS")
                || line.contains("RESULT: ");
            let is_footer = !footer_key.is_empty() && line.contains(&footer_key);
            let overrides = part_colors(line);
            // A line is red for its own count, not for the run's: one layer's FAIL does not
            // turn the others red. A bare verdict word still follows the run's total.
            let text_color =
                if !by_part && (counter_above_zero || (bare_fail && failures > 0 && !is_footer)) {
                    RED
                } else if line.contains("RESULT: ") {
                    // The verdict is its badge; the rest of the row stays plain.
                    "\x1b[37m"
                } else if line.contains("REPORT ABI") {
                    // The engine line names the temperature limit; it is a heading, not a warning.
                    CYAN
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
            let mut previous_color = "";
            let chars: Vec<char> = line.chars().collect();
            let fills = bar_fills(&chars, linux);
            let title = title_line(row, line);
            let first_frame = chars.iter().position(|c| *c == '|');
            let last_frame = chars.iter().rposition(|c| *c == '|');
            for (index, &character) in chars.iter().enumerate() {
                let in_band = title
                    && first_frame.is_some_and(|f| index > f)
                    && last_frame.is_some_and(|l| index < l);
                let current = if let Some(&(_, _, fill)) =
                    fills.iter().find(|(a, b, _)| index >= *a && index < *b)
                {
                    if character == '#' { fill } else { GRAY }
                } else if fills.iter().any(|(a, b, _)| index + 1 == *a || index == *b) {
                    // The brackets of a bar: as quiet as its empty part.
                    GRAY
                } else if in_band {
                    TITLE_BAND
                } else if let Some(color) = overrides[index] {
                    color
                } else if character == '|'
                    || (character == '+' && frame_corner(&chars, index, line))
                    || character == '['
                    || character == ']'
                {
                    CYAN
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

    /// Device load bars: green below 70 %, orange from 70 %, red from 90 %; progress and rates
    /// stay green whatever their value. The Linux console gets its own orange (colour 3).
    #[test]
    fn usage_bars_turn_orange_then_red_and_progress_stays_green() {
        let fill = |line: &str, linux: bool| {
            let chars: Vec<char> = line.chars().collect();
            bar_fills(&chars, linux)[0].2
        };
        let bar = |percent: usize| {
            format!(
                "[{}{}] {percent}%",
                "#".repeat(percent / 10),
                "-".repeat(10 - percent / 10)
            )
        };
        let line = |label: &str, percent: usize| format!("| {label} {} |", bar(percent));
        assert_eq!(fill(&line("CPU USAGE", 50), true), BAR_GREEN_CONSOLE);
        assert_eq!(fill(&line("CPU USAGE", 70), true), ORANGE_CONSOLE);
        assert_eq!(fill(&line("GPU 1 VRAM", 89), false), ORANGE_256);
        assert_eq!(fill(&line("RAM USAGE", 90), false), RED);
        assert_eq!(fill(&line("MEMORY", 95), true), RED);
        assert_eq!(fill(&line("PROGRESS", 95), true), BAR_GREEN_CONSOLE);
        assert_eq!(fill(&line("DIT TARGET", 95), false), BAR_GREEN_256);
        // No printed percentage (a rate): the bar's own fill decides, and a rate is not a load.
        let speed = "| SSD 1 SPEED [#########-] R 3 GiB/s W 0 |"
            .chars()
            .collect::<Vec<_>>();
        assert_eq!(bar_fills(&speed, true)[0].2, BAR_GREEN_CONSOLE);
    }

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
        assert!(phases.contains(&format!("{RED}[FAIL]")), "{phases:?}");
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

    /// A report path wider than the frame continues under its value column, broken after a `/`,
    /// and the whole path is still there; rows that fit are left alone.
    #[test]
    fn long_result_rows_wrap_under_their_value_instead_of_being_clipped() {
        let path =
            "/mnt/cpu-dit-results/runs/20261002T135436Z-04bb4a7a-a56b-4c93-a1f6-2ff9b8f1ea6c";
        let rows = vec![
            "RESULT: PASS    All gates passed.".to_owned(),
            format!("REPORT   Reports saved to: {path}"),
        ];
        let size = TerminalSize { rows: 40, cols: 80 };
        let wrapped = wrap_result_rows(&rows, size);
        assert_eq!(wrapped[0], rows[0]);
        assert!(wrapped.len() > rows.len());
        assert!(wrapped.iter().all(|row| row.chars().count() <= 76));
        assert!(wrapped[1].ends_with('/'));
        for continuation in &wrapped[2..] {
            assert!(continuation.starts_with(&" ".repeat(9)));
            assert!(!continuation[9..].starts_with(' '));
        }
        let joined: String = wrapped[1..].iter().map(|row| row.trim_start()).collect();
        assert!(joined.ends_with(path));
    }

    /// The result frame of a pass shows the progress complete; any other verdict keeps the
    /// progress the run reached.
    #[test]
    fn a_passed_result_frame_shows_full_progress() {
        let size = TerminalSize {
            rows: 40,
            cols: 140,
        };
        let progress = |verdict: &str| {
            let frame =
                render_result_frame(&model(), &[format!("RESULT: {verdict}")], "", size, false);
            frame
                .lines()
                .find(|line| line.contains("STATE: DONE"))
                .map(str::to_owned)
                .unwrap()
        };
        assert!(progress("PASS").contains("PROGRESS: 100%"));
        assert!(progress("FAIL").contains("PROGRESS: 25%"));
    }
}
