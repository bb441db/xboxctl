//! `xboxctl diag`: what the controller reports about itself, then a live view
//! of its buttons, sticks and triggers that also watches for stick drift.
//!
//! Nothing is written to the controller. The live view only listens to the
//! input reports and answers the ACKs the controller asks for.

use crate::{
    gip::{
        frame::{self, Packet},
        report::{BUTTONS, GUIDE, InputReport, SHARE, guide_button},
        updater::Link,
    },
    live::Live,
    print_info,
    usb::{self, UsbLink},
};
use std::time::{Duration, Instant};

/// Holding the Xbox button this long ends the live view.
const EXIT_HOLD: Duration = Duration::from_secs(2);

/// Stick travel, as a fraction of full travel, that still counts as the same
/// resting position.
const JITTER: f64 = 0.03;
/// How long a stick must stay put before it is judged.
const STEADY: Duration = Duration::from_secs(3);
/// A stick that rests this far from the centre, or further, is drifting.
const DRIFT_MIN: f64 = 0.06;
/// Further than this, the stick is probably held on purpose.
const DRIFT_MAX: f64 = 0.30;

/// Travel from which a stick or trigger counts as used, for the summary.
const MOVED: f64 = 0.15;

/// Judges whether a stick that is not moving rests at the centre.
///
/// The controller only reports changes, so the position is the last one
/// seen, and the time since it last moved is what counts.
#[derive(Debug, Default)]
struct RestWatch {
    anchor: (f64, f64),
    since: Duration,
    /// Held still for long enough to judge at least once.
    judged: bool,
    /// Where it was last judged to rest, and whether that looked like drift.
    rested_at: Option<(f64, f64)>,
    drift: Option<(f64, f64)>,
    reported: bool,
}

impl RestWatch {
    /// Feed the stick position (-1 to 1 on each axis) at `now`. Returns the
    /// resting position when the stick is first judged to be drifting.
    fn update(&mut self, now: Duration, x: f64, y: f64) -> Option<(f64, f64)> {
        if (x - self.anchor.0).abs() > JITTER || (y - self.anchor.1).abs() > JITTER {
            self.anchor = (x, y);
            self.since = now;
            self.reported = false;
            return None;
        }
        if now.saturating_sub(self.since) < STEADY {
            return None;
        }
        self.judged = true;
        let (x, y) = self.anchor;
        let offset = x.hypot(y);
        if (DRIFT_MIN..=DRIFT_MAX).contains(&offset) {
            self.drift = Some((x, y));
            if !self.reported {
                self.reported = true;
                return Some((x, y));
            }
        } else if offset < DRIFT_MIN {
            self.rested_at = Some((x, y));
            self.drift = None;
        }
        None
    }
}

fn percent(value: f64) -> i32 {
    (value * 100.0).round() as i32
}

fn normalised(axis: i16) -> f64 {
    f64::from(axis) / 32768.0
}

/// Everything the live view tracks.
#[derive(Debug, Default)]
struct View {
    report: InputReport,
    /// The first report: the resting state, if nothing was touched yet. The
    /// reach below only counts what comes after it.
    first: Option<InputReport>,
    buttons_seen: u16,
    share_seen: bool,
    /// The Xbox button, which comes in its own packets.
    guide: bool,
    guide_seen: bool,
    left_reach: f64,
    right_reach: f64,
    left_trigger_peak: u16,
    right_trigger_peak: u16,
    left_rest: RestWatch,
    right_rest: RestWatch,
    exit_since: Option<Duration>,
    /// At least one input report has arrived. Until then the positions are
    /// not known, only assumed to be at rest.
    fed: bool,
}

impl View {
    fn feed(&mut self, report: InputReport) {
        self.fed = true;
        self.report = report;
        self.buttons_seen |= report.buttons;
        self.share_seen |= report.share;
        if self.first.is_none() {
            self.first = Some(report);
            return;
        }
        let reach = |(x, y): (i16, i16)| normalised(x).hypot(normalised(y)).min(1.0);
        self.left_reach = self.left_reach.max(reach(report.left));
        self.right_reach = self.right_reach.max(reach(report.right));
        self.left_trigger_peak = self.left_trigger_peak.max(report.left_trigger);
        self.right_trigger_peak = self.right_trigger_peak.max(report.right_trigger);
    }

    fn set_guide(&mut self, down: bool) {
        self.guide = down;
        self.guide_seen |= down;
    }

