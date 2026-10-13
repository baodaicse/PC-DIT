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
    // FC5: a test with no progress for a minute says so in the footer, from its watchdog.
    let alerted;
    let dashboard = match progress_alert().filter(|_| color && dashboard.state != "DONE") {
        Some(alert) => {
            let mut with_alert = dashboard.clone();
            with_alert.footer = alert;
            alerted = with_alert;
            &alerted
        }
        None => dashboard,
    };
    let width = usize::from(size.cols).clamp(40, 240);
    let height = usize::from(size.rows).saturating_sub(1).max(1);
    let layout = |spaced: bool| {
        if width >= 110 && size.rows >= 29 {
            render_wide(dashboard, width, spaced)
        } else {
            render_compact(dashboard, width, spaced)
        }
    };
    // The verdict in big letters at the bottom of the TESTING panel, to be read from across the
    // room: as big as the spare rows allow, the blank rows between groups given up for it first.
    let banner = banner_word(dashboard).map(|word| (word, failed_parts(dashboard)));
    let inner = width.saturating_sub(4);
    let chosen = banner.as_ref().and_then(|(word, parts)| {
        [(3, true), (2, true), (2, false), (1, true), (1, false)]
            .into_iter()
            .find_map(|(scale, spaced)| {
                let art = big_word(word, scale)?;
                let lines = layout(spaced);
                let block = 1 + art.len() + parts.len();
                (art[0].len() <= inner && lines.len() + block <= height).then_some((lines, art))
            })
    });
    // The big verdict's rows (art and parts, after a blank row) and colour; they sit right above
    // the footer and the closing border.
    let mut banner_rows = None;
    let mut lines = if let (Some((mut lines, art)), Some((word, parts))) = (chosen, &banner) {
        let footer_at = lines.len() - 2;
        let block: Vec<String> = std::iter::once(String::new())
            .chain(art)
            .chain(parts.iter().cloned())
            .map(|row| full_row(&centred(&row, inner), width))
            .collect();
        banner_rows = Some((block.len() - 1, verdict_color(word)));
        lines.splice(footer_at..footer_at, block);
        lines
    } else {
        // Blank rows between groups when the screen has room for them; packed otherwise.
        let lines = layout(true);
        if lines.len() > height {
            layout(false)
        } else {
            lines
        }
    };
    if lines.len() > height {
        lines.truncate(height.saturating_sub(2));
        lines.push(full_row(&dashboard.footer, width));
        lines.push(border("", width));
    } else {
        // Spare rows go above the big verdict, which stays on the panel's last rows.
        let below = banner_rows.map_or(0, |(rows, _)| rows + 1);
        while lines.len() < height {
            let insert_at = lines.len().saturating_sub(2 + below);
            lines.insert(insert_at, full_row("", width));
        }
    }
    debug_assert!(lines.len() <= height);
    let banner_rows = banner_rows.map(|(rows, color)| (lines.len() - 2 - rows, rows, color));
    if color {
        paint(
            &lines,
            dashboard.failures,
            &dashboard.footer,
            unicode_terminal(),
            banner_rows,
        )
    } else {
        lines.join("\n")
    }
}

/// Five-row glyphs of the verdict words, `#` a pixel.
fn glyph(letter: char) -> Option<[&'static str; 5]> {
    Some(match letter {
        'P' => ["####.", "#...#", "####.", "#....", "#...."],
        'A' => [".###.", "#...#", "#####", "#...#", "#...#"],
        'S' => [".####", "#....", ".###.", "....#", "####."],
        'F' => ["#####", "#....", "####.", "#....", "#...."],
        'I' => ["###", ".#.", ".#.", ".#.", "###"],
        'L' => ["#...", "#...", "#...", "#...", "####"],
        'N' => ["#...#", "##..#", "#.#.#", "#..##", "#...#"],
        'V' => ["#...#", "#...#", "#...#", ".#.#.", "..#.."],
        'D' => ["####.", "#...#", "#...#", "#...#", "####."],
        _ => return None,
    })
}

