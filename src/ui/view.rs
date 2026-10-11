use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, Window, div, px, rems,
};

use crate::app::{ActivityEntry, AppController, AppError, AppEvent, EventSink, RunRequest};
use crate::domain::{RunState, RunStats, ValidationError};
use crate::infrastructure::database::DuplicateReport;
use crate::ports::LogLevel;
use crate::ui::format;
use crate::ui::i18n::{Language, tr, trf};
use crate::ui::messages;

/// The activity panel keeps only the most recent rows.
const MAX_ACTIVITY_ROWS: usize = 250;
/// How often the view drains events while a run is active; the coordinator
/// already batches per-image results, so this only bounds UI latency.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

const DEFAULT_QUALITY: i32 = 50;
const TAB_ACTIVITY: usize = 0;
const TAB_DUPLICATES: usize = 1;

/// Delivers coordinator events to the view through a channel the view drains.
struct ChannelSink(Sender<AppEvent>);

impl EventSink for ChannelSink {
    fn send(&self, event: AppEvent) {
        // The receiver is gone only when the window closed mid-run; the
        // coordinator is being cancelled then, so the event has no reader.
        let _ = self.0.send(event);
    }
}

#[derive(Clone)]
struct LanguageChoice(Language);

impl SearchableListItem for LanguageChoice {
    type Value = Language;

    fn title(&self) -> SharedString {
        self.0.native_name().into()
    }

    fn value(&self) -> &Self::Value {
        &self.0
    }
}

/// One activity row with a domain identity, so rows keep their element ids
/// when older rows are dropped from the bounded list.
struct ActivityRow {
    id: u64,
    entry: ActivityEntry,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Idle,
    Active,
    Success,
    Warning,
    Error,
}

