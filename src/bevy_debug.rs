//! Optional Bevy 0.18 built-in UI for the client packet conditioner.
//!
//! Add [`ConditionerDebugPlugin::new`] with the same handle attached to your
//! transport. Your app supplies Bevy's UI/render plugins and a UI camera. After
//! updating Renet, call [`ConditionerDebug::report_rtt`] with its transport RTT
//! (`Duration::from_secs_f64(client.rtt())`), or `None` while disconnected.
//! This is transport RTT, never gameplay input acknowledgement age.

use crate::conditioner::{ConditionerHandle, RttCalibration};
use bevy::prelude::*;
use std::time::Duration;

/// Reusable UI controls and telemetry. The transport owns the packet queues.
#[derive(Resource)]
pub struct ConditionerDebug {
    handle: ConditionerHandle,
    calibration: RttCalibration,
    observed_rtt: Option<Duration>,
    target_rtt: Option<Duration>,
    status: String,
    elapsed: Duration,
    /// Hide the panel without changing network conditioning.
    pub visible: bool,
}

impl ConditionerDebug {
    pub fn new(handle: ConditionerHandle) -> Self {
        Self {
            handle,
            calibration: RttCalibration::default(),
            observed_rtt: None,
            target_rtt: None,
            status: String::new(),
            elapsed: Duration::ZERO,
            visible: true,
        }
    }

    pub fn handle(&self) -> &ConditionerHandle {
        &self.handle
    }

    /// Report once per transport update. Pass `None` on disconnect, including
    /// before replacing the transport, so the old session baseline is discarded.
    pub fn report_rtt(&mut self, rtt: Option<Duration>) {
        if rtt.is_none() {
            self.calibration.reset();
            self.observed_rtt = None;
            return;
        }
        self.observed_rtt = rtt;
        if let Some(rtt) = rtt
            && !rtt.is_zero()
        {
            self.calibration
                .observe_at(self.elapsed, rtt, self.handle.is_active());
        }
    }

    pub fn baseline_rtt(&self) -> Option<Duration> {
        self.calibration.baseline()
    }
    pub fn observed_rtt(&self) -> Option<Duration> {
        self.observed_rtt
    }

    /// Configure an approximate total RTT using an unconditioned baseline.
    /// Returns false until a baseline is available; does not invent a baseline.
    pub fn target_rtt(&mut self, target: Duration) -> bool {
        let Some(delay) = self.calibration.added_delay_for_target(target) else {
            self.status = "Measure baseline with Off first.".into();
            return false;
        };
        let mut config = self.handle.config();
        config.enabled = true;
        config.latency = delay;
        if let Err(error) = self.handle.configure(config) {
            self.status = error.to_string();
            return false;
        }
        self.target_rtt = Some(target);
        self.status.clear();
        true
    }

    pub fn disable(&mut self) {
        if self.handle.is_active() {
            self.calibration
                .observe_at(self.elapsed, Duration::ZERO, true);
        }
        let mut config = self.handle.config();
        config.enabled = false;
        if let Err(error) = self.handle.configure(config) {
            self.status = error.to_string();
        }
        self.target_rtt = None;
    }

    /// Turn conditioning off and discard the old estimate. Calibration waits for
    /// unconditioned RTT to settle; observe fresh connected samples afterward.
    pub fn recalibrate(&mut self) {
        self.disable();
        self.calibration.reset();
        self.status = "Recalibrating with impairment off…".into();
    }
}

/// Adds an optional panel using only Bevy Node/Text/Button components.
/// Does not install DefaultPlugins or spawn/change your cameras.
pub struct ConditionerDebugPlugin {
    handle: ConditionerHandle,
}
impl ConditionerDebugPlugin {
    pub fn new(handle: ConditionerHandle) -> Self {
        Self { handle }
    }
}
impl Plugin for ConditionerDebugPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ConditionerDebug::new(self.handle.clone()))
            .add_systems(Startup, spawn_panel)
            .add_systems(PreUpdate, advance_clock)
            .add_systems(Update, (controls, refresh_panel).chain());
    }
}

fn advance_clock(time: Res<Time<Real>>, mut state: ResMut<ConditionerDebug>) {
    state.elapsed = time.elapsed();
}

#[derive(Component)]
struct Panel;
#[derive(Component)]
struct Readout;
#[derive(Component, Clone, Copy)]
enum Action {
    Off,
    Target150,
    Target300,
    DelayDown,
    DelayUp,
    JitterDown,
    JitterUp,
    LossDown,
    LossUp,
    Outage,
    Calibrate,
}

