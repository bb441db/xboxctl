//! `xboxctl calibrate`: a guided thumbstick and trigger calibration.
//!
//! The controller does the work. One request starts a calibration, and the
//! controller finishes it by itself once the stick has been turned through
//! its full range. This module guides the user through that, follows it, and
//! ends with a check that pushes the stick in each direction (or presses the
//! trigger). See
//! `gip::calibration` for the wire format.

use crate::{
    gip::{
        calibration::{self, Axes, Controls, Limits, Response, TriggerLimits, Triggers},
        frame::{self, Packet, Sequence},
        updater::Link,
    },
    live::Live,
    start_device,
    usb::{self, UsbLink},
};
use clap::ValueEnum;
use std::{
    f64::consts::{PI, TAU},
    io::{self, BufRead, Write},
    time::{Duration, Instant},
};

/// Full circles the wizard asks for in each direction.
const ROTATIONS: u32 = 6;
/// Full presses the wizard asks for on a trigger (what the Xbox Accessories
/// app asks for).
const PRESSES: u32 = 12;
/// Calibration status while the controller saves the result.
const STATUS_SAVING: u8 = 0x10;
/// No movement for this long ends the calibration with an error.
const IDLE_LIMIT: Duration = Duration::from_secs(180);
/// How long the controller may take to finish after the last rotation.
const FINISH_LIMIT: Duration = Duration::from_secs(20);
/// How long to wait for the user to start each push of the check.
const PUSH_WAIT: Duration = Duration::from_secs(30);
/// How long a push may take from start to rest before it is judged anyway.
const PUSH_LIMIT: Duration = Duration::from_secs(10);
/// No input report for this long means the stick has stopped moving; the
/// controller only reports changes.
const SETTLE: Duration = Duration::from_millis(300);