    /// Names of the buttons that are down.
    fn pressed(&self) -> Vec<&'static str> {
        let mut names = self.report.pressed();
        if self.guide {
            names.push(GUIDE);
        }
        names
    }

    /// Advance time with the last known positions. Returns messages for new
    /// findings, and sets `exit_since` while the Xbox button is held.
    fn tick(&mut self, now: Duration) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.fed {
            return notes;
        }
        let r = self.report;
        for (name, watch, (x, y)) in [
            ("Left", &mut self.left_rest, r.left),
            ("Right", &mut self.right_rest, r.right),
        ] {
            if let Some((x, y)) = watch.update(now, normalised(x), normalised(y)) {
                notes.push(format!(
                    "  ! The {} stick rests off-centre (x {:+}%, y {:+}%). That can be stick \
                     drift; `xboxctl calibrate --stick {}` may help.",
                    name.to_lowercase(),
                    percent(x),
                    percent(y),
                    name.to_lowercase()
                ));
            }
        }
        match (self.guide, self.exit_since) {
            (true, None) => self.exit_since = Some(now),
            (false, _) => self.exit_since = None,
            _ => {}
        }
        notes
    }

    fn exit_requested(&self, now: Duration) -> bool {
        self.exit_since
            .is_some_and(|since| now.saturating_sub(since) >= EXIT_HOLD)
    }

    fn line(&self) -> String {
        if !self.fed {
            return "  Waiting for the first input report: press any button".to_owned();
        }
        let r = &self.report;
        let pressed = self.pressed();
        let buttons = if pressed.is_empty() {
            "-".to_owned()
        } else {
            pressed.join(" ")
        };
        let trigger =
            |value: u16| u32::from(value) * 100 / u32::from(crate::gip::calibration::Triggers::MAX);
        format!(
            "  {buttons:<26}L {:>+4}% {:>+4}%   R {:>+4}% {:>+4}%   LT {:>3}%  RT {:>3}%",
            percent(normalised(r.left.0)),
            percent(normalised(r.left.1)),
            percent(normalised(r.right.0)),
            percent(normalised(r.right.1)),
            trigger(r.left_trigger),
            trigger(r.right_trigger),
        )
    }

    /// The end-of-session report: lines to print indented, an empty string
    /// for a blank line.
    fn summary(&self) -> Vec<String> {
        let Some(first) = self.first.filter(|_| self.fed) else {
            return vec!["The controller sent no input report, so nothing was checked".to_owned()];
        };
        let mut lines = Vec::new();

        let all = BUTTONS.len() + 2;
        let missing: Vec<&str> = BUTTONS
            .iter()
            .filter(|(bit, _)| self.buttons_seen & bit == 0)
            .map(|&(_, name)| name)
            .chain((!self.share_seen).then_some(SHARE))
            .chain((!self.guide_seen).then_some(GUIDE))
            .collect();
        lines.push(if missing.is_empty() {
            format!("Buttons        all {all} pressed")
        } else {
            format!(
                "Buttons        {} of {all} pressed (not pressed: {})",
                all - missing.len(),
                missing.join(", ")
            )
        });
        lines.push(String::new());

        let trigger =
            |value: u16| f64::from(value) / f64::from(crate::gip::calibration::Triggers::MAX);
        let reach = |value: f64, unused: &str| {
            if value < MOVED {
                unused.to_owned()
            } else {
                format!("{}%", percent(value))
            }
        };
        let row = |control: &str, rest: &str, reach: &str, drift: &str| {
            format!("{control:<15}{rest:<13}{reach:<13}{drift}")
                .trim_end()
                .to_owned()
        };
        let position = |(x, y): (f64, f64)| format!("{:+}%, {:+}%", percent(x), percent(y));
        let drift = |watch: &RestWatch| match (watch.drift, watch.rested_at, watch.judged) {
            (Some(at), _, _) => format!("possible drift ({})", position(at)),
            (None, Some(_), _) => "none seen".to_owned(),
            (None, None, true) => "held off-centre, not judged".to_owned(),
            (None, None, false) => "not judged (not left alone)".to_owned(),
        };
        let stick = |(x, y): (i16, i16)| position((normalised(x), normalised(y)));

        lines.push(row("Control", "At rest", "Reach", "Drift"));
        lines.push(row(
            "Left stick",
            &stick(first.left),
            &reach(self.left_reach, "not moved"),
            &drift(&self.left_rest),
        ));
        lines.push(row(
            "Right stick",
            &stick(first.right),
            &reach(self.right_reach, "not moved"),
            &drift(&self.right_rest),
        ));
        lines.push(row(
            "Left trigger",
            &format!("{}%", percent(trigger(first.left_trigger))),
            &reach(trigger(self.left_trigger_peak), "not pressed"),
            "",
        ));
        lines.push(row(
            "Right trigger",
            &format!("{}%", percent(trigger(first.right_trigger))),
            &reach(trigger(self.right_trigger_peak), "not pressed"),
            "",
        ));
        lines.push(String::new());

        let drifting: Vec<&str> = [("left", &self.left_rest), ("right", &self.right_rest)]
            .into_iter()
            .filter(|(_, watch)| watch.drift.is_some())
            .map(|(name, _)| name)
            .collect();
        let untested = !missing.is_empty()
            || self.left_reach < MOVED
            || self.right_reach < MOVED
            || trigger(self.left_trigger_peak) < MOVED
            || trigger(self.right_trigger_peak) < MOVED;
        lines.push(if let Some(name) = drifting.first() {
            format!(
                "Result         possible drift on the {} stick; `xboxctl calibrate --stick {name}` \
                 may help",
                drifting.join(" and ")
            )
        } else if untested {
            "Result         nothing wrong seen, but not everything was used".to_owned()
        } else {
            "Result         everything responded and no drift was seen".to_owned()
        });
        lines
    }
}

