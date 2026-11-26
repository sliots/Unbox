//! The progress window: drives the worker process, shows prompts, and delivers the
//! result.

mod complete;
mod effects;
mod headless;
mod plan;
mod recycle;

pub use headless::extract_cli;

use effects::{Taskbar, TaskbarState};
use plan::ExtractionPlan;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unbox_core::ipc::*;
use unbox_core::manifest::Manifest;
use unbox_core::progress::{SpeedGraph, SpeedMeter};
use unbox_core::settings::UserSettings;
use unbox_core::text::{describe_failure, format_bytes, format_duration, format_speed};

slint::include_modules!();

const MIB: u64 = 1024 * 1024;
const GRAPH_W: f64 = 448.0;
const GRAPH_H: f64 = 92.0;

// ---------------------------------------------------------------------------
// Worker process handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WorkerHandle {
    generation: u64,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl WorkerHandle {
    fn send(&self, command: &WorkerCommand) {
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            let _ = writeln!(stdin, "{}", to_line(command));
            let _ = stdin.flush();
        }
    }

    fn close_stdin(&self) {
        self.stdin.lock().unwrap().take();
    }

    fn try_exit(&self) -> Option<i32> {
        self.child.lock().unwrap().try_wait().ok().flatten().map(|s| s.code().unwrap_or(-1))
    }

    fn wait_exit(&self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_exit() {
                return Some(code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }

    /// Wait for the worker to leave (killing it if needed) before cleaning staging, so
    /// no file is still open while it is removed.
    fn stop_and_cleanup(&self, plan: Option<&ExtractionPlan>, manifest: &str) {
        if self.wait_exit(Duration::from_secs(3)).is_none() {
            self.kill();
            if self.wait_exit(Duration::from_secs(2)).is_none() {
                return;
            }
        }
        if let Some(plan) = plan {
            plan.cleanup_staging();
        }
        remove_manifest(manifest);
    }
}

fn remove_manifest(path: &str) {
    if !path.is_empty() {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Messages posted to the UI thread
// ---------------------------------------------------------------------------

enum Msg {
    Event(u64, WorkerEvent),
    Exited(u64, i32),
    RetryPrepared(u64, Box<FinishedReport>, RetryPrep),
    Finalized(u64, FinalOutcome),
    SoundDone(bool),
    Stopped,
}

enum RetryPrep {
    Prompt { phase: Phase, limit: u64, written: u64, intermediate: u64, available: u64, minimum: u64 },
    Fail(Option<String>),
}

enum FinalOutcome {
    Failed { detail: String, keep_staging: bool },
    Delivered { final_dir: String, notes: String, partial: bool, recycle: Option<recycle::RecycleResult> },
}

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<RefCell<Controller>>>> = const { RefCell::new(None) };
}

fn post(msg: Msg) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(controller) = CONTROLLER.with(|c| c.borrow().clone()) {
            controller.borrow_mut().on_message(msg);
        }
    });
}

