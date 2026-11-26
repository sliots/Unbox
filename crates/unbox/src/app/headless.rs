//! `unbox extract <archive>`: the same plan → worker → verify → commit pipeline as the
//! window, answered without prompts. Used for scripting and by CI as an end-to-end test.

use super::*;

const USAGE: &str =
    "用法: unbox extract <压缩包> [--password <密码>] [--skip-invalid] [--cp <代码页>] [--recycle] [--here] [--to <文件夹>]";

struct Options {
    archive: Option<PathBuf>,
    password: Option<String>,
    skip_invalid: bool,
    code_page: u32,
    recycle: bool,
    here: bool,
    to: Option<PathBuf>,
}

fn parse(args: &[std::ffi::OsString]) -> Result<Options, String> {
    let mut options = Options { archive: None, password: None, skip_invalid: false, code_page: 0, recycle: false, here: false, to: None };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--password") => options.password = Some(iter.next().ok_or(USAGE)?.to_string_lossy().into_owned()),
            Some("--cp") => {
                options.code_page = iter.next().and_then(|v| v.to_str()?.parse().ok()).ok_or(USAGE)?;
            }
            Some("--skip-invalid") => options.skip_invalid = true,
            Some("--recycle") => options.recycle = true,
            Some("--here") => options.here = true,
            Some("--to") => options.to = Some(PathBuf::from(iter.next().ok_or(USAGE)?)),
            _ if options.archive.is_none() => options.archive = Some(PathBuf::from(arg)),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(options)
}

/// Exit codes: 0 delivered, 2 partially delivered, 1 failed.
pub fn extract_cli(args: &[std::ffi::OsString]) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let Some(archive) = options.archive.filter(|p| p.is_file()) else {
        eprintln!("找不到压缩包。\n{USAGE}");
        return 1;
    };
    let archive = plan::first_volume(&archive);
    let plan = ExtractionPlan::create(&archive, options.to.as_deref(), options.here);
    if let Err(e) = std::fs::create_dir_all(&plan.staging) {
        eprintln!("无法创建暂存目录：{e}");
        return 1;
    }
    let manifest = new_manifest_path(&plan);
    let (worker, _job, stdout, stderr) = match spawn_worker_process(&plan, &manifest, options.code_page, Attempt::default(), 1) {
        Ok(spawned) => spawned,
        Err(e) => {
            plan.cleanup_staging();
            eprintln!("无法启动解压工作进程：{e}");
            return 1;
        }
    };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
    });

    let fail = |message: &str| {
        worker.send(&WorkerCommand::Cancel);
        worker.stop_and_cleanup(Some(&plan), &manifest);
        eprintln!("解压失败：{message}");
        1
    };
    let mut password_sent = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(event) = serde_json::from_str::<WorkerEvent>(line.trim()) else { continue };
        match event {
            WorkerEvent::InvalidCharsFound { items } => {
                let action = if options.skip_invalid { InvalidCharsAction::Skip } else { InvalidCharsAction::BatchRename };
                eprintln!(
                    "{} 个文件名包含 Windows 不支持的字符，已{}。",
                    items.len(),
                    if options.skip_invalid { "跳过" } else { "批量重命名" }
                );
                worker.send(&WorkerCommand::InvalidCharsResponse { action });
            }
            WorkerEvent::EncodingOptions { options: choices } => {
                let cp = choices.first().map(|o| o.cp).unwrap_or(0);
                eprintln!("文件名编码不确定，按 {} 解码。", unbox_core::decode::code_page_label(cp));
                worker.send(&WorkerCommand::EncodingResponse { cp });
            }
            // An inner archive nobody can open is delivered as it is.
            WorkerEvent::PasswordRequest { inner: true, .. } if options.password.is_none() || password_sent => {
                worker.send(&WorkerCommand::PasswordSkip);
            }
            WorkerEvent::PasswordRequest { .. } => match (&options.password, password_sent) {
                (Some(password), false) => {
                    password_sent = true;
                    worker.send(&WorkerCommand::PasswordResponse { password: password.clone() });
                }
                (Some(_), true) => return fail("密码错误"),
                (None, _) => return fail("压缩包需要密码，自动尝试的密码都不对（使用 --password 提供）"),
            },
            WorkerEvent::Error { message, error_reason } => {
                let message = match error_reason {
                    Some(r) if r == "insufficient_disk_space" => format!("{message}{}", describe_failure(&r, -1, "failed")),
                    Some(r) => describe_failure(&r, -1, "failed"),
                    None => message,
                };
                return fail(&message);
            }
            WorkerEvent::Canceled => return fail("已取消"),
            WorkerEvent::Finished(report) => {
                let outcome = finalize(FinalizeRequest {
                    worker: worker.clone(),
                    report,
                    budget_detail: None,
                    plan: plan.clone(),
                    manifest_path: manifest.clone(),
                    recycle: options.recycle,
                });
                remove_manifest(&manifest);
                return match outcome {
                    FinalOutcome::Failed { detail, keep_staging, .. } => {
                        if !keep_staging {
                            plan.cleanup_staging();
                        }
                        eprintln!("解压未完成：{detail}");
                        1
                    }
                    FinalOutcome::Delivered { final_dir, notes, partial, recycle, .. } => {
                        println!("{final_dir}");
                        eprintln!("{notes}");
                        if let Some(r) = recycle {
                            eprintln!("{}", r.message);
                        }
                        if partial {
                            2
                        } else {
                            0
                        }
                    }
                };
            }
            WorkerEvent::Init { .. }
            | WorkerEvent::PasswordProbing { .. }
            | WorkerEvent::Progress { .. }
            | WorkerEvent::PausedAck
            | WorkerEvent::ResumedAck => {}
        }
    }
    let code = worker.wait_exit(Duration::from_secs(5)).unwrap_or(-1);
    fail(&format!("工作进程异常退出（退出代码 {code}）"))
}