/// `word` in big letters: each pixel `scale` rows high and twice as many columns wide (a console
/// cell is about twice as high as it is wide), one pixel between letters.
fn big_word(word: &str, scale: usize) -> Option<Vec<String>> {
    let glyphs = word.chars().map(glyph).collect::<Option<Vec<_>>>()?;
    let mut rows = Vec::new();
    for row in 0..5 {
        let pixels = glyphs
            .iter()
            .map(|glyph| glyph[row])
            .collect::<Vec<_>>()
            .join(".");
        let wide: String = pixels
            .chars()
            .flat_map(|pixel| std::iter::repeat_n(if pixel == '#' { '#' } else { ' ' }, 2 * scale))
            .collect();
        rows.extend(std::iter::repeat_n(wide, scale));
    }
    Some(rows)
}

/// The word the screen shows in big letters: the verdict once a result is on the screen (its
/// `RESULT: <VERDICT>` row; none for a verdict without glyphs), else FAIL from the first failure
/// counted and PASS before it.
fn banner_word(dashboard: &UnifiedDashboard) -> Option<&'static str> {
    if let Some(row) = dashboard
        .running_rows
        .iter()
        .find(|row| row.starts_with("RESULT: "))
    {
        let verdict = row["RESULT: ".len()..].split_whitespace().next()?;
        return ["PASS", "FAIL", "INVALID"]
            .into_iter()
            .find(|word| verdict.eq_ignore_ascii_case(word));
    }
    Some(if dashboard.failures > 0 {
        "FAIL"
    } else {
        "PASS"
    })
}

fn verdict_color(word: &str) -> &'static str {
    match word {
        "PASS" => GREEN,
        "FAIL" => RED,
        _ => YELLOW,
    }
}

/// On a result screen, one row per part of `FAIL BY PART` with failures, followed by the device
/// the hardware panel names for it: `RAM 1 (1 susp)  -  64 GiB total`.
fn failed_parts(dashboard: &UnifiedDashboard) -> Vec<String> {
    if !dashboard
        .running_rows
        .iter()
        .any(|row| row.starts_with("RESULT: "))
    {
        return Vec::new();
    }
    let Some(row) = dashboard
        .running_rows
        .iter()
        .find_map(|row| row.strip_prefix("FAIL BY PART"))
    else {
        return Vec::new();
    };
    row.split(" | ")
        .map(str::trim)
        .filter(|part| {
            part.split_whitespace()
                .find_map(|token| token.parse::<u64>().ok())
                .is_some_and(|count| count > 0)
        })
        .map(|part| match device_of(part, &dashboard.hardware_rows) {
            Some(device) => format!("{part}  -  {device}"),
            None => part.to_owned(),
        })
        .collect()
}

/// The device a `FAIL BY PART` cell is on, from the hardware rows (`CPU:`, `RAM:`, `GPU 2:` or a
/// single `GPU:`, `SSD 1:`). An unresolved group (`? PCIe-GPU1|GPU1|VRAM1`) is on its first
/// part's device; PLATFORM is on none.
fn device_of(part: &str, hardware_rows: &[String]) -> Option<String> {
    let name = part
        .split_whitespace()
        .find(|token| *token != "?")?
        .split('|')
        .next()?;
    let name = name.strip_prefix("PCIe-").unwrap_or(name);
    let number = |prefix: &str| -> Option<String> {
        let digits: String = name
            .strip_prefix(prefix)?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then_some(digits)
    };
    let labels = if name == "CPU" || name == "RAM" {
        vec![format!("{name}:")]
    } else if let Some(n) = number("GPU").or_else(|| number("VRAM")) {
        let mut labels = vec![format!("GPU {n}:")];
        if n == "1" {
            labels.push("GPU:".to_owned());
        }
        labels
    } else {
        vec![format!("SSD {}:", number("SSD")?)]
    };
    hardware_rows.iter().find_map(|row| {
        labels
            .iter()
            .find_map(|label| row.strip_prefix(label.as_str()))
            .map(|device| device.trim().to_owned())
            .filter(|device| !device.is_empty())
    })
}