fn with_controller(f: impl FnOnce(&mut Controller)) {
    if let Some(controller) = CONTROLLER.with(|c| c.borrow().clone()) {
        f(&mut controller.borrow_mut());
    }
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Attempt {
    outer_limit: Option<u64>,
    inner_limit: Option<u64>,
}

enum PromptCtx {
    None,
    Password,
    InvalidNames,
    Encoding(Vec<u32>),
    RecycleNotice,
    Budget { phase: Phase, limit: u64, intermediate: u64, minimum: u64, report: Box<FinishedReport> },
}

struct Controller {
    ui: slint::Weak<AppWindow>,
    archive: PathBuf,
    plan: Option<ExtractionPlan>,
    worker: Option<WorkerHandle>,
    _job: Option<crate::win::job::Job>,
    generation: u64,
    manifest_path: String,
    settings: UserSettings,
    settings_path: PathBuf,
    attempt: Attempt,
    source_identity: Option<VolumeIdentity>,
    taskbar: Option<Taskbar>,
    hwnd: Option<windows_sys::Win32::Foundation::HWND>,

    paused: bool,
    completed: bool,
    finished_handled: bool,
    error_shown: bool,
    closing: bool,
    close_requested: bool,

    total_bytes: u64,
    completed_bytes: u64,
    total_items: u32,
    size_known: bool,
    phase: Option<Phase>,
    started: Instant,
    meter: SpeedMeter,
    graph: SpeedGraph,
    prompt: PromptCtx,
}

impl Controller {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window alive")
    }

    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn save_settings(&self) {
        self.settings.save(&self.settings_path);
    }

    fn send(&self, command: WorkerCommand) {
        if let Some(worker) = &self.worker {
            worker.send(&command);
        }
    }

    fn set_status(&self, text: &str) {
        let ui = self.ui();
        ui.set_status_text(text.into());
        ui.set_title_text(text.into());
    }

    fn taskbar(&mut self, state: TaskbarState, value: f64) {
        if let Some(taskbar) = self.taskbar.as_mut() {
            taskbar.set(state, value);
        }
    }

    // ----- lifecycle -----

    fn start(&mut self) {
        if !self.archive.is_file() {
            let path = self.archive.display().to_string();
            self.trigger_error("文件不存在", &path);
            return;
        }
        let plan = ExtractionPlan::new(&self.archive);
        self.source_identity = Some(crate::win::capture_identity(&plan.source));
        let ui = self.ui();
        ui.set_path_info(format!("正在将 1 个项目从 {} 解压到 {}", plan.source_name(), plan.target_name).into());
        self.set_status("正在打开压缩包…");
        self.plan = Some(plan);
        self.started = Instant::now();
        self.meter.reset(0.0, 0);
        self.spawn_worker();
    }

    fn spawn_worker(&mut self) {
        let plan = self.plan.clone().expect("plan");
        if let Err(e) = std::fs::create_dir_all(&plan.staging) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                self.trigger_error(
                    "无法写入解压目录",
                    &format!("Windows 拒绝在「{}」创建暂存目录。解压尚未开始，源压缩包已保留。", plan.parent),
                );
            } else {
                self.trigger_error("解压启动出错", &e.to_string());
            }
            return;
        }
        self.manifest_path = new_manifest_path(&plan);

        self.generation += 1;
        let result = spawn_worker_process(&plan, &self.manifest_path, self.settings.archive_code_page, self.attempt, self.generation);

        match result {
            Ok((worker, job, stdout, stderr)) => {
                let reader_worker = worker.clone();
                std::thread::spawn(move || {
                    let generation = reader_worker.generation;
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        if let Ok(event) = serde_json::from_str::<WorkerEvent>(line.trim()) {
                            post(Msg::Event(generation, event));
                        }
                    }
                    let code = reader_worker.wait_exit(Duration::from_secs(3600 * 24)).unwrap_or(-1);
                    post(Msg::Exited(generation, code));
                });
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
                });
                self.worker = Some(worker);
                self._job = Some(job);
            }
            Err(e) => {
                plan.cleanup_staging();
                remove_manifest(&self.manifest_path);
                self.trigger_error("解压启动出错", &e.to_string());
            }
        }
    }

    fn on_message(&mut self, msg: Msg) {
        match msg {
            Msg::Event(generation, event) => {
                if generation != self.generation || self.closing || self.finished_handled || self.error_shown || self.completed {
                    return;
                }
                self.on_event(event);
            }
            Msg::Exited(generation, code) => {
                if generation == self.generation && !self.finished_handled && !self.error_shown && !self.completed && !self.closing {
                    self.trigger_error("解压中断", &format!("工作进程异常退出（退出代码 {code}）"));
                }
            }
            Msg::RetryPrepared(generation, report, prep) => {
                if generation == self.generation {
                    self.on_retry_prepared(report, prep);
                }
            }
            Msg::Finalized(generation, outcome) => {
                if generation == self.generation {
                    self.on_finalized(outcome);
                }
            }
            Msg::SoundDone(auto_close) => {
                if auto_close && !self.close_requested {
                    slint::Timer::single_shot(Duration::from_millis(800), || slint::quit_event_loop().unwrap_or_default());
                }
            }
            Msg::Stopped => {
                let _ = slint::quit_event_loop();
            }
        }
    }

    fn on_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Progress { phase, current_file, completed_bytes, total_bytes, size_known } => {
                if self.phase != Some(phase) {
                    self.phase = Some(phase);
                    self.meter.reset(self.now(), 0);
                }
                self.completed_bytes = completed_bytes;
                self.total_bytes = total_bytes;
                self.size_known = size_known;
                self.update_progress(Some(&current_file));
            }
            WorkerEvent::Init { total_items, total_bytes, size_known, .. } => {
                self.total_items = total_items;
                self.total_bytes = total_bytes;
                self.size_known = size_known;
                if let Some(plan) = &self.plan {
                    self.ui()
                        .set_path_info(format!("正在将 {total_items} 个项目从 {} 解压到 {}", plan.source_name(), plan.target_name).into());
                }
                self.update_progress(None);
            }
            WorkerEvent::InvalidCharsFound { items } => self.ask_invalid_names(items),
            WorkerEvent::EncodingOptions { options } => self.ask_encoding(options),
            WorkerEvent::PasswordRequest => {
                self.prompt = PromptCtx::Password;
                self.show_prompt(PromptKind::Password, "输入密码", "确定");
            }
            WorkerEvent::Finished(report) => self.on_finished(report),
            WorkerEvent::Canceled => self.request_close(),
            WorkerEvent::Error { message, error_reason } => {
                let message = match error_reason {
                    Some(reason) => describe_failure(&reason, -1, "failed"),
                    None => message,
                };
                self.trigger_error("解压失败", &message);
            }
            WorkerEvent::PausedAck | WorkerEvent::ResumedAck => {}
        }
    }

    // ----- progress -----

    fn update_progress(&mut self, current_file: Option<&str>) {
        let ui = self.ui();
        if let Some(file) = current_file {
            ui.set_detail_name(file.into());
        }
        let determinate = self.size_known && self.total_bytes > 0;
        ui.set_indeterminate(!determinate);
        let progress = if determinate { (self.completed_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) } else { 0.0 };
        ui.set_progress(progress as f32);
        let mut status = if determinate {
            format!("已完成 {:.0}%", progress * 100.0)
        } else {
            format!("已解出 {}", format_bytes(self.completed_bytes))
        };
        if self.paused {
            status = format!("已暂停 · {status}");
        }
        self.set_status(&status);
        let taskbar_state = if self.paused {
            TaskbarState::Paused
        } else if determinate {
            TaskbarState::Normal
        } else {
            TaskbarState::Indeterminate
        };
        self.taskbar(taskbar_state, progress);

        let now = self.now();
        self.meter.sample(now, self.completed_bytes, self.paused);
        self.graph.push(progress, self.meter.current, self.meter.displayed);
        ui.set_graph_area(self.graph.area_path(GRAPH_W, GRAPH_H).into());
        ui.set_graph_progress(self.graph.progress as f32);
        ui.set_graph_reference_y(self.graph.reference_y(GRAPH_H) as f32);
        ui.set_graph_label(if self.paused {
            "已暂停".into()
        } else {
            format!("速度: {}", format_speed(self.graph.shown_speed())).into()
        });

        if !determinate {
            ui.set_detail_remaining(format!("已解出 {}", format_bytes(self.completed_bytes)).into());
            ui.set_detail_eta("—".into());
            return;
        }
        let remaining = self.total_bytes.saturating_sub(self.completed_bytes);
        let mut items = ((1.0 - progress) * self.total_items as f64).ceil().max(0.0) as u64;
        if remaining > 0 && items == 0 {
            items = 1;
        }
        ui.set_detail_remaining(format!("{items} ({})", format_bytes(remaining)).into());
        let eta = if progress >= 1.0 || remaining == 0 {
            "完成".to_string()
        } else if self.meter.current > 0.1 {
            format!("大约 {}", format_duration(remaining as f64 / MIB as f64 / self.meter.current))
        } else if progress > 0.01 {
            "大约 几秒钟".to_string()
        } else {
            "正在计算…".to_string()
        };
        ui.set_detail_eta(eta.into());
    }

    fn tick(&mut self) {
        if self.completed || self.paused || self.error_shown || self.closing || self.finished_handled || self.worker.is_none() {
            return;
        }
        self.update_progress(None);
    }

    // ----- user actions -----

    fn pause_resume(&mut self) {
        if self.finished_handled || self.completed || self.error_shown {
            return;
        }
        self.paused = !self.paused;
        self.ui().set_state(if self.paused { TaskState::Paused } else { TaskState::Running });
        if self.paused {
            self.update_progress(None);
            self.send(WorkerCommand::Pause);
        } else {
            let now = self.now();
            self.meter.rebase(now, self.completed_bytes);
            self.update_progress(None);
            self.send(WorkerCommand::Resume);
        }
    }

    fn toggle_details(&mut self) {
        let ui = self.ui();
        let expanded = !ui.get_expanded();
        ui.set_expanded(expanded);
        self.settings.is_details_expanded = expanded;
        self.save_settings();
    }

    fn recycle_toggled(&mut self, checked: bool) {
        if checked {
            // Explain once per opt-in what recycling covers.
            self.ui().set_recycle_checked(false);
            self.prompt = PromptCtx::RecycleNotice;
            self.show_prompt(PromptKind::RecycleNotice, "移入回收站", "我知道了");
        } else {
            self.settings.recycle_on_success = false;
            self.save_settings();
        }
    }

    fn open_folder(&mut self) {
        if let Some(plan) = &self.plan {
            if Path::new(&plan.final_dir).is_dir() {
                let _ = Command::new("explorer.exe").arg(&plan.final_dir).spawn();
            }
        }
        self.request_close();
    }

    // ----- prompts -----

    fn show_prompt(&mut self, kind: PromptKind, title: &str, submit: &str) {
        let ui = self.ui();
        ui.set_prompt_title(title.into());
        ui.set_prompt_submit(submit.into());
        ui.set_prompt_choice(0);
        ui.set_prompt_remember(false);
        ui.set_prompt_input(SharedString::default());
        ui.set_prompt_error(SharedString::default());
        ui.set_prompt(kind);
        if let Some(hwnd) = self.hwnd {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            }
        }
    }

    fn hide_prompt(&mut self) {
        let ui = self.ui();
        ui.set_prompt(PromptKind::None);
        ui.set_prompt_input(SharedString::default());
        self.prompt = PromptCtx::None;
    }

    fn ask_invalid_names(&mut self, items: Vec<InvalidItem>) {
        if let Some(action) = self.settings.remembered_invalid_action() {
            self.send(WorkerCommand::InvalidCharsResponse { action });
            return;
        }
        let lines: Vec<SharedString> = items.iter().map(|i| format!("• {}  →  {}", i.raw, i.sanitized).into()).collect();
        self.ui().set_invalid_names(ModelRc::new(VecModel::from(lines)));
        self.prompt = PromptCtx::InvalidNames;
        self.show_prompt(PromptKind::InvalidNames, "文件名包含非法字符", "确定应用");
    }

    fn ask_encoding(&mut self, options: Vec<EncodingOption>) {
        if options.is_empty() {
            self.send(WorkerCommand::EncodingResponse { cp: 0 });
            return;
        }
        if self.settings.remember_archive_code_page && self.settings.archive_code_page != 0 {
            self.send(WorkerCommand::EncodingResponse { cp: self.settings.archive_code_page });
            return;
        }
        let choices: Vec<EncodingChoice> = options
            .iter()
            .map(|o| EncodingChoice {
                label: o.label.clone().into(),
                samples: if o.samples.is_empty() {
                    SharedString::default()
                } else {
                    format!("示例：{}", o.samples.join("  /  ")).into()
                },
            })
            .collect();
        self.ui().set_encodings(ModelRc::new(VecModel::from(choices)));
        self.prompt = PromptCtx::Encoding(options.iter().map(|o| o.cp).collect());
        self.show_prompt(PromptKind::Encoding, "选择文件名编码", "应用并继续");
    }

    fn prompt_accepted(&mut self) {
        let ui = self.ui();
        match std::mem::replace(&mut self.prompt, PromptCtx::None) {
            PromptCtx::None => {}
            PromptCtx::Password => {
                let password = ui.get_prompt_input().to_string();
                self.send(WorkerCommand::PasswordResponse { password });
            }
            PromptCtx::InvalidNames => {
                let action = if ui.get_prompt_choice() == 0 { InvalidCharsAction::BatchRename } else { InvalidCharsAction::Skip };
                if ui.get_prompt_remember() {
                    self.settings.remember_invalid_action(action);
                    self.save_settings();
                }
                self.send(WorkerCommand::InvalidCharsResponse { action });
            }
            PromptCtx::Encoding(cps) => {
                let cp = cps.get(ui.get_prompt_choice().max(0) as usize).copied().unwrap_or(0);
                if ui.get_prompt_remember() && cp != 0 {
                    self.settings.archive_code_page = cp;
                    self.settings.remember_archive_code_page = true;
                    self.save_settings();
                }
                self.send(WorkerCommand::EncodingResponse { cp });
            }
            PromptCtx::RecycleNotice => {
                ui.set_recycle_checked(true);
                self.settings.recycle_on_success = true;
                self.save_settings();
            }
            PromptCtx::Budget { phase, limit, intermediate, minimum, report } => {
                let input = ui.get_prompt_input().trim().to_string();
                let Some(available) = self.free_mib(intermediate) else {
                    ui.set_prompt_error("无法查询目标目录的可用空间。请检查连接和目录访问权限后重新打开源包。".into());
                    self.prompt = PromptCtx::Budget { phase, limit, intermediate, minimum, report };
                    return;
                };
                if minimum > available {
                    ui.set_prompt_error("目标目录的可用空间不足以提高解压上限。请释放空间后重新打开源包。".into());
                    self.prompt = PromptCtx::Budget { phase, limit, intermediate, minimum, report };
                    return;
                }
                match input.parse::<u64>() {
                    Ok(entered) if input.bytes().all(|b| b.is_ascii_digit()) && entered >= minimum && entered <= available => {
                        self.hide_prompt();
                        if self.close_requested || !self.sources_match(&report.volume_identities, &report.opened_volumes) {
                            self.finalize(report, None);
                        } else {
                            self.retry(phase, entered * MIB);
                        }
                    }
                    _ => {
                        ui.set_prompt_error(format!("请输入 {minimum} 到 {available} MiB 之间的整数。").into());
                        self.prompt = PromptCtx::Budget { phase, limit, intermediate, minimum, report };
                    }
                }
                return;
            }
        }
        self.hide_prompt();
    }

    fn prompt_dismissed(&mut self) {
        match std::mem::replace(&mut self.prompt, PromptCtx::None) {
            PromptCtx::RecycleNotice => {
                self.ui().set_recycle_checked(false);
                self.settings.recycle_on_success = false;
                self.save_settings();
                self.hide_prompt();
            }
            PromptCtx::Budget { report, .. } => {
                self.hide_prompt();
                self.finalize(report, None);
            }
            PromptCtx::None => self.hide_prompt(),
            _ => {
                // Dismissing a question the worker is waiting on cancels the task.
                self.hide_prompt();
                self.request_close();
            }
        }
    }

    // ----- errors -----

    fn trigger_error(&mut self, title: &str, message: &str) {
        self.error_shown = true;
        let ui = self.ui();
        ui.set_state(TaskState::Failed);
        ui.set_indeterminate(false);
        ui.set_finished(true);
        ui.set_can_open_folder(false);
        ui.set_controls_enabled(false);
        self.set_status(title);
        ui.set_path_info(message.into());
        self.taskbar(TaskbarState::Error, 1.0);
        if let Some(hwnd) = self.hwnd {
            effects::demand_attention(hwnd);
        }
    }

    fn fail_without_commit(&mut self, detail: &str, keep_staging: bool) {
        let ui = self.ui();
        ui.set_state(TaskState::Failed);
        ui.set_indeterminate(false);
        ui.set_finished(true);
        ui.set_can_open_folder(false);
        ui.set_status_text("解压未完成".into());
        ui.set_path_info(detail.into());
        let name = self.plan.as_ref().map(|p| p.source_name()).unwrap_or_default();
        ui.set_title_text(format!("解压失败 - {name}").into());
        if !keep_staging {
            if let Some(plan) = &self.plan {
                plan.cleanup_staging();
            }
        }
        self.taskbar(TaskbarState::Error, 1.0);
        if let Some(hwnd) = self.hwnd {
            effects::demand_attention(hwnd);
        }
    }

    // ----- completion -----

    fn on_finished(&mut self, report: Box<FinishedReport>) {
        if self.finished_handled {
            return;
        }
        self.finished_handled = true;
        let ui = self.ui();
        ui.set_controls_enabled(false);
        ui.set_state(TaskState::Verifying);
        ui.set_status_text("正在校验和交付…".into());

        let is_budget_failure = report.status == FinishStatus::Failed
            && report.error_reason == "output_budget_exceeded"
            && report.manifest == self.manifest_path
            && !report.size_known
            && report.written_bytes <= report.limit_bytes;
        if !is_budget_failure {
            self.finalize(report, None);
            return;
        }
        let (Some(worker), Some(plan)) = (self.worker.clone(), self.plan.clone()) else {
            self.finalize(report, None);
            return;
        };
        let source = self.source_identity.clone();
        let manifest = self.manifest_path.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            let prep = prepare_retry(&worker, &plan, source.as_ref(), &manifest, &report);
            post(Msg::RetryPrepared(generation, report, prep));
        });
    }

    fn free_mib(&self, intermediate: u64) -> Option<u64> {
        let plan = self.plan.as_ref()?;
        let free = crate::win::disk_free(Path::new(&plan.parent))?;
        Some(free.saturating_sub(intermediate) / MIB)
    }

    fn sources_match(&self, identities: &[VolumeIdentity], opened: &[String]) -> bool {
        sources_match(self.plan.as_ref(), self.source_identity.as_ref(), identities, opened)
    }

    fn on_retry_prepared(&mut self, report: Box<FinishedReport>, prep: RetryPrep) {
        match prep {
            RetryPrep::Fail(detail) => self.finalize(report, detail),
            RetryPrep::Prompt { phase, limit, written, intermediate, available, minimum } => {
                if self.close_requested {
                    self.finalize(report, None);
                    return;
                }
                let ui = self.ui();
                ui.set_prompt_detail(
                    format!(
                        "已解出 {:.1} MiB，上限为 {:.1} MiB。磁盘可用 {available} MiB。设置更大的上限后将从头解压。",
                        written as f64 / MIB as f64,
                        limit as f64 / MIB as f64
                    )
                    .into(),
                );
                let suggested = available.min(minimum.max(2 * (limit / MIB)));
                self.prompt = PromptCtx::Budget { phase, limit, intermediate, minimum, report };
                self.show_prompt(PromptKind::Budget, "解压超出本次上限", "重新解压");
                self.ui().set_prompt_input(suggested.to_string().into());
            }
        }
    }

    fn retry(&mut self, phase: Phase, limit: u64) {
        if let Some(worker) = self.worker.take() {
            worker.close_stdin();
        }
        self._job = None;
        match phase {
            Phase::Inner => self.attempt.inner_limit = Some(limit),
            Phase::Outer => self.attempt.outer_limit = Some(limit),
        }
        self.plan = Some(ExtractionPlan::new(&self.archive));
        self.finished_handled = false;
        self.error_shown = false;
        self.size_known = false;
        self.phase = None;
        self.paused = false;
        self.total_bytes = 0;
        self.completed_bytes = 0;
        self.graph = SpeedGraph::default();
        self.started = Instant::now();
        self.meter.reset(0.0, 0);
        let ui = self.ui();
        ui.set_state(TaskState::Running);
        ui.set_controls_enabled(true);
        ui.set_progress(0.0);
        self.spawn_worker();
    }

    fn finalize(&mut self, report: Box<FinishedReport>, budget_detail: Option<String>) {
        let Some(worker) = self.worker.clone() else {
            self.fail_without_commit("工作进程已不可用，已保留源包。", true);
            self.completed = true;
            return;
        };
        let request = FinalizeRequest {
            worker,
            report,
            budget_detail,
            plan: self.plan.clone().expect("plan"),
            manifest_path: self.manifest_path.clone(),
            recycle: self.ui().get_recycle_checked(),
        };
        let generation = self.generation;
        std::thread::spawn(move || {
            let outcome = finalize(request);
            post(Msg::Finalized(generation, outcome));
        });
    }

    fn on_finalized(&mut self, outcome: FinalOutcome) {
        remove_manifest(&self.manifest_path);
        self.completed = true;
        let ui = self.ui();
        match outcome {
            FinalOutcome::Failed { detail, keep_staging } => self.fail_without_commit(&detail, keep_staging),
            FinalOutcome::Delivered { final_dir, notes, partial, recycle } => {
                if let Some(plan) = self.plan.as_mut() {
                    plan.final_dir = final_dir;
                }
                ui.set_finished(true);
                ui.set_can_open_folder(true);
                ui.set_indeterminate(false);
                ui.set_progress(1.0);
                ui.set_detail_eta("完成".into());
                ui.set_detail_remaining("0 (0 B)".into());
                self.graph.push(1.0, self.meter.current, self.meter.displayed);
                ui.set_graph_area(self.graph.area_path(GRAPH_W, GRAPH_H).into());
                ui.set_graph_progress(1.0);
                self.taskbar(TaskbarState::None, 1.0);

                let recycle_problem = recycle.as_ref().is_some_and(|r| !r.success);
                if partial {
                    ui.set_state(TaskState::Warning);
                    self.set_status("部分完成");
                    ui.set_path_info(format!("{notes}；源压缩包已保留。").into());
                } else if let Some(r) = recycle.as_ref().filter(|_| recycle_problem) {
                    ui.set_state(TaskState::Warning);
                    let all_unavailable = !r.items.is_empty() && r.items.iter().all(|i| i.bin_unavailable);
                    ui.set_status_text(if all_unavailable { recycle::BIN_DISABLED } else { "解压完成（源文件回收未完成）" }.into());
                    ui.set_title_text("已完成 100%".into());
                    let failures: Vec<String> = r
                        .items
                        .iter()
                        .filter(|i| !i.recycled)
                        .map(|i| format!("{}：{}", Path::new(&i.path).file_name().unwrap_or_default().to_string_lossy(), i.detail))
                        .collect();
                    let tail =
                        if failures.is_empty() { String::new() } else { format!("（回收未确认：{}）", failures.join("；")) };
                    ui.set_path_info(format!("解压已完成并已逐项校验。{}。{tail}", r.message).into());
                    std::thread::spawn(|| {
                        effects::play_finished_sound();
                        post(Msg::SoundDone(false));
                    });
                } else {
                    ui.set_state(TaskState::Done);
                    self.set_status("已完成 100%");
                    let mut text = notes;
                    if let Some(r) = recycle.filter(|r| r.success) {
                        text.push('；');
                        text.push_str(&r.message);
                    }
                    ui.set_path_info(text.into());
                    std::thread::spawn(|| {
                        effects::play_finished_sound();
                        post(Msg::SoundDone(true));
                    });
                }
            }
        }
        if self.close_requested {
            let _ = slint::quit_event_loop();
        }
    }

    // ----- closing -----

    fn request_close(&mut self) {
        if self.finished_handled && !self.completed {
            self.close_requested = true;
            return;
        }
        if !self.completed {
            if let Some(worker) = self.worker.clone() {
                if self.closing {
                    return;
                }
                self.closing = true;
                worker.send(&WorkerCommand::Cancel);
                let plan = self.plan.clone();
                let manifest = self.manifest_path.clone();
                self.ui().set_status_text("正在取消…".into());
                std::thread::spawn(move || {
                    worker.stop_and_cleanup(plan.as_ref(), &manifest);
                    post(Msg::Stopped);
                });
                return;
            }
            if let Some(plan) = &self.plan {
                plan.cleanup_staging();
            }
            remove_manifest(&self.manifest_path);
        }
        let _ = slint::quit_event_loop();
    }
}