fn spawn_panel(mut commands: Commands) {
    commands.spawn((Panel, Node {
        position_type: PositionType::Absolute, right: Val::Px(12.0), top: Val::Px(12.0),
        width: Val::Px(350.0), max_width: Val::Percent(95.0),
        flex_direction: FlexDirection::Column, row_gap: Val::Px(6.0),
        padding: UiRect::all(Val::Px(12.0)), ..default()
    }, BackgroundColor(Color::srgba(0.035,0.045,0.065,0.96)), GlobalZIndex(1000)))
    .with_children(|parent| {
        parent.spawn((Text::new("Network conditioner"), TextFont { font_size: 18.0, ..default() }));
        parent.spawn((Readout, Text::new("Waiting for transport RTT…"), TextFont { font_size: 13.0, ..default() }));
        for row in [
            vec![("Off",Action::Off),("~150 ms RTT",Action::Target150),("~300 ms RTT",Action::Target300)],
            vec![("Delay −10 ms",Action::DelayDown),("Delay +10 ms",Action::DelayUp)],
            vec![("Jitter −5 ms",Action::JitterDown),("Jitter +5 ms",Action::JitterUp)],
            vec![("Loss −1%",Action::LossDown),("Loss +1%",Action::LossUp)],
            vec![("Outage 1 s",Action::Outage),("Recalibrate baseline",Action::Calibrate)],
        ] {
            parent.spawn(Node { column_gap: Val::Px(5.0), flex_wrap: FlexWrap::Wrap, ..default() })
                .with_children(|buttons| {
                    for (label, action) in row {
                        buttons.spawn((Button, action, Node { padding: UiRect::all(Val::Px(6.0)), ..default() },
                            BackgroundColor(Color::srgb(0.13,0.18,0.25))))
                            .with_children(|button| { button.spawn((Text::new(label), TextFont { font_size: 13.0, ..default() })); });
                    }
                });
        }
        parent.spawn((Text::new("Delay is added EACH way. Targets are approximate; polling and jitter affect RTT. Browser ICE/DTLS/SCTP establishment is not conditioned."), TextFont { font_size: 11.0, ..default() }, TextColor(Color::srgb(0.65,0.72,0.82))));
    });
}

type Buttons<'w, 's> = Query<
    'w,
    's,
    (
        &'static Interaction,
        &'static Action,
        &'static mut BackgroundColor,
    ),
    (Changed<Interaction>, With<Button>),
>;
fn controls(mut state: ResMut<ConditionerDebug>, mut buttons: Buttons) {
    for (interaction, action, mut color) in &mut buttons {
        *color = BackgroundColor(match interaction {
            Interaction::Pressed => Color::srgb(0.25, 0.42, 0.58),
            Interaction::Hovered => Color::srgb(0.19, 0.28, 0.38),
            Interaction::None => Color::srgb(0.13, 0.18, 0.25),
        });
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            Action::Off => state.disable(),
            Action::Target150 => {
                state.target_rtt(Duration::from_millis(150));
            }
            Action::Target300 => {
                state.target_rtt(Duration::from_millis(300));
            }
            Action::Calibrate => state.recalibrate(),
            Action::Outage => state.handle.outage(Duration::from_secs(1)),
            action => {
                let mut config = state.handle.config();
                config.enabled = true;
                match action {
                    Action::DelayDown => {
                        config.latency = config.latency.saturating_sub(Duration::from_millis(10))
                    }
                    Action::DelayUp => {
                        config.latency =
                            (config.latency + Duration::from_millis(10)).min(Duration::from_secs(5))
                    }
                    Action::JitterDown => {
                        config.jitter = config.jitter.saturating_sub(Duration::from_millis(5))
                    }
                    Action::JitterUp => {
                        config.jitter =
                            (config.jitter + Duration::from_millis(5)).min(Duration::from_secs(5))
                    }
                    Action::LossDown => config.packet_loss = (config.packet_loss - 0.01).max(0.0),
                    Action::LossUp => config.packet_loss = (config.packet_loss + 0.01).min(1.0),
                    _ => unreachable!(),
                }
                state.target_rtt = None;
                state.status = state
                    .handle
                    .configure(config)
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_default();
            }
        }
    }
}

