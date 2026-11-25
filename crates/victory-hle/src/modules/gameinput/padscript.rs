//! A scripted gamepad for unattended runs and agents: `VICTORY_PAD_FILE=<path>`.
//!
//! While the variable is set, host slot 0 holds a virtual pad that stays
//! connected for the whole run (the keyboard's stand-in pad is folded into it). Whatever is written to the file is taken
//! (the file is renamed away first, so a line appended meanwhile starts a new
//! file and is not lost), parsed into steps and played in order. A real pad in
//! slot 0, and the keyboard's stand-in pad, still add their buttons and axes.
//!
//! One step per line (or several separated by `;`):
//!
//! ```text
//! A                      tap A (held 150 ms)
//! A @500                 hold A for 500 ms
//! LB+RB @300             hold both shoulders
//! LSTICK=0,1 @2000       left stick fully up for 2 s
//! LSTICK=0,1+RT=1 @3000  stick and right trigger together
//! WAIT @1000             nothing for 1 s
//! CLEAR                  drop every step not yet played
//! # comment
//! ```
//!
//! Buttons: A B X Y MENU (START) VIEW (BACK) UP DOWN LEFT RIGHT LB RB LS RS.
//! Axes: LSTICK=x,y RSTICK=x,y (-1..1, up positive), LT=v RT=v (0..1).
//! Every step is followed by 100 ms of a neutral pad, so repeated taps are
//! seen as separate presses. Unknown words are logged and skipped.
//!
//! Progress is written to `<path>.status` after every step: `done=<n>
//! queued=<n> playing=<step or ->`, so a script can wait for its steps.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{
    A, B, DOWN, KeyState, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PADS, PadState, RIGHT,
    RIGHT_SHOULDER, RIGHT_STICK, Source, UP, VIEW, X, Y,
};

/// The environment variable that turns the scripted pad on.
pub(super) const ENV: &str = "VICTORY_PAD_FILE";

const TAP: Duration = Duration::from_millis(150);
const GAP: Duration = Duration::from_millis(100);
/// How often the file is looked at.
const LOOK: Duration = Duration::from_millis(50);

/// One step: a pad state held for a time.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Step {
    pub(super) pad: PadState,
    pub(super) hold: Duration,
    /// The text it came from, for the status file.
    pub(super) text: String,
}

/// A line of the file: steps to queue, or an order to drop the queue.
#[derive(Debug, PartialEq)]
pub(super) enum Command {
    Step(Step),
    Clear,
}

fn button(word: &str) -> Option<u32> {
    Some(match word {
        "A" => A,
        "B" => B,
        "X" => X,
        "Y" => Y,
        "MENU" | "START" => MENU,
        "VIEW" | "BACK" => VIEW,
        "UP" => UP,
        "DOWN" => DOWN,
        "LEFT" => LEFT,
        "RIGHT" => RIGHT,
        "LB" => LEFT_SHOULDER,
        "RB" => RIGHT_SHOULDER,
        "LS" => LEFT_STICK,
        "RS" => RIGHT_STICK,
        _ => return None,
    })
}

fn axis(text: &str, low: f32) -> Result<f32, String> {
    let v: f32 = text
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {text}"))?;
    if !v.is_finite() {
        return Err(format!("not a number: {text}"));
    }
    Ok(v.clamp(low, 1.0))
}

fn pair(text: &str) -> Result<(f32, f32), String> {
    let (x, y) = text
        .split_once(',')
        .ok_or_else(|| format!("expected x,y: {text}"))?;
    Ok((axis(x, -1.0)?, axis(y, -1.0)?))
}

/// Parses one step (`parts[@ms]`).
pub(super) fn parse_step(text: &str) -> Result<Command, String> {
    let text = text.trim();
    let (body, hold) = match text.split_once('@') {
        Some((body, ms)) => {
            let ms: u64 = ms
                .trim()
                .parse()
                .map_err(|_| format!("bad hold time: {ms}"))?;
            (body.trim(), Some(Duration::from_millis(ms.min(600_000))))
        }
        None => (text, None),
    };
    let upper = body.to_ascii_uppercase();
    if upper == "CLEAR" {
        return Ok(Command::Clear);
    }
    let mut pad = PadState::default();
    let mut waits = false;
    for part in upper.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            None if part == "WAIT" => waits = true,
            None => pad.buttons |= button(part).ok_or_else(|| format!("unknown button: {part}"))?,
            Some((name, value)) => match name.trim() {
                "LSTICK" => (pad.left_x, pad.left_y) = pair(value)?,
                "RSTICK" => (pad.right_x, pad.right_y) = pair(value)?,
                "LT" => pad.left_trigger = axis(value, 0.0)?,
                "RT" => pad.right_trigger = axis(value, 0.0)?,
                other => return Err(format!("unknown axis: {other}")),
            },
        }
    }
    if waits && pad != PadState::default() {
        return Err("WAIT takes no buttons".to_owned());
    }
    if !waits && pad == PadState::default() {
        return Err(format!("nothing to press: {text}"));
    }
    Ok(Command::Step(Step {
        pad,
        hold: hold.unwrap_or(TAP),
        text: text.to_owned(),
    }))
}