type Spawned = (WorkerHandle, crate::win::job::Job, std::process::ChildStdout, std::process::ChildStderr);

/// Start `unbox --worker --managed …` inside a kill-on-close job and send `start`.
fn spawn_worker_process(
    plan: &ExtractionPlan,
    manifest: &str,
    code_page: u32,
    attempt: Attempt,
    generation: u64,
) -> std::io::Result<Spawned> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command.arg("--worker").arg("--managed");
    command.arg("--archive").arg(&plan.source).arg("--staging").arg(&plan.staging).arg("--manifest").arg(manifest);
    if code_page != 0 {
        command.arg("--cp").arg(code_page.to_string());
    }
    if let Some(limit) = attempt.outer_limit {
        command.arg("--max-output-bytes").arg(limit.to_string());
    }
    if let Some(limit) = attempt.inner_limit {
        command.arg("--max-inner-output-bytes").arg(limit.to_string());
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let job = crate::win::job::Job::new()?;
    let mut child = command.spawn()?;
    if let Err(e) = job.assign(&child) {
        let _ = child.kill();
        return Err(e);
    }
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let worker = WorkerHandle { generation, child: Arc::new(Mutex::new(child)), stdin: Arc::new(Mutex::new(stdin)) };
    worker.send(&WorkerCommand::Start);
    Ok((worker, job, stdout, stderr))
}