/// `text` centred in `width` columns (left-aligned when it does not fit).
fn centred(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(text.chars().count()) / 2;
    format!("{}{text}", " ".repeat(pad))
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

/// Set by `dit-watchdog` for its worker (and by the CPU-RAM UI for its screen): the watchdog's
/// status file of the test being drawn.
pub const WATCHDOG_STATUS_ENV: &str = "DIT_WATCHDOG_STATUS";

/// The footer alert of a test with no progress for a minute or more (FC5), from its watchdog's
/// status (read at most every 2 s): nobody should stop or reset the machine without knowing.
fn progress_alert() -> Option<String> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
        std::sync::Mutex::new(None);
    let path = std::env::var_os(WATCHDOG_STATUS_ENV)?;
    let mut cache = CACHE.lock().ok()?;
    if let Some((at, alert)) = cache.as_ref()
        && at.elapsed() < std::time::Duration::from_secs(2)
    {
        return alert.clone();
    }
    let alert = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|record| {
            let age = record["progress_age_seconds"].as_u64()?;
            (age >= 60).then(|| {
                let moving = if record["state"] == "SLOW_IO" {
                    " (I/O still moving: slow, not stuck)"
                } else {
                    ""
                };
                format!(
                    "! NO PROGRESS {age} s{moving}; the watchdog decides at {} s",
                    record["stale_after_seconds"].as_u64().unwrap_or(0)
                )
            })
        });
    *cache = Some((std::time::Instant::now(), alert.clone()));
    alert
}

/// Where the image keeps the rescue console (`docs/FAULT_CATCH_ROADMAP.md`, FC1).
const RESCUE_CONSOLE: &str = "/usr/local/bin/dit-rescue";

/// The way out of a test screen that stopped answering, as the title band says it where the
/// rescue console runs: the longest that fits beside the engine's name.
fn rescue_hint(room: usize) -> Option<&'static str> {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*INSTALLED.get_or_init(|| std::path::Path::new(RESCUE_CONSOLE).is_file()) {
        return None;
    }
    [
        "Frozen? Alt+F2: rescue | Alt+PrtSc+F: free RAM",
        "Frozen? Alt+F2: rescue",
        "Frozen? Alt+F2",
    ]
    .into_iter()
    .find(|hint| hint.len() + 2 <= room)
}

/// The title row: the engine's name and version, centred (painted as a band), and on the right
/// the rescue console's key.
fn title_row(width: usize) -> String {
    let inner = width.saturating_sub(4);
    let name = super::product_title();
    let mut row: Vec<char> = format!("{name:^inner$}").chars().collect();
    let room = inner.saturating_sub(name.chars().count()) / 2;
    if let Some(hint) = rescue_hint(room) {
        let start = inner - hint.len() - 1;
        for (slot, c) in row[start..].iter_mut().zip(hint.chars()) {
            *slot = c;
        }
    }
    full_row(&row.into_iter().collect::<String>(), width)
}

/// The last dashboard drawn on a terminal, kept for [`save_final_frame`].
static LAST: std::sync::Mutex<Option<UnifiedDashboard>> = std::sync::Mutex::new(None);

