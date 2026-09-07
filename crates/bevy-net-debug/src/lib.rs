//! Optional Bevy 0.18 built-in UI for the client packet conditioner.
//!
//! Add [`ConditionerDebugPlugin::new`] with the same handle attached to your
//! transport. Your app supplies Bevy's UI/render plugins and a UI camera. After
//! updating Renet, call [`ConditionerDebug::report_rtt`] with its transport RTT
//! (`Duration::from_secs_f64(client.rtt())`), or `None` while disconnected.
//! This is transport RTT, never gameplay input acknowledgement age.

use bevy::{
    input::mouse::{MouseScrollUnit, MouseWheel},
    prelude::*,
};
use renet_cross::conditioner::{ConditionerHandle, RttCalibration};
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
        self.status = "Recalibrating with impairment off...".into();
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
            .add_message::<MouseWheel>()
            .add_systems(Startup, spawn_panel)
            .add_systems(PreUpdate, advance_clock)
            .add_systems(Update, (controls, refresh_panel, scroll_panel).chain());
    }
}

fn advance_clock(time: Res<Time<Real>>, mut state: ResMut<ConditionerDebug>) {
    state.elapsed = time.elapsed();
}

#[derive(Component)]
struct Panel;
#[derive(Component, Clone, Copy)]
enum Metric {
    Baseline,
    Observed,
    Target,
    Delay,
    Jitter,
    Loss,
    Status,
    Note,
    QueuePackets(bool),
    QueueBytes(bool),
    LossDrops(bool),
    OutageDrops(bool),
    OverflowDrops(bool),
    TransitionDrops(bool),
}
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

const PANEL_HELP: &str = "Delay is added EACH way. Targets are approximate; polling and jitter affect RTT. Browser ICE/DTLS/SCTP establishment is not conditioned.";