fn new_manifest_path(plan: &ExtractionPlan) -> String {
    Path::new(&plan.parent).join(format!(".unbox_manifest_{}.json", plan::random_token())).to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Background steps
// ---------------------------------------------------------------------------

fn sources_match(plan: Option<&ExtractionPlan>, source: Option<&VolumeIdentity>, identities: &[VolumeIdentity], opened: &[String]) -> bool {
    let Some(plan) = plan else { return false };
    let source_ok = source.is_some_and(|s| s.matches(&crate::win::capture_identity(&plan.source)));
    let volumes = ordered_volumes(identities, opened);
    source_ok && !volumes.is_empty() && volumes.iter().all(|v| v.valid && v.matches(&crate::win::capture_identity(&v.path)))
}

/// Volumes in the order the worker opened them, each with its captured identity (an
/// invalid placeholder when none was captured, which keeps that volume).
fn ordered_volumes(identities: &[VolumeIdentity], opened: &[String]) -> Vec<VolumeIdentity> {
    let mut seen: Vec<String> = Vec::new();
    opened
        .iter()
        .filter(|p| {
            let key = unbox_core::fold(p);
            let fresh = !seen.contains(&key);
            seen.push(key);
            fresh
        })
        .map(|path| {
            identities
                .iter()
                .find(|i| unbox_core::eq_ignore_case(&i.path, path))
                .cloned()
                .unwrap_or_else(|| VolumeIdentity { path: path.clone(), ..Default::default() })
        })
        .collect()
}

fn prepare_retry(
    worker: &WorkerHandle,
    plan: &ExtractionPlan,
    source: Option<&VolumeIdentity>,
    manifest: &str,
    report: &FinishedReport,
) -> RetryPrep {
    if worker.wait_exit(Duration::from_secs(15)) != Some(10) {
        return RetryPrep::Fail(None);
    }
    if !sources_match(Some(plan), source, &report.volume_identities, &report.opened_volumes) {
        return RetryPrep::Fail(None);
    }
    worker.close_stdin();
    plan.cleanup_staging();
    remove_manifest(manifest);
    if Path::new(&plan.staging).exists() || Path::new(manifest).exists() {
        return RetryPrep::Fail(None);
    }
    let Some(free) = crate::win::disk_free(Path::new(&plan.parent)) else {
        return RetryPrep::Fail(Some("无法查询目标目录的可用空间。请检查连接和目录访问权限后重新打开源包。".into()));
    };
    let available = free.saturating_sub(report.intermediate_bytes) / MIB;
    let minimum = report.limit_bytes / MIB + 1;
    if minimum > available {
        return RetryPrep::Fail(Some("目标目录的可用空间不足以提高解压上限。请释放空间后重新打开源包。".into()));
    }
    RetryPrep::Prompt {
        phase: report.phase,
        limit: report.limit_bytes,
        written: report.written_bytes,
        intermediate: report.intermediate_bytes,
        available,
        minimum,
    }
}

struct FinalizeRequest {
    worker: WorkerHandle,
    report: Box<FinishedReport>,
    budget_detail: Option<String>,
    plan: ExtractionPlan,
    manifest_path: String,
    recycle: bool,
}

/// Wait for the worker's exit code, check the finish report against it, then deliver.
fn finalize(request: FinalizeRequest) -> FinalOutcome {
    let FinalizeRequest { worker, report, budget_detail, plan, manifest_path, recycle } = request;
    let exit = worker.wait_exit(Duration::from_secs(15));
    if exit.is_none() {
        worker.send(&WorkerCommand::Cancel);
        worker.stop_and_cleanup(Some(&plan), &manifest_path);
    }
    let code = exit.unwrap_or(-1);
    let status_text = match report.status {
        FinishStatus::Ok => "ok",
        FinishStatus::Partial => "partial",
        FinishStatus::Failed => "failed",
    };
    let exit_matches = report.status != FinishStatus::Failed && exit == Some(report.status.exit_code());
    if exit.is_none() || !exit_matches || !report.manifest_ok || !report.error_reason.is_empty() {
        let detail = match budget_detail {
            Some(detail) => format!("{detail} 源压缩包已保留。"),
            None => describe_failure(&report.error_reason, code, status_text),
        };
        return FinalOutcome::Failed { detail, keep_staging: exit.is_none() };
    }
    if !unbox_core::eq_ignore_case(&report.manifest, &manifest_path) {
        return FinalOutcome::Failed {
            detail: "工作进程返回的交付清单路径与本次任务不符，已保留源包。".into(), keep_staging: true
        };
    }

    let partial = report.status == FinishStatus::Partial || report.skipped_items > 0 || report.unsafe_items > 0 || report.missing_items > 0;
    let manifest = match std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string()).and_then(|t| Manifest::parse(&t)) {
        Ok(m) if m.delivery_root == report.delivery_root => m,
        Ok(_) => {
            return FinalOutcome::Failed {
                detail: "完成处理异常，已保留源包和可用的暂存内容：交付清单与完成消息的交付根不一致".into(),
                keep_staging: true,
            }
        }
        Err(e) => {
            return FinalOutcome::Failed {
                detail: format!("完成处理异常，已保留源包和可用的暂存内容：{e}"), keep_staging: true
            }
        }
    };
    let result = complete::complete(complete::CompletionRequest {
        plan,
        delivery_root: report.delivery_root.clone(),
        manifest,
        recycle: recycle && !partial,
        volumes: ordered_volumes(&report.volume_identities, &report.opened_volumes),
    });
    if !result.delivered {
        let mut detail = result.failure;
        if !result.kept_staging.is_empty() {
            detail.push_str(&format!("\n暂存目录已保留以便恢复：{}", result.kept_staging));
        }
        return FinalOutcome::Failed { keep_staging: !result.kept_staging.is_empty(), detail };
    }

    let mut notes = result.summary;
    if report.policy_filtered_items > 0 {
        notes.push_str(&format!("；已按产品规则过滤 {} 个 macOS 元数据项", report.policy_filtered_items));
    }
    if report.skipped_items > 0 {
        notes.push_str(&format!("；{} 项被跳过", report.skipped_items));
    }
    if report.unsafe_items > 0 {
        notes.push_str(&format!("；{} 项因路径不安全被拒绝", report.unsafe_items));
    }
    if report.missing_items > 0 {
        notes.push_str(&format!("；{} 项未交付（计划条目缺失）", report.missing_items));
    }
    if report.empty_archive {
        notes = "归档为空，已创建空目标目录".into();
    }
    if result.zone_marked > 0 {
        notes.push_str(&format!("；已为 {} 个文件保留下载来源标记", result.zone_marked));
    }
    FinalOutcome::Delivered { final_dir: result.final_dir, notes, partial, recycle: result.recycle }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn settings_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("UnzipProxy").join("settings.json")
}