/// When [`remember`] last wrote the screen to `DIT_UI_FINAL_FRAME`.
static LAST_SAVED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn remember(dashboard: &UnifiedDashboard) {
    if let Ok(mut last) = LAST.lock() {
        *last = Some(dashboard.clone());
    }
    // The black box draws a session's result on its last screen when the test cannot
    // (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a`): the file follows the screen every few seconds, not
    // only when the live view ends.
    let due = LAST_SAVED.lock().is_ok_and(|mut saved| {
        let due = saved.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(2));
        if due {
            *saved = Some(std::time::Instant::now());
        }
        due
    });
    if due {
        save_final_frame();
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

/// Writes the last dashboard drawn to `path` (whole, then renamed): a process that draws its
/// screen itself (the combined run) keeps its session's screen file this way.
pub fn save_last_frame(path: &std::path::Path) {
    let Some(dashboard) = LAST.lock().ok().and_then(|last| last.clone()) else {
        return;
    };
    let temporary = path.with_extension("tmp");
    if serde_json::to_vec(&dashboard).is_ok_and(|bytes| std::fs::write(&temporary, bytes).is_ok()) {
        let _ = std::fs::rename(&temporary, path);
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
    // The result's THROTTLE row (counted from the reports) replaces the live one.
    if rows.iter().any(|row| row.starts_with("THROTTLE")) {
        finished
            .running_rows
            .retain(|row| !row.starts_with("THROTTLE"));
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
    // A throttle or no-progress alert (footer) opens with a warning badge.
    for alert in ["! THROTTLE", "! NO PROGRESS"] {
        if let Some(at) = find_chars(&chars, alert) {
            color_range(at, at + alert.len(), BADGE_WARN);
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
    } else if let Some(at) = find_chars(&chars, "THROTTLE  ") {
        // A device that slowed itself down is yellow; one without counters grey.
        for (from, to) in segments(at + "THROTTLE".len()) {
            let text: String = chars[from..to].iter().collect();
            if text.contains("N/A") {
                color_range(from, to, GRAY);
            } else if text
                .split_whitespace()
                .find(|token| token.chars().all(|c| c.is_ascii_digit()))
                .and_then(|token| token.parse::<u64>().ok())
                .is_some_and(|count| count > 0)
            {
                color_range(from, to, YELLOW);
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
    paint(&lines, 0, "", unicode_terminal(), None)
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

/// `banner`: the big verdict's rows (first row, row count) and colour: its pixels are solid
/// blocks, its text (the failed parts) in the same colour.
#[allow(clippy::too_many_lines)]
fn paint(
    lines: &[String],
    failures: u64,
    footer: &str,
    unicode: bool,
    banner: Option<(usize, usize, &'static str)>,
) -> String {
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
                || line.contains("THROTTLE  ")
                || line.contains("RESULT: ");
            let is_footer = !footer_key.is_empty() && line.contains(&footer_key);
            let overrides = part_colors(line);
            let banner_color = banner
                .filter(|&(first, rows, _)| row >= first && row < first + rows)
                .map(|(_, _, color)| color);
            // A line is red for its own count, not for the run's: one layer's FAIL does not
            // turn the others red. A bare verdict word still follows the run's total.
            let text_color = if let Some(color) = banner_color {
                color
            } else if !by_part && (counter_above_zero || (bare_fail && failures > 0 && !is_footer))
            {
                RED
            } else if line.contains("PLATFORM      ") {
                // The cause of a failure a rule counted (FC5): red like the failure.
                RED
            } else if line.contains("[NO ROOM]") {
                // A setup option the hardware has no room for (an SSD write level).
                RED
            } else if line.contains("[FITS]") {
                GREEN
            } else if line.contains("[CHECKED AT START]") {
                YELLOW
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
                || line.contains("! THROTTLE")
                || line.contains("! NO PROGRESS")
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
                    // A badge's background must end with it. Grey (90) is bright black, and the
                    // Linux console turns bold on with it: the colours after it would come out
                    // bright (a bar's green or orange, the text) unless that ends too.
                    if previous_color.contains(';') || previous_color == GRAY {
                        result.push_str(RESET);
                    }
                    result.push_str(current);
                    previous_color = current;
                }
                result.push(if banner_color.is_some() && character == '#' && unicode {
                    // A pixel of the big verdict: a full block.
                    '\u{2588}'
                } else {
                    shown.get(index).copied().unwrap_or(character)
                });
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

    /// The content of a framed row, without the frame.
    fn inside(line: &str) -> &str {
        line.trim_start_matches('|')
            .trim_end_matches('|')
            .trim_end()
    }

    /// PASS until a failure is counted, then FAIL: in big letters on the TESTING panel's last
    /// rows, right above the footer, centred, as big as the spare rows allow.
    #[test]
    fn big_verdict_sits_at_the_bottom_of_the_testing_panel() {
        let size = TerminalSize {
            cols: 160,
            rows: 50,
        };
        for (failures, word, color) in [(0, "PASS", GREEN), (1, "FAIL", RED)] {
            let mut dashboard = model();
            dashboard.failures = failures;
            let frame = render_unified_dashboard(&dashboard, size, false);
            let lines: Vec<&str> = frame.lines().collect();
            let art = big_word(word, 3).expect("glyphs");
            let footer = lines.len() - 2;
            assert!(lines[footer].contains("TEMP LIMIT OFF"));
            for (offset, row) in art.iter().enumerate() {
                let line = lines[footer - art.len() + offset];
                assert_eq!(inside(line).trim(), row.trim(), "{word} row {offset}");
            }
            // Centred as a block: as much room on its left as on its right.
            let block = &lines[footer - art.len()..footer];
            let room = |side: fn(&str) -> &str| {
                block
                    .iter()
                    .map(|line| {
                        let content = &line[1..line.len() - 1];
                        content.len() - side(content).len()
                    })
                    .min()
                    .unwrap_or(0)
            };
            let (left, right) = (room(str::trim_start), room(str::trim_end));
            assert!(left.abs_diff(right) <= 1, "{left} / {right}");
            let painted = render_unified_dashboard(&dashboard, size, true);
            let pixel = if unicode_terminal() { "\u{2588}" } else { "#" };
            assert!(
                painted
                    .lines()
                    .nth(footer - 1)
                    .is_some_and(|line| line.contains(&format!("{color} ")) && line.contains(pixel)),
                "{word} in its colour"
            );
        }
        // A smaller screen gets smaller letters.
        let small = TerminalSize { cols: 90, rows: 34 };
        let frame = render_unified_dashboard(&model(), small, false);
        let art = big_word("PASS", 1).expect("glyphs");
        let lines: Vec<&str> = frame.lines().collect();
        assert_eq!(inside(lines[lines.len() - 3]).trim(), art[4].trim());
    }

    /// No spare rows for the letters: the screen is drawn as before, the badge of the state row
    /// still says it.
    #[test]
    fn no_big_verdict_without_room() {
        let mut dashboard = model();
        dashboard.running_rows = (0..12).map(|i| format!("ROW {i}")).collect();
        let size = TerminalSize { cols: 90, rows: 24 };
        let frame = render_unified_dashboard(&dashboard, size, false);
        assert!(frame.lines().count() < 24);
        let art = |line: &str| {
            let content = inside(line).trim();
            !content.is_empty() && content.chars().all(|c| c == '#' || c == ' ')
        };
        assert!(!frame.lines().any(art), "{frame}");
    }

    /// A result screen: the verdict in big letters, then each part that failed with the device
    /// the hardware panel names for it; parts without failures are not listed.
    #[test]
    fn result_lists_the_failed_parts_under_the_big_verdict() {
        let mut dashboard = model();
        dashboard.hardware_rows = vec![
            "CPU:     AMD Ryzen 9 9950X".to_owned(),
            "RAM:     64 GiB total".to_owned(),
            "GPU 1:   NVIDIA GeForce RTX 5080 [0000:01:00.0]".to_owned(),
            "SSD 1:   Samsung SSD 990 PRO 1TB".to_owned(),
        ];
        dashboard.failures = 4;
        let rows = [
            "RESULT: FAIL    COMBINED DEEP".to_owned(),
            "FAIL BY PART  CPU 0 | RAM 1 (1 susp) | ? PCIe-GPU1|GPU1|VRAM1 2 | SSD1-path 1 | PLATFORM 1"
                .to_owned(),
        ];
        let size = TerminalSize {
            cols: 160,
            rows: 50,
        };
        let frame = render_result_frame(&dashboard, &rows, "Enter: menu", size, false);
        let lines: Vec<&str> = frame.lines().collect();
        let footer = lines.len() - 2;
        let listed: Vec<&str> = lines[footer - 4..footer]
            .iter()
            .map(|line| inside(line).trim())
            .collect();
        assert_eq!(
            listed,
            [
                "RAM 1 (1 susp)  -  64 GiB total",
                "? PCIe-GPU1|GPU1|VRAM1 2  -  NVIDIA GeForce RTX 5080 [0000:01:00.0]",
                "SSD1-path 1  -  Samsung SSD 990 PRO 1TB",
                "PLATFORM 1",
            ]
        );
        let art = big_word("FAIL", 3).expect("glyphs");
        assert_eq!(inside(lines[footer - 5]).trim(), art[14].trim());
        let painted = render_result_frame(&dashboard, &rows, "Enter: menu", size, true);
        assert!(
            painted
                .lines()
                .nth(footer - 4)
                .is_some_and(|line| line.contains(&format!("{RED} "))),
            "the failed parts are red"
        );
        // An INVALID result: the word in yellow, no part listed.
        let rows = ["RESULT: INVALID    stopped by the operator".to_owned()];
        let frame = render_result_frame(&model(), &rows, "Enter: menu", size, false);
        let art = big_word("INVALID", 2).expect("glyphs");
        let lines: Vec<&str> = frame.lines().collect();
        assert_eq!(inside(lines[lines.len() - 3]).trim(), art[9].trim());
    }

    #[test]
    fn failed_parts_are_found_on_their_devices() {
        let rows = vec![
            "GPU:     NVIDIA GeForce GTX 1050 Ti".to_owned(),
            "SSD 2:   Lexar NM610 PRO".to_owned(),
        ];
        assert_eq!(
            device_of("VRAM1 3 (3 susp)", &rows).as_deref(),
            Some("NVIDIA GeForce GTX 1050 Ti")
        );
        assert_eq!(
            device_of("PCIe-GPU1 1", &rows).as_deref(),
            Some("NVIDIA GeForce GTX 1050 Ti")
        );
        assert_eq!(
            device_of("SSD2-path 1", &rows).as_deref(),
            Some("Lexar NM610 PRO")
        );
        assert_eq!(
            device_of("? SSD2|SSD2-path 1", &rows).as_deref(),
            Some("Lexar NM610 PRO")
        );
        assert_eq!(device_of("GPU2 1", &rows), None);
        assert_eq!(device_of("PLATFORM 1", &rows), None);
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

    /// Grey (bright black) turns bold on at the Linux console: whatever follows it starts from a
    /// reset, so a bar's fill and the text after the bar keep their normal intensity.
    #[test]
    fn colours_after_grey_start_from_a_reset() {
        let painted = paint(
            &["| CPU USAGE [#####-----] 50% |".to_owned()],
            0,
            "",
            false,
            None,
        );
        assert!(painted.contains(GRAY));
        for (at, _) in painted.match_indices(GRAY) {
            let after = &painted[at + GRAY.len()..];
            let next = after.find('\x1b').map_or("", |i| &after[i..]);
            assert!(next.is_empty() || next.starts_with(RESET), "{painted:?}");
        }
    }

    /// Setup options are coloured by whether they fit: green, red when there is no room, yellow
    /// when it is checked at the start.
    #[test]
    fn setup_options_are_coloured_by_their_fit() {
        let colour = |text: &str| {
            let painted = paint(&[format!("| {text} |")], 0, "", false, None);
            [GREEN, RED, YELLOW]
                .into_iter()
                .find(|code| painted.contains(&format!("{code} S) Standard")))
        };
        assert_eq!(colour("S) Standard  16 GiB per SSD   [FITS]"), Some(GREEN));
        assert_eq!(
            colour("S) Standard  16 GiB per SSD   [NO ROOM] SSD [1] has 2.0 GiB free"),
            Some(RED)
        );
        assert_eq!(
            colour("S) Standard  16 GiB per SSD   [CHECKED AT START] SSD [1]"),
            Some(YELLOW)
        );
    }

    /// Each device of the THROTTLE line in its colour: yellow when it throttled, grey without
    /// counters, plain at zero.
    #[test]
    fn throttle_parts_are_coloured_per_device() {
        let line = "| THROTTLE      CPU 0 | GPU1 3 (42 s) | SSD1 N/A |".to_owned();
        let painted = paint(&[line], 0, "", false, None);
        assert!(
            painted.contains(&format!("{YELLOW}GPU1 3 (42 s)")),
            "{painted:?}"
        );
        assert!(painted.contains(&format!("{GRAY}SSD1 N/A")), "{painted:?}");
        assert!(!painted.contains(&format!("{YELLOW}CPU 0")), "{painted:?}");
    }

    /// A throttle alert opens with a warning badge and stays yellow after it.
    #[test]
    fn a_throttle_alert_opens_with_a_badge() {
        let line = "| ! THROTTLE CPU +1 at 00:12:31 - it slowed itself down |".to_owned();
        let painted = paint(&[line], 0, "", false, None);
        assert!(
            painted.contains(&format!("{BADGE_WARN}! THROTTLE")),
            "{painted:?}"
        );
        assert!(painted.contains(&format!("{YELLOW} CPU +1")), "{painted:?}");
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