fn spawn_panel(mut commands: Commands) {
    commands
        .spawn((
            Panel,
            Interaction::default(),
            Node {
                flex_shrink: 0.0,
                position_type: PositionType::Absolute,
                right: Val::Px(12.0),
                top: Val::Px(12.0),
                width: Val::Px(620.0),
                max_width: Val::Percent(95.0),
                max_height: Val::Vh(94.0),
                overflow: Overflow::scroll_y(),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(10.0),
                padding: UiRect::all(Val::Px(12.0)),
                ..default()
            },
            BackgroundColor(Color::srgb(0.035, 0.045, 0.065)),
            GlobalZIndex(1000),
        ))
        .with_children(|parent| {
            parent
                .spawn(Node {
                    flex_shrink: 0.0,
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: AlignItems::Center,
                    flex_wrap: FlexWrap::Wrap,
                    ..default()
                })
                .with_children(|header| {
                    header.spawn((
                        Text::new("Network conditioner"),
                        TextFont {
                            font_size: 18.0,
                            ..default()
                        },
                    ));
                    header.spawn((
                        Metric::Status,
                        Text::new("OFF"),
                        TextFont {
                            font_size: 13.0,
                            ..default()
                        },
                        TextColor(Color::srgb(0.55, 0.85, 0.95)),
                    ));
                });
            parent
                .spawn(Node {
                    flex_shrink: 0.0,
                    flex_wrap: FlexWrap::Wrap,
                    column_gap: Val::Px(6.0),
                    row_gap: Val::Px(6.0),
                    ..default()
                })
                .with_children(|cards| {
                    for (label, metric) in [
                        ("Baseline RTT", Metric::Baseline),
                        ("Observed Renet RTT", Metric::Observed),
                        ("Target RTT", Metric::Target),
                        ("Added delay / way", Metric::Delay),
                        ("Jitter / way", Metric::Jitter),
                        ("Packet loss", Metric::Loss),
                    ] {
                        cards
                            .spawn((
                                Node {
                                    flex_shrink: 0.0,
                                    flex_basis: Val::Px(165.0),
                                    flex_grow: 1.0,
                                    min_width: Val::Px(0.0),
                                    flex_direction: FlexDirection::Column,
                                    row_gap: Val::Px(4.0),
                                    padding: UiRect::all(Val::Px(8.0)),
                                    ..default()
                                },
                                BackgroundColor(Color::srgb(0.075, 0.095, 0.13)),
                            ))
                            .with_children(|card| {
                                card.spawn((
                                    Text::new(label),
                                    TextFont {
                                        font_size: 11.0,
                                        ..default()
                                    },
                                    TextColor(Color::srgb(0.65, 0.72, 0.82)),
                                ));
                                card.spawn((
                                    metric,
                                    Text::new("--"),
                                    TextFont {
                                        font_size: 17.0,
                                        ..default()
                                    },
                                ));
                            });
                    }
                });
            parent.spawn((
                Text::new("Queues and drops"),
                TextFont {
                    font_size: 13.0,
                    ..default()
                },
            ));
            // A single grid keeps all columns aligned;
            // Cells can wrap at narrow widths.
            parent
                .spawn(Node {
                    flex_shrink: 0.0,
                    display: Display::Grid,
                    width: Val::Percent(100.0),
                    grid_template_columns: vec![
                        GridTrack::fr(1.5),
                        GridTrack::fr(1.0),
                        GridTrack::fr(1.0),
                        GridTrack::fr(1.0),
                        GridTrack::fr(1.0),
                        GridTrack::fr(1.2),
                        GridTrack::fr(1.2),
                    ],
                    row_gap: Val::Px(6.0),
                    column_gap: Val::Px(4.0),
                    ..default()
                })
                .with_children(|table| {
                    for label in [
                        "Direction",
                        "Packets",
                        "Bytes",
                        "Loss",
                        "Outage",
                        "Overflow",
                        "Transition",
                    ] {
                        table.spawn((
                            Text::new(label),
                            TextFont {
                                font_size: 11.0,
                                ..default()
                            },
                            Node {
                                flex_shrink: 0.0,
                                min_width: Val::Px(0.0),
                                ..default()
                            },
                            TextColor(Color::srgb(0.65, 0.72, 0.82)),
                        ));
                    }
                    for (label, incoming) in [("Incoming", true), ("Outgoing", false)] {
                        table.spawn((
                            Text::new(label),
                            TextFont {
                                font_size: 11.0,
                                ..default()
                            },
                            Node {
                                flex_shrink: 0.0,
                                min_width: Val::Px(0.0),
                                ..default()
                            },
                        ));
                        for metric in [
                            Metric::QueuePackets(incoming),
                            Metric::QueueBytes(incoming),
                            Metric::LossDrops(incoming),
                            Metric::OutageDrops(incoming),
                            Metric::OverflowDrops(incoming),
                            Metric::TransitionDrops(incoming),
                        ] {
                            table.spawn((
                                metric,
                                Text::new("0"),
                                TextFont {
                                    font_size: 11.0,
                                    ..default()
                                },
                                Node {
                                    flex_shrink: 0.0,
                                    min_width: Val::Px(0.0),
                                    ..default()
                                },
                            ));
                        }
                    }
                });
            parent.spawn((
                Metric::Note,
                Text::new(""),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(Color::srgb(0.95, 0.78, 0.45)),
            ));
            for row in [
                vec![
                    ("Off", Action::Off),
                    ("~150 ms RTT", Action::Target150),
                    ("~300 ms RTT", Action::Target300),
                ],
                vec![
                    ("Delay -10 ms", Action::DelayDown),
                    ("Delay +10 ms", Action::DelayUp),
                    ("Jitter -5 ms", Action::JitterDown),
                    ("Jitter +5 ms", Action::JitterUp),
                ],
                vec![
                    ("Loss -1%", Action::LossDown),
                    ("Loss +1%", Action::LossUp),
                    ("Outage 1 s", Action::Outage),
                    ("Recalibrate baseline", Action::Calibrate),
                ],
            ] {
                parent
                    .spawn(Node {
                        flex_shrink: 0.0,
                        column_gap: Val::Px(5.0),
                        row_gap: Val::Px(5.0),
                        flex_wrap: FlexWrap::Wrap,
                        ..default()
                    })
                    .with_children(|buttons| {
                        for (label, action) in row {
                            buttons
                                .spawn((
                                    Button,
                                    action,
                                    Node {
                                        flex_shrink: 0.0,
                                        padding: UiRect::all(Val::Px(6.0)),
                                        ..default()
                                    },
                                    BackgroundColor(Color::srgb(0.13, 0.18, 0.25)),
                                ))
                                .with_children(|button| {
                                    button.spawn((
                                        Text::new(label),
                                        TextFont {
                                            font_size: 13.0,
                                            ..default()
                                        },
                                    ));
                                });
                        }
                    });
            }
            parent.spawn((
                Text::new(PANEL_HELP),
                TextFont {
                    font_size: 11.0,
                    ..default()
                },
                TextColor(Color::srgb(0.65, 0.72, 0.82)),
            ));
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

fn scroll_panel(
    state: Res<ConditionerDebug>,
    mut wheel: MessageReader<MouseWheel>,
    mut panels: Query<(&Interaction, &ComputedNode, &mut ScrollPosition), With<Panel>>,
    buttons: Query<&Interaction, With<Action>>,
) {
    let delta: f32 = wheel
        .read()
        .map(|event| {
            event.y
                * if event.unit == MouseScrollUnit::Line {
                    24.0
                } else {
                    1.0
                }
        })
        .sum();
    if !state.visible || delta == 0.0 {
        return;
    }
    let over_button = buttons
        .iter()
        .any(|interaction| *interaction != Interaction::None);
    for (interaction, node, mut scroll) in &mut panels {
        if *interaction != Interaction::None || over_button {
            let max =
                ((node.content_size().y - node.size().y) * node.inverse_scale_factor()).max(0.0);
            scroll.0.y = (scroll.0.y - delta).clamp(0.0, max);
        }
    }
}

fn millis(value: Option<Duration>) -> String {
    value
        .map(|value| format!("{:.1} ms", value.as_secs_f64() * 1000.0))
        .unwrap_or_else(|| "Measuring...".into())
}
fn refresh_panel(
    state: Res<ConditionerDebug>,
    mut panels: Query<&mut Node, With<Panel>>,
    mut texts: Query<(&Metric, &mut Text)>,
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
    let direction = |incoming: bool| {
        if incoming {
            stats.incoming
        } else {
            stats.outgoing
        }
    };
    for (metric, mut text) in &mut texts {
        let value = match *metric {
            Metric::Baseline => millis(state.baseline_rtt()),
            Metric::Observed => millis(state.observed_rtt()),
            Metric::Target => state
                .target_rtt
                .map(|v| millis(Some(v)))
                .unwrap_or_else(|| "Custom / off".into()),
            Metric::Delay => millis(Some(config.latency)),
            Metric::Jitter => format!("+/- {}", millis(Some(config.jitter))),
            Metric::Loss => format!("{:.0}%", config.packet_loss * 100.0),
            Metric::Status => if stats.outage_active {
                "OUTAGE"
            } else if config.enabled {
                "ON"
            } else {
                "OFF"
            }
            .into(),
            Metric::Note => {
                if state
                    .target_rtt
                    .zip(state.baseline_rtt())
                    .is_some_and(|(target, baseline)| target < baseline)
                {
                    "Target below baseline: no added delay.".into()
                } else {
                    state.status.clone()
                }
            }
            Metric::QueuePackets(incoming) => direction(incoming).queued_packets.to_string(),
            Metric::QueueBytes(incoming) => direction(incoming).queued_bytes.to_string(),
            Metric::LossDrops(incoming) => direction(incoming).simulated_loss_drops.to_string(),
            Metric::OutageDrops(incoming) => direction(incoming).outage_drops.to_string(),
            Metric::OverflowDrops(incoming) => direction(incoming).overflow_drops.to_string(),
            Metric::TransitionDrops(incoming) => direction(incoming).transition_drops.to_string(),
        };
        if text.0 != value {
            text.0 = value;
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
    #[test]
    fn structured_metrics_keep_directions_separate_and_labels_ascii() {
        use renet_cross::conditioner::ConditionerConfig;
        let handle = ConditionerHandle::new(ConditionerConfig {
            enabled: true,
            latency: Duration::from_secs(1),
            ..Default::default()
        })
        .unwrap();
        handle.outage(Duration::from_secs(1));
        assert!(handle.stats().outage_active);
        let mut config = handle.config();
        config.latency = Duration::from_millis(125);
        handle.configure(config).unwrap();
        assert!(!handle.stats().outage_active);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(ConditionerDebugPlugin::new(handle));
        app.update();
        let world = app.world_mut();
        let metrics: Vec<_> = world
            .query::<(&Metric, &Text)>()
            .iter(world)
            .map(|(metric, text)| (*metric, text.0.clone()))
            .collect();
        assert_eq!(metrics.len(), 20);
        assert!(
            metrics
                .iter()
                .any(|(metric, text)| matches!(metric, Metric::Delay) && text == "125.0 ms")
        );
        assert!(
            world
                .query_filtered::<&BackgroundColor, With<Panel>>()
                .iter(world)
                .all(|color| color.0.alpha() == 1.0)
        );
        assert!(
            metrics
                .iter()
                .any(|(metric, text)| matches!(metric, Metric::QueueBytes(true)) && text == "0")
        );
        assert!(
            metrics
                .iter()
                .any(|(metric, text)| matches!(metric, Metric::QueueBytes(false)) && text == "0")
        );
        assert!(
            world
                .query::<&Text>()
                .iter(world)
                .all(|text| text.0.is_ascii() && !text.0.contains('\n'))
        );
        assert_eq!(world.query::<&Action>().iter(world).count(), 11);
        world.resource_mut::<ConditionerDebug>().visible = false;
        app.update();
        let world = app.world_mut();
        assert!(
            world
                .query_filtered::<&Node, With<Panel>>()
                .iter(world)
                .all(|node| node.display == Display::None)
        );
    }
}