fn millis(value: Option<Duration>) -> String {
    value
        .map(|value| format!("{:.1} ms", value.as_secs_f64() * 1000.0))
        .unwrap_or_else(|| "measuring…".into())
}
fn refresh_panel(
    state: Res<ConditionerDebug>,
    mut panels: Query<&mut Node, With<Panel>>,
    mut texts: Query<&mut Text, With<Readout>>,
) {
    for mut panel in &mut panels {
        panel.display = if state.visible {
            Display::Flex
        } else {
            Display::None
        };
    }
    if !state.visible {
        return;
    }
    let config = state.handle.config();
    let stats = state.handle.stats();
    let target_note = if state
        .target_rtt
        .zip(state.baseline_rtt())
        .is_some_and(|(target, baseline)| target < baseline)
    {
        "Target below baseline: no added delay.\n"
    } else {
        ""
    };
    let line = format!(
        "{}{}\nBaseline: {} | Renet RTT: {}\nAdded delay/way: {:.0} ms | jitter: ±{:.0} ms\nLoss: {:.0}% | target: {}\nQueues in/out: {}/{} packets, {}/{} bytes\nDrops in/out — loss: {}/{}, outage: {}/{}\nOverflow: {}/{} | transition: {}/{}\n{}{}",
        if config.enabled { "ON" } else { "OFF" },
        if stats.outage_active {
            " • OUTAGE"
        } else {
            ""
        },
        millis(state.baseline_rtt()),
        millis(state.observed_rtt()),
        config.latency.as_secs_f64() * 1000.0,
        config.jitter.as_secs_f64() * 1000.0,
        config.packet_loss * 100.0,
        state
            .target_rtt
            .map(|v| millis(Some(v)))
            .unwrap_or_else(|| "custom/off".into()),
        stats.incoming.queued_packets,
        stats.outgoing.queued_packets,
        stats.incoming.queued_bytes,
        stats.outgoing.queued_bytes,
        stats.incoming.simulated_loss_drops,
        stats.outgoing.simulated_loss_drops,
        stats.incoming.outage_drops,
        stats.outgoing.outage_drops,
        stats.incoming.overflow_drops,
        stats.outgoing.overflow_drops,
        stats.incoming.transition_drops,
        stats.outgoing.transition_drops,
        target_note,
        state.status
    );
    for mut text in &mut texts {
        if text.0 != line {
            text.0.clone_from(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_requires_baseline_and_freezes_it_under_conditioning() {
        let mut state = ConditionerDebug::new(ConditionerHandle::default());
        assert!(!state.target_rtt(Duration::from_millis(300)));
        assert!(!state.handle.config().enabled);
        state.report_rtt(Some(Duration::from_millis(40)));
        assert!(state.target_rtt(Duration::from_millis(300)));
        assert_eq!(state.handle.config().latency, Duration::from_millis(130));
        state.report_rtt(Some(Duration::from_millis(300)));
        assert_eq!(state.baseline_rtt(), Some(Duration::from_millis(40)));
        assert!(state.target_rtt(Duration::from_millis(20)));
        assert_eq!(state.handle.config().latency, Duration::ZERO);
    }

    #[test]
    fn off_and_recalibrate_do_not_sample_conditioned_ema_immediately() {
        let mut state = ConditionerDebug::new(ConditionerHandle::default());
        state.report_rtt(Some(Duration::from_millis(40)));
        state.target_rtt(Duration::from_millis(300));
        state.recalibrate();
        assert!(!state.handle.config().enabled);
        state.report_rtt(Some(Duration::from_millis(300)));
        assert_eq!(state.baseline_rtt(), None);
        state.elapsed = Duration::from_secs(6);
        state.report_rtt(Some(Duration::from_millis(40)));
        assert_eq!(state.baseline_rtt(), Some(Duration::from_millis(40)));
        state.report_rtt(None);
        assert_eq!(state.baseline_rtt(), None);
        assert_eq!(state.observed_rtt(), None);
    }

    #[test]
    fn built_in_button_drives_shared_handle_without_renderer() {
        let handle = ConditionerHandle::default();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(ConditionerDebugPlugin::new(handle.clone()));
        app.update();
        app.world_mut()
            .resource_mut::<ConditionerDebug>()
            .report_rtt(Some(Duration::from_millis(40)));
        let button = app
            .world_mut()
            .query::<(Entity, &Action)>()
            .iter(app.world())
            .find_map(|(entity, action)| matches!(action, Action::Target300).then_some(entity))
            .unwrap();
        app.world_mut()
            .entity_mut(button)
            .insert(Interaction::Pressed);
        app.update();
        assert!(handle.config().enabled);
        assert_eq!(handle.config().latency, Duration::from_millis(130));
    }
}