impl StatusKind {
    fn of(state: RunState) -> Self {
        match state {
            RunState::Idle => Self::Idle,
            RunState::Scanning | RunState::Processing => Self::Active,
            RunState::Cancelling | RunState::Cancelled => Self::Warning,
            RunState::Completed => Self::Success,
            RunState::Failed => Self::Error,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportStatus {
    Loading,
    Ready,
    Failed,
}

pub struct CompressorView {
    controller: Arc<AppController>,
    data_directory: String,
    language: Language,
    language_select: Entity<SelectState<Vec<LanguageChoice>>>,
    path_input: Entity<InputState>,
    quality_input: Entity<InputState>,
    workers_input: Entity<InputState>,
    verify_hash: bool,
    process_mpo: bool,
    delete_thumbs: bool,

    run_state: RunState,
    status_kind: StatusKind,
    status_code: &'static str,
    status_value: i32,
    status_detail: String,
    stats: RunStats,
    current_file: String,
    activity: VecDeque<ActivityRow>,
    next_activity_id: u64,

    active_tab: usize,
    report: DuplicateReport,
    report_status: ReportStatus,
    report_pending: bool,

    _job_events: Option<Task<()>>,
    _report_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CompressorView {
    pub fn new(
        controller: Arc<AppController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = Language::default();
        let choices = Language::ALL.map(LanguageChoice).to_vec();
        let language_select =
            cx.new(|cx| SelectState::new(choices, Some(IndexPath::new(0)), window, cx));
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(language, "Folder containing .jpg or .jpeg files"))
        });
        let quality_input = cx.new(|cx| number_state(DEFAULT_QUALITY, 1, 100, window, cx));
        let workers_input = cx.new(|cx| {
            number_state(
                AppController::default_worker_count(),
                1,
                crate::domain::MAX_WORKERS,
                window,
                cx,
            )
        });

        let subscriptions = vec![
            cx.subscribe_in(
                &language_select,
                window,
                |this, _, event: &SelectEvent<Vec<LanguageChoice>>, window, cx| {
                    if let SelectEvent::Confirm(Some(language)) = event {
                        this.set_language(*language, window, cx);
                    }
                },
            ),
            // The Compress button depends on whether a path was entered.
            cx.subscribe(&path_input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        let data_directory = format::display_path(&controller.data_directory().to_string_lossy());
        let mut view = Self {
            controller,
            data_directory,
            language,
            language_select,
            path_input,
            quality_input,
            workers_input,
            verify_hash: true,
            process_mpo: true,
            delete_thumbs: false,
            run_state: RunState::Idle,
            status_kind: StatusKind::Idle,
            status_code: "idle",
            status_value: 0,
            status_detail: String::new(),
            stats: RunStats::default(),
            current_file: String::new(),
            activity: VecDeque::new(),
            next_activity_id: 0,
            active_tab: TAB_ACTIVITY,
            report: DuplicateReport::default(),
            report_status: ReportStatus::Loading,
            report_pending: false,
            _job_events: None,
            _report_task: None,
            _subscriptions: subscriptions,
        };
        view.refresh_report(cx);
        view
    }

    fn tr(&self, source: &str) -> String {
        tr(self.language, source)
    }

    fn running(&self) -> bool {
        matches!(
            self.run_state,
            RunState::Scanning | RunState::Processing | RunState::Cancelling
        )
    }

    fn set_language(&mut self, language: Language, window: &mut Window, cx: &mut Context<Self>) {
        if self.language == language {
            return;
        }
        self.language = language;
        let placeholder = self.tr("Folder containing .jpg or .jpeg files");
        self.path_input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
        });
        window.set_window_title(&self.tr("Image Compressor"));
        cx.notify();
    }

    fn set_run_state(&mut self, state: RunState) {
        self.run_state = state;
        self.status_kind = StatusKind::of(state);
    }

    fn browse(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = rfd::AsyncFileDialog::new().set_title(self.tr("Select the image folder"));
        cx.spawn_in(window, async move |this, cx| {
            let Some(folder) = dialog.pick_folder().await else {
                return;
            };
            let path = folder.path().display().to_string();
            // The view may have closed while the dialog was open.
            let _ = this.update_in(cx, |view, window, cx| {
                view.path_input.update(cx, |state, cx| {
                    state.set_value(path, window, cx);
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let path = self.path_input.read(cx).value();
        let quality = parse_number(&self.quality_input.read(cx).value());
        let workers = parse_number(&self.workers_input.read(cx).value());
        let request = match RunRequest::new(
            &path,
            quality,
            workers,
            self.verify_hash,
            self.process_mpo,
            self.delete_thumbs,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.set_run_state(RunState::Failed);
                self.status_code = validation_status_code(error);
                cx.notify();
                return;
            }
        };

        let (sender, receiver) = unbounded();
        if let Err(error) = self
            .controller
            .start(request, Arc::new(ChannelSink(sender)))
        {
            // A rejected start leaves any previous results on screen.
            eprintln!("the run could not be started: {error}");
            self.status_kind = StatusKind::Error;
            self.status_code = error.status_code();
            cx.notify();
            return;
        }

        self.set_run_state(RunState::Scanning);
        self.status_code = "preparing";
        self.status_value = 0;
        self.status_detail.clear();
        self.stats = RunStats::default();
        self.current_file.clear();
        self.activity.clear();
        self.watch_job(receiver, cx);
        cx.notify();
    }

    fn cancel(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.controller.cancel() {
            Ok(true) => {
                self.set_run_state(RunState::Cancelling);
                self.status_code = "cancelling";
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("cancellation could not be requested: {error}");
                self.status_kind = StatusKind::Error;
                self.status_code = error.status_code();
            }
        }
        cx.notify();
    }

    /// Drains the run's events on the UI thread until the run finishes. Each
    /// run has its own channel, so a late event can never reach a newer run.
    fn watch_job(&mut self, receiver: Receiver<AppEvent>, cx: &mut Context<Self>) {
        self._job_events = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EVENT_POLL_INTERVAL).await;
                let mut events = Vec::new();
                let mut finished = false;
                let mut disconnected = false;
                loop {
                    match receiver.try_recv() {
                        Ok(event) => {
                            finished |= matches!(event, AppEvent::Finished { .. });
                            events.push(event);
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if events.is_empty() && !disconnected {
                    continue;
                }
                let updated = this.update(cx, |view, cx| {
                    for event in events {
                        view.apply_event(event, cx);
                    }
                    if disconnected && !finished {
                        // The coordinator ended without its final event.
                        view.set_run_state(RunState::Failed);
                        view.status_code = "thread-panicked";
                    }
                    cx.notify();
                });
                if finished || disconnected || updated.is_err() {
                    break;
                }
            }
        }));
    }

    fn apply_event(&mut self, event: AppEvent, cx: &mut Context<Self>) {
        match event {
            AppEvent::State {
                state,
                status_code,
                status_value,
            } => {
                self.set_run_state(state);
                self.status_code = status_code;
                self.status_value = status_value;
            }
            AppEvent::Progress {
                stats,
                current_file,
            } => {
                self.stats = stats;
                self.current_file = current_file;
            }
            AppEvent::Activity(entries) => self.push_activity(entries),
            AppEvent::Finished {
                state,
                stats,
                elapsed,
            } => {
                self.stats = stats;
                self.set_run_state(state);
                self.current_file.clear();
                match state {
                    RunState::Completed => {
                        if stats.has_incidents() {
                            self.status_kind = StatusKind::Warning;
                            self.status_code = "completed-incidents";
                        } else {
                            self.status_code = "completed";
                        }
                        self.status_detail = format::duration(elapsed);
                    }
                    RunState::Cancelled => {
                        self.status_code = "cancelled";
                        self.status_detail = format::duration(elapsed);
                    }
                    // A failure keeps the status code its State event set.
                    _ => {}
                }
                self.refresh_report(cx);
            }
        }
    }

    fn push_activity(&mut self, entries: Vec<ActivityEntry>) {
        for entry in entries {
            if self.activity.len() >= MAX_ACTIVITY_ROWS {
                self.activity.pop_back();
            }
            self.activity.push_front(ActivityRow {
                id: self.next_activity_id,
                entry,
            });
            self.next_activity_id = self.next_activity_id.wrapping_add(1);
        }
    }

    /// Reloads the duplicate report off the UI thread. A request made while
    /// one is loading is queued, so the report never misses a finished run.
    fn refresh_report(&mut self, cx: &mut Context<Self>) {
        if self._report_task.is_some() {
            self.report_pending = true;
            return;
        }
        self.report_status = ReportStatus::Loading;
        let controller = Arc::clone(&self.controller);
        self._report_task = Some(cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async move { controller.duplicate_report() })
                .await;
            let _ = this.update(cx, |view, cx| view.finish_report(report, cx));
        }));
        cx.notify();
    }

    fn finish_report(&mut self, report: Result<DuplicateReport, AppError>, cx: &mut Context<Self>) {
        self._report_task = None;
        match report {
            Ok(report) => {
                self.report = report;
                self.report_status = ReportStatus::Ready;
            }
            Err(error) => {
                // Previous data stays visible; only the note reports the error.
                eprintln!("the duplicate report could not be loaded: {error}");
                self.report_status = ReportStatus::Failed;
            }
        }
        if std::mem::take(&mut self.report_pending) {
            self.refresh_report(cx);
        }
        cx.notify();
    }

    fn select_tab(&mut self, index: &usize, _: &mut Window, cx: &mut Context<Self>) {
        self.active_tab = *index;
        if *index == TAB_DUPLICATES {
            self.refresh_report(cx);
        }
        cx.notify();
    }

    fn open_about(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let language = self.language;
        let data_directory = self.data_directory.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            dialog
                .width(px(448.))
                .title(tr(language, "Image Compressor"))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(trf(language, "Version {}", &[&env!("CARGO_PKG_VERSION")])),
                        )
                        .child(div().text_sm().child(tr(
                            language,
                            "Local application written in Rust. Files are not sent to any network service.",
                        )))
                        .child(div().text_xs().text_color(muted).child(trf(
                            language,
                            "Data and log folder (next to the executable): {}",
                            &[&data_directory],
                        )))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tr(language, "Built with GPUI Kit")),
                        ),
                )
                .footer(
                    h_flex().w_full().justify_end().child(
                        Button::new("close-about")
                            .primary()
                            .label(tr(language, "Close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    fn card(cx: &App) -> Div {
        let theme = cx.theme();
        v_flex()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .bg(theme.background)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child(self.tr("Image Compressor")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                self.tr("Safe, parallel JPEG compression with duplicate history"),
                            ),
                    ),
            )
            .child(
                div()
                    .w_32()
                    .child(Select::new(&self.language_select).id("language")),
            )
            .child(
                Button::new("about")
                    .icon(IconName::Info)
                    .label(self.tr("About"))
                    .on_click(cx.listener(Self::open_about)),
            )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running();
        let muted = cx.theme().muted_foreground;
        let label = |text: String| div().text_sm().text_color(muted).child(text);

        Self::card(cx)
            .px_4()
            .py_3()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.tr("Source and settings")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.path_input).disabled(running)),
                    )
                    .child(
                        Button::new("browse")
                            .icon(IconName::FolderOpen)
                            .label(self.tr("Browse…"))
                            .disabled(running)
                            .on_click(cx.listener(Self::browse)),
                    ),
            )
            .child(
                h_flex()
                    .gap_6()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label(self.tr("JPEG quality")))
                            .child(
                                div()
                                    .w_32()
                                    .child(NumberInput::new(&self.quality_input).disabled(running)),
                            ),
                    )
                    .child(
                        h_flex().gap_2().child(label(self.tr("Workers"))).child(
                            div()
                                .w_32()
                                .child(NumberInput::new(&self.workers_input).disabled(running)),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_6()
                    .gap_y_2()
                    .child(
                        Checkbox::new("verify-hash")
                            .label(self.tr("Skip images already recorded"))
                            .checked(self.verify_hash)
                            .disabled(running)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.verify_hash = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("process-mpo")
                            .label(self.tr("Convert the primary MPO image"))
                            .checked(self.process_mpo)
                            .disabled(running)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.process_mpo = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("delete-thumbs")
                            .label(self.tr("Delete Thumbs.db files when finished"))
                            .checked(self.delete_thumbs)
                            .disabled(running)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.delete_thumbs = *checked;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn status_color(&self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self.status_kind {
            StatusKind::Idle => theme.muted_foreground,
            StatusKind::Active => theme.info,
            StatusKind::Success => theme.success,
            StatusKind::Warning => theme.warning,
            StatusKind::Error => theme.danger,
        }
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running();
        let cancelling = self.run_state == RunState::Cancelling;
        let color = self.status_color(cx);
        let has_path = !self.path_input.read(cx).value().trim().is_empty();
        let message = messages::status(
            self.language,
            self.status_code,
            self.status_value,
            &self.status_detail,
        );

        let action = if running {
            Button::new("cancel")
                .label(if cancelling {
                    self.tr("Cancelling…")
                } else {
                    self.tr("Cancel")
                })
                .disabled(cancelling)
                .on_click(cx.listener(Self::cancel))
        } else {
            Button::new("compress")
                .primary()
                .label(self.tr("Compress"))
                .disabled(!has_path)
                .on_click(cx.listener(Self::start))
        };

        let border = if self.status_kind == StatusKind::Idle {
            cx.theme().border
        } else {
            color
        };
        Self::card(cx)
            .border_color(border)
            .px_4()
            .py_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(message),
                    )
                    .child(action),
            )
            .child(
                Progress::new("progress")
                    .value(self.stats.progress() * 100.0)
                    .loading(self.run_state == RunState::Scanning),
            )
            .child(
                div()
                    .h_4()
                    .text_xs()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.current_file.clone()),
            )
    }

    fn stat_card(label: String, value: String, accent: Hsla, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_1()
            .min_w_0()
            .items_stretch()
            .gap_3()
            .px_3()
            .py_1p5()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .child(div().w_1().rounded_full().bg(accent))
            .child(
                v_flex()
                    .min_w_0()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child(value))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    ),
            )
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let stats = self.stats;
        let row = |left: AnyElement, right: AnyElement| h_flex().gap_2().child(left).child(right);

        v_flex()
            .id("summary")
            .w(rems(22.))
            .flex_none()
            .gap_2()
            .overflow_y_scrollbar()
            .child(row(
                Self::stat_card(self.tr("Found"), stats.total_found.to_string(), theme.info, cx)
                    .into_any_element(),
                Self::stat_card(self.tr("Optimized"), stats.optimized.to_string(), theme.success, cx)
                    .into_any_element(),
            ))
            .child(row(
                Self::stat_card(self.tr("Skipped"), stats.skipped.to_string(), theme.warning, cx)
                    .into_any_element(),
                Self::stat_card(self.tr("Errors"), stats.total_errors().to_string(), theme.danger, cx)
                    .into_any_element(),
            ))
            .child(row(
                Self::stat_card(
                    self.tr("Space saved"),
                    format::signed_bytes(stats.saved_bytes()),
                    theme.primary,
                    cx,
                )
                .into_any_element(),
                Self::stat_card(
                    self.tr("Thumbs.db deleted"),
                    stats.thumbs_deleted.to_string(),
                    theme.muted_foreground,
                    cx,
                )
                .into_any_element(),
            ))
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .bg(theme.muted)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.tr("Data protection")),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(self.tr(
                        "Each image is written to a temporary file in the same directory first. The original is replaced only after encoding and synchronization succeed.",
                    )))
                    .child(if stats.partial > 0 {
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child(self.tr("Warning: some optimized files were not recorded in SQLite."))
                    } else {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.tr("Do not shut down the computer while an operation is running."))
                    }),
            )
    }

    fn render_activity(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.activity.is_empty() {
            return empty_state(
                self.tr("Recent activity will appear here."),
                theme.muted_foreground,
            );
        }
        v_flex()
            .id("activity-list")
            .size_full()
            .gap_1()
            .p_2()
            .overflow_y_scrollbar()
            .children(self.activity.iter().map(|row| {
                let color = match row.entry.level {
                    LogLevel::Info => theme.success,
                    LogLevel::Warning => theme.warning,
                    LogLevel::Error => theme.danger,
                };
                h_flex()
                    .id(("activity", row.id))
                    .flex_none()
                    .items_stretch()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .child(div().w_1().flex_none().rounded_full().bg(color))
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_start()
                                    .child(messages::activity_source(
                                        self.language,
                                        &row.entry.source,
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(messages::activity(
                                        self.language,
                                        row.entry.message_code,
                                        row.entry.message_args,
                                    )),
                            ),
                    )
            }))
            .into_any_element()
    }

    fn report_note(&self) -> String {
        match self.report_status {
            ReportStatus::Loading => self.tr("Reading local history…"),
            ReportStatus::Failed => self.tr("The duplicate report could not be loaded."),
            ReportStatus::Ready if self.report.total_groups > self.report.groups.len() => trf(
                self.language,
                "Showing the first {} of {} groups. Local data: {}",
                &[
                    &self.report.groups.len(),
                    &self.report.total_groups,
                    &self.data_directory,
                ],
            ),
            ReportStatus::Ready => trf(self.language, "Local data: {}", &[&self.data_directory]),
        }
    }

    fn render_duplicates(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let report = &self.report;
        let loading = self.report_status == ReportStatus::Loading;

        let header = v_flex()
            .gap_1()
            .px_2()
            .pt_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(trf(
                                self.language,
                                "Records: {} · Unique: {} · Duplicates: {} · Groups: {}",
                                &[
                                    &report.total_records,
                                    &report.unique_hashes,
                                    &report.duplicate_files,
                                    &report.total_groups,
                                ],
                            )),
                    )
                    .child(
                        Button::new("refresh-report")
                            .small()
                            .icon(IconName::RefreshCw)
                            .label(if loading {
                                self.tr("Loading…")
                            } else {
                                self.tr("Refresh")
                            })
                            .disabled(loading)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.refresh_report(cx);
                            })),
                    ),
            )
            .child(div().text_sm().text_color(theme.warning).child(trf(
                self.language,
                "Visible duplicate space: {}",
                &[&format::bytes(report.visible_wasted_bytes)],
            )))
            .child(
                div()
                    .text_xs()
                    .truncate()
                    .text_color(theme.muted_foreground)
                    .child(self.report_note()),
            );

        let body = if report.groups.is_empty() {
            if loading {
                div().flex_1().into_any_element()
            } else {
                empty_state(self.tr("No duplicate groups are recorded."), theme.success)
            }
        } else {
            v_flex()
                .id("duplicate-list")
                .flex_1()
                .min_h_0()
                .gap_2()
                .p_2()
                .overflow_y_scrollbar()
                .children(report.groups.iter().map(|group| {
                    v_flex()
                        .id(SharedString::from(format!(
                            "duplicate-{}",
                            group.hash_prefix
                        )))
                        .flex_none()
                        .gap_1()
                        .p_3()
                        .border_1()
                        .border_color(theme.border)
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme.warning)
                                        .child(trf(self.language, "{} copies", &[&group.copies])),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(group.hash_prefix.clone()),
                                ),
                        )
                        .child(div().text_xs().child(trf(
                            self.language,
                            "Duplicate space: {}",
                            &[&format::bytes(group.wasted_bytes)],
                        )))
                        .children(group.locations.lines().map(|location| {
                            div()
                                .text_xs()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_start()
                                .text_color(theme.muted_foreground)
                                .child(format::display_path(location))
                        }))
                }))
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_panels(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.active_tab == TAB_DUPLICATES {
            self.render_duplicates(cx)
        } else {
            self.render_activity(cx)
        };
        Self::card(cx)
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(
                div().px_2().pt_2().child(
                    TabBar::new("panels")
                        .underline()
                        .selected_index(self.active_tab)
                        .on_click(cx.listener(Self::select_tab))
                        .child(Tab::new().label(self.tr("Activity")))
                        .child(Tab::new().label(self.tr("Duplicates"))),
                ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

impl Render for CompressorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(cx))
            .child(self.render_settings(cx))
            .child(self.render_status(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .gap_3()
                    .child(self.render_summary(cx))
                    .child(self.render_panels(cx)),
            )
    }
}

fn empty_state(text: String, color: Hsla) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn number_state(
    value: i32,
    min: i32,
    max: usize,
    window: &mut Window,
    cx: &mut Context<InputState>,
) -> InputState {
    InputState::new(window, cx)
        .default_value(value.to_string())
        .min(f64::from(min))
        .max(max as f64)
        .step(1.0)
        .validate(|text, _| {
            text.len() <= 3 && text.chars().all(|character| character.is_ascii_digit())
        })
}

/// Parses a numeric field; anything unparsable becomes 0, which request
/// validation rejects with a precise message.
fn parse_number(text: &str) -> i32 {
    text.trim().parse().unwrap_or(0)
}

fn validation_status_code(error: ValidationError) -> &'static str {
    match error {
        ValidationError::EmptyPath => "empty-path",
        ValidationError::PathTooLong => "path-too-long",
        ValidationError::InvalidQuality => "invalid-quality",
        ValidationError::InvalidWorkerCount => "invalid-workers",
    }
}

#[cfg(test)]
mod tests {
    use super::parse_number;

    #[test]
    fn unparsable_numbers_fall_back_to_an_invalid_value() {
        assert_eq!(parse_number(" 75 "), 75);
        assert_eq!(parse_number(""), 0);
        assert_eq!(parse_number("abc"), 0);
    }
}