/// Receive one packet, answering the ACK it asks for.
fn receive(link: &mut UsbLink, wait: Duration) -> Result<Option<Packet>, String> {
    let deadline = Instant::now() + wait;
    while let Some(bytes) = link.recv(deadline.saturating_duration_since(Instant::now()))? {
        if let Some(packet) = Packet::parse(&bytes) {
            if packet.wants_ack() {
                let received = packet.payload.len() as u32;
                link.send(&frame::ack(&packet, received, 0))?;
            }
            return Ok(Some(packet));
        }
    }
    Ok(None)
}

pub fn run(device: Option<&str>, duration: Option<Duration>, trace: bool) -> Result<(), String> {
    let controller = usb::select_controller(device)?;
    println!("{}", crate::describe(&controller));
    if let Some(serial) = controller.serial() {
        println!("Serial         {serial}");
    }
    let mut link = UsbLink::open(&controller, trace)?;
    print_info(&controller, &mut link)?;

    let mut view = View::default();

    println!("\nLive view");
    println!("  - Press any button to start. The controller only reports changes, so this");
    println!("    first report also shows where the sticks rest.");
    println!("  - Press every button and push the sticks and triggers all the way.");
    println!("  - Leave the sticks alone for a few seconds to check for drift.");
    match duration {
        Some(limit) => println!(
            "  - It stops after {} seconds, or hold the Xbox button for two seconds.\n",
            limit.as_secs()
        ),
        None => println!("  - Hold the Xbox button for two seconds to finish, or press Ctrl-C.\n"),
    }
    let mut live = Live::new();
    let began = Instant::now();
    loop {
        let now = began.elapsed();
        if duration.is_some_and(|limit| now >= limit) || view.exit_requested(now) {
            break;
        }
        if let Some(packet) = receive(&mut link, Duration::from_millis(50))? {
            if let Some(report) = InputReport::parse(&packet) {
                view.feed(report);
            } else if let Some(down) = guide_button(&packet) {
                view.set_guide(down);
            }
        }
        for note in view.tick(began.elapsed()) {
            live.done(&note);
        }
        live.show(&view.line());
    }
    live.done(&view.line());

    println!("\nSummary\n");
    for line in view.summary() {
        if line.is_empty() {
            println!();
        } else {
            println!("  {line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_stick_resting_off_centre_is_reported_once() {
        let mut watch = RestWatch::default();
        assert_eq!(watch.update(secs(0), 0.0, 0.0), None);
        assert_eq!(watch.update(secs(1), 0.1, -0.02), None);
        assert_eq!(watch.update(secs(3), 0.1, -0.02), None);
        assert_eq!(watch.update(secs(4), 0.1, -0.02), Some((0.1, -0.02)));
        assert_eq!(watch.update(secs(9), 0.1, -0.02), None);
        assert_eq!(watch.drift, Some((0.1, -0.02)));
    }

    #[test]
    fn a_stick_resting_at_the_centre_is_fine() {
        let mut watch = RestWatch::default();
        assert_eq!(watch.update(secs(0), 0.0, 0.0), None);
        assert_eq!(watch.update(secs(5), 0.01, 0.0), None);
        assert!(watch.judged);
        assert_eq!(watch.drift, None);
        assert_eq!(watch.rested_at, Some((0.0, 0.0)));
    }

    #[test]
    fn a_stick_held_far_out_is_not_drift() {
        let mut watch = RestWatch::default();
        watch.update(secs(0), 0.8, 0.0);
        assert_eq!(watch.update(secs(5), 0.8, 0.0), None);
        assert!(watch.judged);
        assert_eq!(watch.drift, None);
        assert_eq!(watch.rested_at, None);
    }

    #[test]
    fn moving_the_stick_restarts_the_wait() {
        let mut watch = RestWatch::default();
        watch.update(secs(0), 0.1, 0.0);
        assert_eq!(watch.update(secs(2), 0.2, 0.0), None);
        assert_eq!(watch.update(secs(4), 0.2, 0.0), None);
        assert_eq!(watch.update(secs(5), 0.2, 0.0), Some((0.2, 0.0)));
    }

    #[test]
    fn small_jitter_does_not_restart_the_wait() {
        let mut watch = RestWatch::default();
        watch.update(secs(0), 0.1, 0.0);
        assert_eq!(watch.update(secs(2), 0.11, 0.01), None);
        assert_eq!(watch.update(secs(3), 0.09, -0.01), Some((0.1, 0.0)));
    }

    fn report(buttons: u16) -> InputReport {
        InputReport {
            buttons,
            ..InputReport::default()
        }
    }

    #[test]
    fn nothing_is_judged_before_the_first_report() {
        let mut view = View::default();
        assert!(view.tick(secs(10)).is_empty());
        assert!(!view.left_rest.judged);
        assert!(view.line().contains("press any button"));
        assert_eq!(view.summary().len(), 1);
        view.feed(InputReport {
            left: (3277, 0),
            ..InputReport::default()
        });
        view.tick(secs(11));
        assert_eq!(view.tick(secs(15)).len(), 1);
        // The resting state is the first report, not a later one.
        view.feed(InputReport {
            left: (0, 0),
            ..InputReport::default()
        });
        assert_eq!(view.first.unwrap().left, (3277, 0));
    }

    #[test]
    fn holding_the_xbox_button_ends_the_view() {
        let mut view = View::default();
        view.feed(report(0));
        view.set_guide(true);
        view.tick(secs(10));
        assert!(!view.exit_requested(secs(11)));
        view.tick(secs(11));
        assert!(view.exit_requested(secs(12)));
        // Letting go in time cancels it.
        view.set_guide(false);
        view.tick(secs(12));
        assert!(!view.exit_requested(secs(20)));
        // A tap does not end it, and View and Menu are ordinary buttons.
        view.feed(report(1 << 2 | 1 << 3));
        view.tick(secs(30));
        assert!(!view.exit_requested(secs(40)));
    }

    #[test]
    fn the_summary_is_a_table_of_controls() {
        let mut view = View::default();
        // The first report is the resting state: a slightly off stick.
        view.feed(InputReport {
            buttons: 1 << 4,
            left: (1311, -328),
            ..InputReport::default()
        });
        let summary = view.summary();
        assert_eq!(
            summary,
            [
                "Buttons        1 of 16 pressed (not pressed: Menu, View, B, X, Y, Up, Down, Left, \
                 Right, LB, RB, LS, RS, Share, Guide)",
                "",
                "Control        At rest      Reach        Drift",
                "Left stick     +4%, -1%     not moved    not judged (not left alone)",
                "Right stick    +0%, +0%     not moved    not judged (not left alone)",
                "Left trigger   0%           not pressed",
                "Right trigger  0%           not pressed",
                "",
                "Result         nothing wrong seen, but not everything was used",
            ]
        );
        view.feed(InputReport {
            left: (32767, 0),
            right_trigger: 1023,
            ..InputReport::default()
        });
        let summary = view.summary();
        assert!(summary[3].starts_with("Left stick     +4%, -1%     100%"));
        assert!(summary[6].starts_with("Right trigger  0%           100%"));
    }

    #[test]
    fn the_xbox_button_is_shown_and_counted() {
        let mut view = View::default();
        view.feed(report(1 << 4));
        view.set_guide(true);
        assert!(view.line().contains("A Guide"));
        view.set_guide(false);
        assert!(!view.line().contains("Guide"));
        assert!(view.summary()[0].contains("2 of 16 pressed"));
        assert!(!view.summary()[0].contains("Guide)"));
    }

    #[test]
    fn the_summary_names_a_drifting_stick() {
        let mut view = View::default();
        view.feed(InputReport {
            left: (3277, 0),
            ..InputReport::default()
        });
        view.tick(secs(1));
        view.tick(secs(5));
        let summary = view.summary();
        assert!(summary[3].ends_with("possible drift (+10%, +0%)"));
        assert!(
            summary
                .last()
                .unwrap()
                .contains("possible drift on the left stick")
        );
        assert!(summary.last().unwrap().contains("--stick left"));
    }

    #[test]
    fn the_live_line_shows_buttons_and_positions() {
        let mut view = View::default();
        view.feed(InputReport {
            buttons: 1 << 4 | 1 << 12,
            left: (16384, -32768),
            left_trigger: 1023,
            ..InputReport::default()
        });
        let line = view.line();
        assert!(line.contains("A LB"));
        assert!(line.contains("L  +50% -100%"));
        assert!(line.contains("LT 100%  RT   0%"));
    }
}