/// Parses a whole file's text; bad steps are returned as errors, in order.
pub(super) fn parse(text: &str) -> Vec<Result<Command, String>> {
    text.lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .map(parse_step)
        .collect()
}

/// The queue and what is playing; driven by time so it can be tested.
#[derive(Default)]
pub(super) struct Player {
    queue: std::collections::VecDeque<Step>,
    /// The step playing and when it started.
    current: Option<(Step, Instant)>,
    pub(super) done: u64,
}

impl Player {
    pub(super) fn add(&mut self, commands: Vec<Command>) {
        for c in commands {
            match c {
                Command::Clear => {
                    self.queue.clear();
                    self.current = None;
                }
                Command::Step(s) => self.queue.push_back(s),
            }
        }
    }

    /// The pad at `now`; `true` with it when a step ended or began.
    pub(super) fn pad(&mut self, now: Instant) -> (PadState, bool) {
        let mut changed = false;
        loop {
            if let Some((step, start)) = &self.current {
                let elapsed = now.saturating_duration_since(*start);
                if elapsed < step.hold {
                    return (step.pad, changed);
                }
                if elapsed < step.hold + GAP {
                    return (PadState::default(), changed);
                }
                self.current = None;
                self.done += 1;
                changed = true;
            }
            match self.queue.pop_front() {
                Some(next) => {
                    self.current = Some((next, now));
                    changed = true;
                }
                None => return (PadState::default(), changed),
            }
        }
    }

    pub(super) fn status(&self) -> String {
        let playing = self.current.as_ref().map_or("-", |(s, _)| s.text.as_str());
        format!(
            "done={} queued={} playing={}\n",
            self.done,
            self.queue.len(),
            playing
        )
    }
}

/// Two pads at once: buttons combined, the axis further from rest wins.
pub(super) fn merge(a: PadState, b: PadState) -> PadState {
    let pick = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: pick(a.left_trigger, b.left_trigger),
        right_trigger: pick(a.right_trigger, b.right_trigger),
        left_x: pick(a.left_x, b.left_x),
        left_y: pick(a.left_y, b.left_y),
        right_x: pick(a.right_x, b.right_x),
        right_y: pick(a.right_y, b.right_y),
    }
}

/// Takes the file's text: renamed away first, read, then removed.
fn take(path: &Path) -> Option<String> {
    let taking = path.with_extension("taking");
    std::fs::rename(path, &taking).ok()?;
    let text = std::fs::read_to_string(&taking).ok();
    let _ = std::fs::remove_file(&taking);
    text
}

/// The host source with the scripted pad in slot 0.
pub(super) struct Scripted {
    inner: Box<dyn Source>,
    path: PathBuf,
    status: PathBuf,
    player: Player,
    looked: Option<Instant>,
}

impl Scripted {
    pub(super) fn new(inner: Box<dyn Source>, path: PathBuf) -> Self {
        let mut status = path.clone().into_os_string();
        status.push(".status");
        let me = Self {
            inner,
            path,
            status: status.into(),
            player: Player::default(),
            looked: None,
        };
        tracing::info!(file = %me.path.display(), "scripted pad in slot 0 ({ENV})");
        me.write_status();
        me
    }

    fn write_status(&self) {
        let _ = std::fs::write(&self.status, self.player.status());
    }

    fn look(&mut self, now: Instant) {
        if self
            .looked
            .is_some_and(|t| now.saturating_duration_since(t) < LOOK)
        {
            return;
        }
        self.looked = Some(now);
        let Some(text) = take(&self.path) else { return };
        let mut commands = Vec::new();
        for parsed in parse(&text) {
            match parsed {
                Ok(c) => commands.push(c),
                Err(e) => tracing::warn!(error = %e, "scripted pad: step skipped"),
            }
        }
        tracing::info!(steps = commands.len(), "scripted pad: steps taken");
        self.player.add(commands);
        self.write_status();
    }
}