const NO_REPLY: &str = "no reply to the calibration request";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Side {
    Left,
    Right,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stick {
    Left,
    Right,
}

impl Stick {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    fn controls(self) -> Controls {
        match self {
            Self::Left => Controls::LEFT_STICK,
            Self::Right => Controls::RIGHT_STICK,
        }
    }

    fn position(self, axes: &Axes) -> (i16, i16) {
        match self {
            Self::Left => (axes.left_x, axes.left_y),
            Self::Right => (axes.right_x, axes.right_y),
        }
    }

    fn limits(self, response: &Response) -> Option<Limits> {
        match (self, response) {
            (Self::Left, Response::Read { left, .. }) => *left,
            (Self::Right, Response::Read { right, .. }) => *right,
            _ => None,
        }
    }
}

/// Counts the rotations a stick makes at its outer edge, in each direction.
#[derive(Debug, Default)]
struct RotationCounter {
    last_angle: Option<f64>,
    clockwise: f64,
    counter_clockwise: f64,
}

impl RotationCounter {
    /// Stick radius, as a fraction of full travel, from which it counts as
    /// being at the outer edge.
    const EDGE: f64 = 0.85;
    /// Angle changes smaller than this (radians) are noise.
    const MIN_STEP: f64 = 0.02;

    fn feed(&mut self, x: i16, y: i16) {
        let (x, y) = (f64::from(x) / 32768.0, f64::from(y) / 32768.0);
        if x.hypot(y) < Self::EDGE {
            self.last_angle = None;
            return;
        }
        // Y is up, so the angle grows counter-clockwise.
        let angle = y.atan2(x);
        if let Some(last) = self.last_angle {
            let mut step = angle - last;
            if step > PI {
                step -= TAU;
            } else if step < -PI {
                step += TAU;
            }
            if step.abs() >= Self::MIN_STEP {
                if step < 0.0 {
                    self.clockwise -= step;
                } else {
                    self.counter_clockwise += step;
                }
            }
        }
        self.last_angle = Some(angle);
    }

    /// Complete turns so far: (clockwise, counter-clockwise).
    fn turns(&self) -> (f64, f64) {
        (self.clockwise / TAU, self.counter_clockwise / TAU)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Right,
    Down,
    Left,
}

impl Direction {
    const ALL: [Self; 4] = [Self::Up, Self::Right, Self::Down, Self::Left];

    fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Right => "right",
            Self::Down => "down",
            Self::Left => "left",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Up => "Up",
            Self::Right => "Right",
            Self::Down => "Down",
            Self::Left => "Left",
        }
    }

    /// How far a position (-1 to 1 on each axis, Y up) is in this direction.
    fn along(self, x: f64, y: f64) -> f64 {
        match self {
            Self::Up => y,
            Self::Down => -y,
            Self::Right => x,
            Self::Left => -x,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    ShortOfEdge,
    OffCentre,
    NoMovement,
}

/// One push of the check: all the way in a direction, then let go.
#[derive(Debug)]
struct PushCheck {
    direction: Direction,
    started: bool,
    peak: f64,
    last: (f64, f64),
}

impl PushCheck {
    /// Travel in the direction from which the push counts as started.
    const START: f64 = 0.5;
    /// Travel in the direction that counts as reaching the edge.
    const REACH: f64 = 0.9;
    /// Distance from the centre, as a fraction of full travel, that still
    /// counts as back at rest.
    const CENTRE: f64 = 0.06;
    /// Distance from the centre below which the stick may be judged once it
    /// stops moving.
    const RELEASED: f64 = 0.3;

    fn new(direction: Direction) -> Self {
        Self {
            direction,
            started: false,
            peak: 0.0,
            last: (0.0, 0.0),
        }
    }

    fn feed(&mut self, x: i16, y: i16) {
        let (x, y) = (f64::from(x) / 32768.0, f64::from(y) / 32768.0);
        self.last = (x, y);
        let along = self.direction.along(x, y);
        if along >= Self::START {
            self.started = true;
        }
        if self.started {
            self.peak = self.peak.max(along);
        }
    }

    /// The stick was pushed and has come back close enough to rest to judge.
    fn released(&self) -> bool {
        self.started && self.last.0.hypot(self.last.1) < Self::RELEASED
    }

    fn outcome(&self) -> Outcome {
        if !self.started {
            Outcome::NoMovement
        } else if self.peak < Self::REACH {
            Outcome::ShortOfEdge
        } else if self.last.0.hypot(self.last.1) > Self::CENTRE {
            Outcome::OffCentre
        } else {
            Outcome::Pass
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Left,
    Right,
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    fn controls(self) -> Controls {
        match self {
            Self::Left => Controls::LEFT_TRIGGER,
            Self::Right => Controls::RIGHT_TRIGGER,
        }
    }

    fn position(self, triggers: &Triggers) -> u16 {
        match self {
            Self::Left => triggers.left,
            Self::Right => triggers.right,
        }
    }

    fn limits(self, response: &Response) -> Option<TriggerLimits> {
        match (self, response) {
            (Self::Left, Response::Read { left_trigger, .. }) => *left_trigger,
            (Self::Right, Response::Read { right_trigger, .. }) => *right_trigger,
            _ => None,
        }
    }
}

/// Counts the full presses a trigger gets: down to the end, then let go.
#[derive(Debug, Default)]
struct PressCounter {
    down: bool,
    presses: u32,
}

impl PressCounter {
    /// Travel, as a fraction of the full range, from which a trigger counts
    /// as pressed all the way in.
    const PRESSED: f64 = 0.9;
    /// Travel below which a trigger counts as let go.
    const RELEASED: f64 = 0.1;

    fn feed(&mut self, value: u16) {
        let fraction = f64::from(value) / f64::from(Triggers::MAX);
        if !self.down && fraction >= Self::PRESSED {
            self.down = true;
        } else if self.down && fraction <= Self::RELEASED {
            self.down = false;
            self.presses += 1;
        }
    }
}

/// The check after a trigger calibration, like the app's "Resting" and
/// "Range" columns: the trigger at rest reads close to 0, and pressed all
/// the way in it reaches the end of its range.
#[derive(Debug, Default)]
struct TriggerCheck {
    started: bool,
    peak: f64,
    last: f64,
    /// The highest and the last raw reading, for the report.
    peak_raw: u16,
    last_raw: u16,
}

impl TriggerCheck {
    /// Travel from which the press counts as started.
    const START: f64 = 0.5;
    /// Highest reading that still counts as resting.
    const REST: f64 = 0.04;
    /// Travel that counts as the end of the range.
    const REACH: f64 = 0.95;
    /// Travel below which the trigger may be judged once it stops moving.
    const RELEASED: f64 = 0.3;

    fn feed(&mut self, value: u16) {
        let fraction = f64::from(value) / f64::from(Triggers::MAX);
        self.last = fraction;
        self.last_raw = value;
        if fraction >= Self::START {
            self.started = true;
        }
        if self.started {
            self.peak = self.peak.max(fraction);
            self.peak_raw = self.peak_raw.max(value);
        }
    }

    fn released(&self) -> bool {
        self.started && self.last < Self::RELEASED
    }

    /// Reads close to 0 after letting go. The controller only reports changes,
    /// so there is no reading of the trigger before the press.
    fn resting(&self) -> bool {
        self.last <= Self::REST
    }

    fn reaches_end(&self) -> bool {
        self.peak >= Self::REACH
    }
}

struct Session {
    link: UsbLink,
    sequence: Sequence,
}

impl Session {
    fn ack(&mut self, packet: &Packet) -> Result<(), String> {
        if packet.wants_ack() {
            let received = packet.payload.len() as u32;
            self.link.send(&frame::ack(packet, received, 0))?;
        }
        Ok(())
    }

    /// Send a request and wait for the reply `wanted`. Everything else the
    /// controller sends meanwhile (input reports, calibration events) goes to
    /// `other`, after answering the ACK it asks for.
    fn exchange(
        &mut self,
        request: impl Fn(u8) -> Vec<u8>,
        wanted: fn(&Response) -> bool,
        other: &mut dyn FnMut(&Packet),
    ) -> Result<Response, String> {
        const ATTEMPTS: usize = 2;
        for _ in 0..ATTEMPTS {
            self.link.send(&request(self.sequence.take()))?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while let Some(bytes) = self
                .link
                .recv(deadline.saturating_duration_since(Instant::now()))?
            {
                let Some(packet) = Packet::parse(&bytes) else {
                    continue;
                };
                self.ack(&packet)?;
                match Response::parse(&packet) {
                    Some(response) if wanted(&response) => return Ok(response),
                    _ => other(&packet),
                }
            }
        }
        Err(NO_REPLY.to_owned())
    }

    fn read(
        &mut self,
        controls: Controls,
        other: &mut dyn FnMut(&Packet),
    ) -> Result<Response, String> {
        self.exchange(
            |n| calibration::read_packet(n, controls),
            |r| matches!(r, Response::Read { .. }),
            other,
        )
    }

    fn start(&mut self, controls: Controls) -> Result<(), String> {
        self.exchange(
            |n| calibration::recenter_packet(n, controls),
            |r| matches!(r, Response::Recenter { .. }),
            &mut |_| {},
        )
        .map(|_| ())
    }

    /// Keep answering the controller for `window`, passing on what it sends.
    fn pump(&mut self, window: Duration, other: &mut dyn FnMut(&Packet)) -> Result<(), String> {
        let deadline = Instant::now() + window;
        while let Some(bytes) = self
            .link
            .recv(deadline.saturating_duration_since(Instant::now()))?
        {
            if let Some(packet) = Packet::parse(&bytes) {
                self.ack(&packet)?;
                other(&packet);
            }
        }
        Ok(())
    }

    /// Restart the controller with the GIP reset and wait until it answers
    /// again. A calibration that was left unfinished does not survive this.
    fn restart(&mut self) -> Result<(), String> {
        self.link.send(&frame::packet(
            frame::CMD_SET_DEVICE_STATE,
            frame::FLAG_SYSTEM,
            0x02,
            &[frame::STATE_RESET],
        ))?;
        // The power-off status, or a dropped link, says the reset happened.
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Ok(Some(bytes)) = self
            .link
            .recv(deadline.saturating_duration_since(Instant::now()))
        {
            let off = Packet::parse(&bytes).is_some_and(|packet| {
                packet.command == frame::CMD_STATUS
                    && packet.payload.first().is_some_and(|s| s & 0xc0 == 0)
            });
            if off {
                break;
            }
        }
        self.link.reconnect(Duration::from_secs(30))?;
        start_device(&mut self.link)?;
        self.sequence = Sequence::default();
        std::thread::sleep(Duration::from_secs(1));
        Ok(())
    }
}

fn status(response: &Response) -> u8 {
    match response {
        Response::Read { status, .. } => *status,
        Response::Recenter { .. } => 0,
    }
}

fn describe_limits(limits: Option<Limits>) -> String {
    match limits {
        Some(l) if l.horizontal_max != 0 || l.vertical_max != 0 => format!(
            "horizontal {}-{}, vertical {}-{}",
            l.horizontal_min, l.horizontal_max, l.vertical_min, l.vertical_max
        ),
        _ => "no calibration stored (the controller uses its defaults)".to_owned(),
    }
}

/// Highest calibration status of the given controls; non-zero means a
/// calibration is still pending.
fn pending(session: &mut Session, groups: &[Controls]) -> Result<u8, String> {
    let mut highest = 0;
    for &controls in groups {
        highest = highest.max(status(&session.read(controls, &mut |_| {})?));
    }
    Ok(highest)
}

fn describe_trigger_limits(limits: Option<TriggerLimits>) -> String {
    match limits {
        Some(l) if l.max != 0 => format!("range {}-{}", l.min, l.max),
        _ => "no calibration stored (the controller uses its defaults)".to_owned(),
    }
}

pub fn run(
    device: Option<&str>,
    stick: Option<Side>,
    trigger: Option<Side>,
    dry_run: bool,
    yes: bool,
    trace: bool,
) -> Result<(), String> {
    let controller = usb::select_controller(device)?;
    println!("{} ({})", controller.model().0, controller.location());
    let mut link = UsbLink::open(&controller, trace)?;
    start_device(&mut link)?;
    let mut session = Session {
        link,
        sequence: Sequence::default(),
    };
    // Without a choice, the sticks are calibrated.
    let stick = stick.or(trigger.is_none().then_some(Side::Both));
    let sticks: Vec<Stick> = match stick {
        None => vec![],
        Some(Side::Left) => vec![Stick::Left],
        Some(Side::Right) => vec![Stick::Right],
        Some(Side::Both) => vec![Stick::Left, Stick::Right],
    };
    let triggers: Vec<Trigger> = match trigger {
        None => vec![],
        Some(Side::Left) => vec![Trigger::Left],
        Some(Side::Right) => vec![Trigger::Right],
        Some(Side::Both) => vec![Trigger::Left, Trigger::Right],
    };
    let mut groups = Vec::new();
    if !sticks.is_empty() {
        groups.push(Controls::BOTH_STICKS);
    }
    if !triggers.is_empty() {
        groups.push(Controls::BOTH_TRIGGERS);
    }

    // The controller only answers on firmware that supports calibration.
    let unsupported = |error: String| {
        if error == NO_REPLY {
            "This controller did not answer the calibration request, so it probably does \
                 not support calibration. Newer firmware might: see `xboxctl update --latest \
                 --dry-run`."
                .to_owned()
        } else {
            error
        }
    };
    let current = session.read(groups[0], &mut |_| {}).map_err(unsupported)?;

    if dry_run {
        for stick in &sticks {
            println!(
                "{} stick: {}",
                capitalised(stick.name()),
                describe_limits(stick.limits(&current))
            );
        }
        if !triggers.is_empty() {
            let response = session.read(Controls::BOTH_TRIGGERS, &mut |_| {})?;
            for trigger in &triggers {
                println!(
                    "{} trigger: {}",
                    capitalised(trigger.name()),
                    describe_trigger_limits(trigger.limits(&response))
                );
            }
        }
        println!("Nothing was changed.");
        return Ok(());
    }

    if pending(&mut session, &groups).map_err(unsupported)? != 0 {
        println!("An unfinished calibration was found; restarting the controller to clear it.");
        session.restart()?;
        if pending(&mut session, &groups)? != 0 {
            return Err(
                "the controller is still busy with an earlier calibration; unplug it, plug it \
                 in again and try once more"
                    .to_owned(),
            );
        }
    }

    let names: Vec<String> = sticks
        .iter()
        .map(|s| format!("{} stick", s.name()))
        .chain(triggers.iter().map(|t| format!("{} trigger", t.name())))
        .collect();
    if !yes {
        println!(
            "\nThis recalibrates the {}, which takes about a minute each.\n\n\
             The controller forgets its current calibration as soon as you start, so please \
             finish all the steps. Keep the controls untouched until you are asked to use them.",
            names.join(" and "),
        );
        print!("\nType 'calibrate' to continue: ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if answer.trim() != "calibrate" {
            return Err("cancelled".to_owned());
        }
    }

    let mut live = Live::new();
    // The controls whose check found a problem, with the flag that runs them again.
    let mut problems: Vec<(String, String)> = Vec::new();
    for stick in sticks {
        calibrate_stick(&mut session, stick, &mut live).map_err(|error| {
            format!(
                "{error}\nThe {} stick was not calibrated, and the controller now has no \
                 calibration stored for it. Run `xboxctl calibrate --stick {}` again to finish.",
                stick.name(),
                stick.name()
            )
        })?;
        let outcomes = check_stick(&mut session, stick, &mut live)?;
        if outcomes
            .iter()
            .any(|(_, outcome)| *outcome != Outcome::Pass)
        {
            problems.push((
                format!("{} stick", stick.name()),
                format!("--stick {}", stick.name()),
            ));
        }
    }
    for trigger in triggers {
        calibrate_trigger(&mut session, trigger, &mut live).map_err(|error| {
            format!(
                "{error}\nThe {} trigger was not calibrated, and the controller may now have no \
                 calibration stored for it. Run `xboxctl calibrate --trigger {}` again to finish.",
                trigger.name(),
                trigger.name()
            )
        })?;
        if !check_trigger(&mut session, trigger, &mut live)? {
            problems.push((
                format!("{} trigger", trigger.name()),
                format!("--trigger {}", trigger.name()),
            ));
        }
    }

    println!();
    if problems.is_empty() {
        println!("Done. Your controller is calibrated and the check passed.");
    } else {
        let labels: Vec<&str> = problems.iter().map(|(label, _)| label.as_str()).collect();
        println!(
            "Calibration is done, but the check found a problem with the {}. You can run \
             `xboxctl calibrate {}` again.",
            labels.join(" and "),
            problems[0].1
        );
    }
    Ok(())
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn count(counter: &mut RotationCounter, stick: Stick, packet: &Packet) {
    if let Some(axes) = Axes::parse(packet) {
        let (x, y) = stick.position(&axes);
        counter.feed(x, y);
    }
}

/// Steps 1 and 2: rotate clockwise, then counter-clockwise, until the
/// controller says it is done.
fn calibrate_stick(session: &mut Session, stick: Stick, live: &mut Live) -> Result<(), String> {
    let controls = stick.controls();
    println!("\nCalibrating the {} stick", stick.name());
    session.start(controls)?;

    println!("\nStep 1 of 3: turn the stick clockwise");
    println!("  Push it to the outer edge and draw {ROTATIONS} full circles.");
    live.show(&format!("  Rotations: 0/{ROTATIONS}"));

    let mut counter = RotationCounter::default();
    let mut clockwise_step = true;
    let mut counter_clockwise_from = 0.0;
    let mut seen_active = false;
    let mut last_progress = Instant::now();
    let mut shown = 0;
    let mut finishing = None;

    loop {
        let response = session.read(controls, &mut |p| count(&mut counter, stick, p))?;
        let status = status(&response);
        seen_active |= status != 0;
        if seen_active && status == 0 {
            break;
        }
        if !seen_active && last_progress.elapsed() > Duration::from_secs(5) {
            return Err("the controller did not start the calibration".to_owned());
        }

        let (clockwise, counter_clockwise) = counter.turns();
        let done = if clockwise_step {
            clockwise as u32
        } else {
            (counter_clockwise - counter_clockwise_from).max(0.0) as u32
        }
        .min(ROTATIONS);
        if done != shown {
            shown = done;
            last_progress = Instant::now();
        }
        if status == STATUS_SAVING {
            live.show("  Saving...");
        } else {
            live.show(&format!("  Rotations: {done}/{ROTATIONS}"));
        }
        if clockwise_step && done >= ROTATIONS {
            live.done(&format!("  \u{2713} Rotations: {ROTATIONS}/{ROTATIONS}"));
            println!("\nStep 2 of 3: now turn it counter-clockwise");
            println!("  Again {ROTATIONS} full circles at the outer edge.");
            clockwise_step = false;
            counter_clockwise_from = counter_clockwise;
            shown = 0;
            last_progress = Instant::now();
            live.show(&format!("  Rotations: 0/{ROTATIONS}"));
        } else if !clockwise_step && done >= ROTATIONS {
            finishing.get_or_insert_with(Instant::now);
        }
        if finishing.is_some_and(|since| since.elapsed() > FINISH_LIMIT) {
            return Err("the controller did not finish the calibration".to_owned());
        }
        if last_progress.elapsed() > IDLE_LIMIT {
            return Err("no stick movement for three minutes".to_owned());
        }
        session.pump(Duration::from_millis(300), &mut |p| {
            count(&mut counter, stick, p)
        })?;
    }

    if clockwise_step {
        live.done(&format!("  \u{2713} Rotations: {ROTATIONS}/{ROTATIONS}"));
        println!("\nStep 2 of 3: counter-clockwise");
    }
    live.done(&format!("  \u{2713} Rotations: {ROTATIONS}/{ROTATIONS}"));

    // The controller reports success by going quiet; make sure it kept limits.
    let response = session.read(controls, &mut |_| {})?;
    if describe_limits(stick.limits(&response)).starts_with("no calibration") {
        return Err("the controller finished without storing a calibration".to_owned());
    }
    Ok(())
}

/// Step 3: push the stick in each direction and let go, and judge each push.
fn check_stick(
    session: &mut Session,
    stick: Stick,
    live: &mut Live,
) -> Result<Vec<(Direction, Outcome)>, String> {
    println!("\nStep 3 of 3: check the {} stick", stick.name());
    println!("  Push the stick all the way in each direction, then let go.");
    let mut outcomes = Vec::new();
    for direction in Direction::ALL {
        live.show(&format!(
            "  {}: push the stick all the way {}, then let go",
            direction.label(),
            direction.name()
        ));
        let outcome = wait_for_push(session, stick, direction)?;
        let line = match outcome {
            Outcome::Pass => format!("  \u{2713} {}", direction.label()),
            Outcome::ShortOfEdge => format!(
                "  \u{2717} {}: the stick did not reach the edge",
                direction.label()
            ),
            Outcome::OffCentre => format!(
                "  \u{2717} {}: the stick did not come back to the centre",
                direction.label()
            ),
            Outcome::NoMovement => format!("  - {}: skipped, no movement seen", direction.label()),
        };
        live.done(&line);
        outcomes.push((direction, outcome));
    }
    Ok(outcomes)
}

fn wait_for_push(
    session: &mut Session,
    stick: Stick,
    direction: Direction,
) -> Result<Outcome, String> {
    let mut check = PushCheck::new(direction);
    let began = Instant::now();
    let mut started_at = None;
    let mut last_report = Instant::now();
    loop {
        session.pump(Duration::from_millis(100), &mut |packet| {
            if let Some(axes) = Axes::parse(packet) {
                let (x, y) = stick.position(&axes);
                check.feed(x, y);
                last_report = Instant::now();
            }
        })?;
        if check.started {
            let since = *started_at.get_or_insert_with(Instant::now);
            if (check.released() && last_report.elapsed() >= SETTLE)
                || since.elapsed() >= PUSH_LIMIT
            {
                return Ok(check.outcome());
            }
        } else if began.elapsed() >= PUSH_WAIT {
            return Ok(Outcome::NoMovement);
        }
    }
}

fn press(counter: &mut PressCounter, trigger: Trigger, packet: &Packet) {
    if let Some(triggers) = Triggers::parse(packet) {
        counter.feed(trigger.position(&triggers));
    }
}

/// Step 1: press the trigger all the way in a number of times, until the
/// controller says it is done.
fn calibrate_trigger(
    session: &mut Session,
    trigger: Trigger,
    live: &mut Live,
) -> Result<(), String> {
    let controls = trigger.controls();
    println!("\nCalibrating the {} trigger", trigger.name());
    session.start(controls)?;

    println!("\nStep 1 of 2: press the trigger");
    println!("  Press it all the way in and let go, {PRESSES} times.");
    live.show(&format!("  Presses: 0/{PRESSES}"));

    let mut counter = PressCounter::default();
    let mut seen_active = false;
    let mut last_progress = Instant::now();
    let mut shown = 0;
    let mut finishing = None;

    loop {
        let response = session.read(controls, &mut |p| press(&mut counter, trigger, p))?;
        let status = status(&response);
        seen_active |= status != 0;
        if seen_active && status == 0 {
            break;
        }
        if !seen_active && counter.presses == 0 && last_progress.elapsed() > Duration::from_secs(5)
        {
            return Err("the controller did not start the calibration".to_owned());
        }

        let done = counter.presses.min(PRESSES);
        if done != shown {
            shown = done;
            last_progress = Instant::now();
        }
        if status == STATUS_SAVING {
            live.show("  Saving...");
        } else {
            live.show(&format!("  Presses: {done}/{PRESSES}"));
        }
        if done >= PRESSES {
            finishing.get_or_insert_with(Instant::now);
        }
        if finishing.is_some_and(|since| since.elapsed() > FINISH_LIMIT) {
            return Err(if seen_active {
                "the controller did not finish the calibration".to_owned()
            } else {
                "the controller never reported a calibration in progress".to_owned()
            });
        }
        if last_progress.elapsed() > IDLE_LIMIT {
            return Err("no trigger movement for three minutes".to_owned());
        }
        session.pump(Duration::from_millis(300), &mut |p| {
            press(&mut counter, trigger, p)
        })?;
    }
    live.done(&format!("  \u{2713} Presses: {PRESSES}/{PRESSES}"));

    let response = session.read(controls, &mut |_| {})?;
    match trigger.limits(&response) {
        Some(l) if l.max != 0 => println!("  Stored: {}", describe_trigger_limits(Some(l))),
        _ => println!("  Note: the controller reports no stored calibration."),
    }
    Ok(())
}

/// Step 2: judge the resting position and the range, as the app does, from
/// one full press. Returns whether both are fine.
fn check_trigger(session: &mut Session, trigger: Trigger, live: &mut Live) -> Result<bool, String> {
    println!("\nStep 2 of 2: check the {} trigger", trigger.name());
    live.show("  Leave it alone, then press it all the way in and let go");
    let mut check = TriggerCheck::default();
    let began = Instant::now();
    let mut started_at = None;
    let mut last_report = Instant::now();
    loop {
        session.pump(Duration::from_millis(100), &mut |packet| {
            if let Some(triggers) = Triggers::parse(packet) {
                check.feed(trigger.position(&triggers));
                last_report = Instant::now();
            }
        })?;
        if check.started {
            let since = *started_at.get_or_insert_with(Instant::now);
            if (check.released() && last_report.elapsed() >= SETTLE)
                || since.elapsed() >= PUSH_LIMIT
            {
                break;
            }
        } else if began.elapsed() >= PUSH_WAIT {
            live.done("  - skipped, no movement seen");
            return Ok(false);
        }
    }
    live.done("  Done");
    let mark = |ok| if ok { "\u{2713}" } else { "\u{2717}" };
    println!(
        "  {} Resting: {}",
        mark(check.resting()),
        if check.resting() {
            format!("at rest it reads {} (of {})", check.last_raw, Triggers::MAX)
        } else {
            format!(
                "at rest it reads {} (of {}), not close to zero",
                check.last_raw,
                Triggers::MAX
            )
        }
    );
    println!(
        "  {} Range: {}",
        mark(check.reaches_end()),
        if check.reaches_end() {
            format!(
                "fully pressed it reads {} (of {})",
                check.peak_raw,
                Triggers::MAX
            )
        } else {
            format!(
                "fully pressed it only reads {} (of {})",
                check.peak_raw,
                Triggers::MAX
            )
        }
    );
    Ok(check.resting() && check.reaches_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stick at full deflection moving through `turns` turns; negative is
    /// clockwise (Y is up).
    fn circle(counter: &mut RotationCounter, turns: f64) {
        let steps = (turns.abs() * 120.0) as usize;
        for i in 0..=steps {
            let angle = turns.signum() * TAU * i as f64 / 120.0;
            counter.feed(
                (angle.cos() * 32000.0) as i16,
                (angle.sin() * 32000.0) as i16,
            );
        }
    }

    #[test]
    fn counts_clockwise_and_counter_clockwise_rotations() {
        let mut counter = RotationCounter::default();
        circle(&mut counter, -6.0);
        let (clockwise, counter_clockwise) = counter.turns();
        assert!((5.9..=6.1).contains(&clockwise), "{clockwise}");
        assert!(counter_clockwise < 0.1);
        counter.feed(0, 0);
        circle(&mut counter, 6.0);
        let (clockwise, counter_clockwise) = counter.turns();
        assert!((5.9..=6.1).contains(&clockwise), "{clockwise}");
        assert!(
            (5.9..=6.1).contains(&counter_clockwise),
            "{counter_clockwise}"
        );
    }

    #[test]
    fn a_wobble_near_the_centre_counts_nothing() {
        let mut counter = RotationCounter::default();
        for i in 0..200 {
            let angle = TAU * f64::from(i) / 20.0;
            counter.feed((angle.cos() * 4000.0) as i16, (angle.sin() * 4000.0) as i16);
        }
        assert_eq!(counter.turns(), (0.0, 0.0));
    }

    fn push(direction: Direction, positions: &[(i16, i16)]) -> PushCheck {
        let mut check = PushCheck::new(direction);
        for &(x, y) in positions {
            check.feed(x, y);
        }
        check
    }

    #[test]
    fn a_clean_push_passes() {
        let check = push(Direction::Up, &[(0, 12000), (0, 32767), (0, 9000), (3, 12)]);
        assert!(check.released());
        assert_eq!(check.outcome(), Outcome::Pass);
        // The same for each direction.
        for (direction, edge) in [
            (Direction::Right, (32767, 0)),
            (Direction::Down, (0, -32768)),
            (Direction::Left, (-32768, 0)),
        ] {
            let check = push(direction, &[edge, (0, 0)]);
            assert_eq!(check.outcome(), Outcome::Pass, "{direction:?}");
        }
    }

    #[test]
    fn a_push_that_stops_short_fails() {
        let check = push(Direction::Right, &[(15000, 0), (22000, 100), (0, 0)]);
        assert_eq!(check.outcome(), Outcome::ShortOfEdge);
    }

    #[test]
    fn a_stick_that_does_not_come_back_fails() {
        // Stuck well away from the centre: not judged until it times out.
        let check = push(Direction::Down, &[(0, -32768), (900, -13000)]);
        assert!(!check.released());
        assert_eq!(check.outcome(), Outcome::OffCentre);
        // Rests a little off the centre: judged, and not good enough.
        let check = push(Direction::Down, &[(0, -32768), (0, -2500)]);
        assert!(check.released());
        assert_eq!(check.outcome(), Outcome::OffCentre);
        // A small residual offset is fine.
        let check = push(Direction::Down, &[(0, -32768), (6, -1200)]);
        assert_eq!(check.outcome(), Outcome::Pass);
    }

    #[test]
    fn a_push_in_another_direction_does_not_count() {
        let check = push(Direction::Up, &[(0, -32768), (0, 0)]);
        assert!(!check.started);
        assert_eq!(check.outcome(), Outcome::NoMovement);
    }

    #[test]
    fn limits_are_described_for_people() {
        let limits = Limits {
            horizontal_min: 900,
            horizontal_max: 3150,
            vertical_min: 910,
            vertical_max: 3270,
        };
        assert_eq!(
            describe_limits(Some(limits)),
            "horizontal 900-3150, vertical 910-3270"
        );
        let empty = Limits {
            horizontal_min: 0,
            horizontal_max: 0,
            vertical_min: 0,
            vertical_max: 0,
        };
        assert!(describe_limits(Some(empty)).starts_with("no calibration"));
        assert!(describe_limits(None).starts_with("no calibration"));
    }

    #[test]
    fn trigger_limits_are_described_for_people() {
        let limits = TriggerLimits {
            flag: 2,
            min: 1134,
            max: 2527,
        };
        assert_eq!(describe_trigger_limits(Some(limits)), "range 1134-2527");
        let empty = TriggerLimits {
            flag: 2,
            min: 0,
            max: 0,
        };
        assert!(describe_trigger_limits(Some(empty)).starts_with("no calibration"));
        assert!(describe_trigger_limits(None).starts_with("no calibration"));
    }

    #[test]
    fn counts_trigger_presses() {
        let mut counter = PressCounter::default();
        for _ in 0..3 {
            for value in [0, 300, 1023, 600, 40] {
                counter.feed(value);
            }
        }
        assert_eq!(counter.presses, 3);
        // A half press, or holding it down, is not a press.
        counter.feed(500);
        counter.feed(0);
        counter.feed(1023);
        assert_eq!(counter.presses, 3);
    }

    fn trigger_check(values: &[u16]) -> TriggerCheck {
        let mut check = TriggerCheck::default();
        for &value in values {
            check.feed(value);
        }
        check
    }

    #[test]
    fn a_clean_trigger_press_passes() {
        let check = trigger_check(&[0, 400, 1000, 1023, 300, 0]);
        assert!(check.released());
        assert!(check.resting());
        assert!(check.reaches_end());
    }

    #[test]
    fn a_trigger_that_does_not_reach_the_end_fails_the_range() {
        let check = trigger_check(&[0, 600, 800, 0]);
        assert!(check.resting());
        assert!(!check.reaches_end());
    }

    #[test]
    fn a_trigger_that_rests_above_zero_fails_the_resting_check() {
        let check = trigger_check(&[0, 1023, 120]);
        assert!(!check.resting());
        assert!(check.reaches_end());
        // The first reading of a press is part of the ramp, not the rest.
        assert!(trigger_check(&[90, 400, 1023, 0]).resting());
    }
}