fn pick_archive() -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("选择要解压的压缩包");
    for (label, extensions) in unbox_core::catalog::picker_filters() {
        dialog = dialog.add_filter(label, &extensions);
    }
    dialog.add_filter("所有文件", &["*"]).pick_file()
}

pub fn run(archive: Option<std::ffi::OsString>) -> i32 {
    let archive = match archive.map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()) {
        Some(path) => path,
        None => match pick_archive() {
            Some(path) => path,
            None => return 0,
        },
    };
    // The UI thread is a single-threaded apartment (taskbar progress, Shell dialogs).
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    }
    let settings_path = settings_path();
    let settings = UserSettings::load(&settings_path);

    let ui = match AppWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("无法创建窗口：{e}");
            return 1;
        }
    };
    ui.set_expanded(settings.is_details_expanded);
    ui.set_recycle_checked(settings.recycle_on_success);

    let controller = Rc::new(RefCell::new(Controller {
        ui: ui.as_weak(),
        archive,
        plan: None,
        worker: None,
        _job: None,
        generation: 0,
        manifest_path: String::new(),
        settings,
        settings_path,
        attempt: Attempt::default(),
        source_identity: None,
        taskbar: None,
        hwnd: None,
        paused: false,
        completed: false,
        finished_handled: false,
        error_shown: false,
        closing: false,
        close_requested: false,
        total_bytes: 0,
        completed_bytes: 0,
        total_items: 1,
        size_known: false,
        phase: None,
        started: Instant::now(),
        meter: SpeedMeter::default(),
        graph: SpeedGraph::default(),
        prompt: PromptCtx::None,
    }));
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

    ui.on_pause_clicked(|| with_controller(|c| c.pause_resume()));
    ui.on_cancel_clicked(|| with_controller(|c| c.request_close()));
    ui.on_close_clicked(|| with_controller(|c| c.request_close()));
    ui.on_toggle_details(|| with_controller(|c| c.toggle_details()));
    ui.on_recycle_toggled(|checked| with_controller(|c| c.recycle_toggled(checked)));
    ui.on_open_folder(|| with_controller(|c| c.open_folder()));
    ui.on_prompt_accepted(|| with_controller(|c| c.prompt_accepted()));
    ui.on_prompt_dismissed(|| with_controller(|c| c.prompt_dismissed()));
    ui.window().on_close_requested(|| {
        with_controller(|c| c.request_close());
        slint::CloseRequestResponse::KeepWindowShown
    });

    // Slint applies the animated min/max height as window constraints; this keeps the
    // native window in step even if a platform ignores constraint-driven resizes.
    let sizer = slint::Timer::default();
    let weak = ui.as_weak();
    sizer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            let wanted = ui.get_shown_height();
            let current = window.size().to_logical(window.scale_factor()).height;
            if (current - wanted).abs() > 0.5 {
                window.set_size(slint::LogicalSize::new(480.0, wanted));
            }
        }
    });

    let ticker = slint::Timer::default();
    ticker.start(slint::TimerMode::Repeated, Duration::from_millis(100), || with_controller(|c| c.tick()));

    if let Err(e) = ui.show() {
        eprintln!("无法显示窗口：{e}");
        return 1;
    }
    // Native polish needs the HWND, which exists once the event loop has started.
    slint::Timer::single_shot(Duration::from_millis(0), || {
        with_controller(|c| {
            let ui = c.ui();
            if let Some(hwnd) = effects::hwnd_of(ui.window()) {
                effects::polish_window(hwnd, effects::system_uses_dark_theme());
                c.hwnd = Some(hwnd);
                c.taskbar = Some(Taskbar::new(hwnd));
            }
            ui.set_appeared(true);
            c.start();
        });
    });

    let _ = slint::run_event_loop_until_quit();
    // Leaving the process closes the job object, which ends a stray worker.
    CONTROLLER.with(|c| c.borrow_mut().take());
    0
}