impl Source for Scripted {
    fn pad(&mut self, slot: usize) -> Option<PadState> {
        let host = self.inner.pad(slot);
        if slot != 0 {
            return host;
        }
        let now = Instant::now();
        self.look(now);
        let (scripted, changed) = self.player.pad(now);
        if changed {
            self.write_status();
        }
        // The keyboard's stand-in pad goes away while a pad is connected,
        // and this one always is: its keys are added here instead.
        let keyboard = super::fallback(&self.inner.keys());
        Some(merge(merge(scripted, host.unwrap_or_default()), keyboard))
    }

    fn keys(&mut self) -> Vec<KeyState> {
        self.inner.keys()
    }

    fn rumble(&mut self, slot: usize, low: u16, high: u16) {
        self.inner.rumble(slot, low, high);
    }
}

const _: () = assert!(PADS >= 1);

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str) -> Step {
        match parse_step(text).unwrap() {
            Command::Step(s) => s,
            Command::Clear => panic!("clear"),
        }
    }

    #[test]
    fn steps_parse() {
        assert_eq!(step("a").pad.buttons, A);
        assert_eq!(step("A").hold, TAP);
        assert_eq!(step("A @500").hold, Duration::from_millis(500));
        assert_eq!(
            step("lb+rb @300").pad.buttons,
            LEFT_SHOULDER | RIGHT_SHOULDER
        );
        let s = step("LSTICK=0,1+RT=1 @3000");
        assert_eq!(
            (s.pad.left_x, s.pad.left_y, s.pad.right_trigger),
            (0.0, 1.0, 1.0)
        );
        assert_eq!(step("RSTICK=-2, 0.5").pad.right_x, -1.0);
        assert_eq!(step("LT=-1+A").pad.left_trigger, 0.0);
        assert!(parse_step("LT=-1").is_err()); // clamped to rest: nothing to press
        assert_eq!(step("WAIT @1000").pad, PadState::default());
        assert_eq!(parse_step("clear"), Ok(Command::Clear));
        assert!(parse_step("JUMP").is_err());
        assert!(parse_step("WAIT+A").is_err());
        assert!(parse_step("LSTICK=1").is_err());
        assert!(parse_step("A @soon").is_err());
        assert!(parse_step("LT=nan").is_err());
    }

    #[test]
    fn files_split_into_steps() {
        let parsed = parse("# boot\nA\n\nDOWN; DOWN ;A @200\nJUMP\n");
        assert_eq!(parsed.len(), 5);
        assert!(parsed[4].is_err());
    }

    #[test]
    fn steps_play_in_order_with_gaps() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = Player::default();
        p.add(vec![
            Command::Step(step("A")),
            Command::Step(step("B @300")),
        ]);
        assert_eq!(p.pad(at(0)).0.buttons, A);
        assert_eq!(p.pad(at(149)).0.buttons, A);
        assert_eq!(p.pad(at(160)).0.buttons, 0); // gap
        assert_eq!(p.pad(at(250)).0.buttons, B); // B starts as the gap ends
        assert_eq!(p.done, 1);
        assert_eq!(p.pad(at(540)).0.buttons, B);
        assert_eq!(p.pad(at(560)).0.buttons, 0);
        assert_eq!(p.pad(at(700)).0, PadState::default());
        assert_eq!(p.done, 2);
        assert_eq!(p.status(), "done=2 queued=0 playing=-\n");
    }

    #[test]
    fn clear_drops_the_queue() {
        let t0 = Instant::now();
        let mut p = Player::default();
        p.add(vec![
            Command::Step(step("A @1000")),
            Command::Step(step("B")),
        ]);
        p.pad(t0);
        p.add(vec![Command::Clear]);
        assert_eq!(p.pad(t0 + Duration::from_millis(10)).0, PadState::default());
        assert_eq!(p.status(), "done=0 queued=0 playing=-\n");
    }

    #[test]
    fn pads_merge() {
        let a = PadState {
            buttons: A,
            left_x: 0.2,
            ..Default::default()
        };
        let b = PadState {
            buttons: B,
            left_x: -0.9,
            right_trigger: 1.0,
            ..Default::default()
        };
        let m = merge(a, b);
        assert_eq!((m.buttons, m.left_x, m.right_trigger), (A | B, -0.9, 1.0));
    }

    #[test]
    fn the_file_is_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pad.txt");
        std::fs::write(&path, "A\n").unwrap();
        assert_eq!(take(&path).as_deref(), Some("A\n"));
        assert!(!path.exists());
        assert_eq!(take(&path), None);
    }
}
