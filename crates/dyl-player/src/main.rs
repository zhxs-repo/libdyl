//! Dyl 桌面壳 — winit + wgpu + egui host compositor。
//!
//! 用法：
//!   dyl-player <视频> [--subtitle <字幕>] [--danmaku <弹幕>] [--no-play]

mod app;
mod gpu;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

struct Args {
    video: Option<PathBuf>,
    subtitle: Option<PathBuf>,
    danmaku: Option<PathBuf>,
    autoplay: bool,
}

fn main() -> ExitCode {
    init_logging();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("dyl-player: {error}");
            print_usage();
            return ExitCode::FAILURE;
        }
    };

    let event_loop = match EventLoop::<app::UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("dyl-player: create event loop: {error}");
            return ExitCode::FAILURE;
        }
    };
    let proxy = event_loop.create_proxy();

    let startup = app::Startup {
        video: args.video,
        subtitle: args.subtitle,
        danmaku: args.danmaku,
        autoplay: args.autoplay,
    };
    let mut app = match app::App::new(startup, proxy) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("dyl-player: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("dyl-player: event loop: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,dyl_player=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args {
        video: None,
        subtitle: None,
        danmaku: None,
        autoplay: true,
    };
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--no-play" => parsed.autoplay = false,
            "--subtitle" => {
                parsed.subtitle = Some(PathBuf::from(expect_value(&mut args, "--subtitle")?));
            }
            "--danmaku" => {
                parsed.danmaku = Some(PathBuf::from(expect_value(&mut args, "--danmaku")?));
            }
            other if other.starts_with('-') => {
                return Err(format!("未知选项: {other}"));
            }
            other => {
                if parsed.video.is_some() {
                    return Err("一次只能指定一个视频文件".to_string());
                }
                parsed.video = Some(PathBuf::from(other));
            }
        }
    }

    Ok(Some(parsed))
}

fn expect_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{option} 需要一个路径参数"))
}

fn print_usage() {
    println!(
        "用法: dyl-player [选项] [视频路径]\n\
         \n\
         选项:\n\
           --subtitle <路径>   加载外挂字幕 (SRT/ASS/SSA)\n\
           --danmaku <路径>    加载 B 站 XML 弹幕\n\
           --no-play           加载完成后不自动播放\n\
           -h, --help          显示帮助\n\
         \n\
         窗口快捷键: 空格 播放/暂停 · ←/→ 快退/快进 5s · F 全屏 · M 静音 · H 帮助 · Esc 退出"
    );
}
