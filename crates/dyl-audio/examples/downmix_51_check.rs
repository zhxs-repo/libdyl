//! 5.1 → 立体声下混的实听验证：确认中置（人声）真的折进了输出。
//!
//! 用法：
//! ```text
//! cargo run -p dyl-audio --example downmix_51_check -- <5.1 素材> [输出目录]
//! ```
//!
//! 产出一对同源的 WAV，便于逐点对比听感：
//! - `downmix_new.wav`：当前 `AudioConverter`（ITU-R BS.775 系数 + 软限幅）
//! - `downmix_legacy.wav`：改造前的声道映射（取前 N 个源声道，`c % src_ch`）
//!
//! 两者由同一份解码 PCM 生成，所以任何差异都只来自下混规则本身。
//! 终端会直接打印中置相关性与电平，非零相关即"人声回来了"。

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dyl_audio::AudioConverter;
use dyl_ffmpeg::{AudioDecoder, Demuxer, FfmpegAudioDecoder, FfmpegDemuxer, TrackSelection};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or("用法: downmix_51_check <5.1 素材> [输出目录]")?;
    let out_dir = args.next().unwrap_or_else(|| ".".to_string());

    let mut demuxer = FfmpegDemuxer::new();
    demuxer
        .open(
            &input,
            &TrackSelection {
                video: None,
                audio: None,
                subtitle: None,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .map_err(|e| format!("open {input}: {e}"))?;
    let info = demuxer.info().cloned().ok_or("没有媒体信息")?;
    let audio_idx = info.audio_stream_index.ok_or("没有音频轨")?;
    let ctx = demuxer
        .take_audio_codec_context()
        .ok_or("没有音频解码上下文")?;
    let mut decoder =
        FfmpegAudioDecoder::from_stream_context(ctx).map_err(|e| format!("打开音频解码器: {e}"))?;

    println!(
        "素材: {} Hz / {} 声道 / 编码 {:?}",
        info.sample_rate, info.channels, info.audio_codec_id
    );
    if info.channels != 6 {
        return Err(format!("这个素材是 {} 声道，本检查需要 5.1", info.channels));
    }

    // 目标设备：立体声、与源同采样率（把变量收敛到"只测下混"）
    let mut converter = AudioConverter::new(info.sample_rate, 6, info.sample_rate, 2);
    let mut new_out: Vec<f32> = Vec::new();
    let mut legacy_out: Vec<f32> = Vec::new();
    let mut center: Vec<f32> = Vec::new();
    let mut scratch = Vec::new();
    let mut frames_in = 0usize;

    let mut drain = |decoder: &mut FfmpegAudioDecoder,
                     new_out: &mut Vec<f32>,
                     legacy_out: &mut Vec<f32>,
                     center: &mut Vec<f32>,
                     scratch: &mut Vec<f32>|
     -> Result<usize, String> {
        let mut n = 0;
        // 排空阶段 ffmpeg 用错误码（AVERROR_EOF）表示"没有更多帧"而不是 None，
        // 与 pipeline 的 drain 一致：把它当作正常收尾
        //
        // 不能用 `while let`：错误分支要区分"EOF 收尾"和"真出错"，纯 while let
        // 会把两者一起吞掉
        #[allow(clippy::while_let_loop)]
        loop {
            let frame = match decoder.receive_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    if !e.to_string().contains("541478725") {
                        eprintln!("解码中止: {e}");
                    }
                    break;
                }
            };
            let ch = frame.channels.max(1) as usize;
            if ch != 6 {
                return Err(format!("解码帧是 {ch} 声道，不是 5.1"));
            }
            let src_frames = frame.samples.len() / ch;
            n += src_frames;

            // 当前实现
            converter.convert(&frame, 1000, scratch);
            new_out.extend_from_slice(scratch);

            // 改造前的映射：设备声道 c 取源声道 c % src_ch，即只取 FL/FR
            for i in 0..src_frames {
                legacy_out.push(frame.samples[i * ch]);
                legacy_out.push(frame.samples[i * ch + 1]);
            }
            // 中置真值（5.1 顺序 FL FR FC LFE BL BR，索引 2）
            for i in 0..src_frames {
                center.push(frame.samples[i * ch + 2]);
            }
        }
        Ok(n)
    };

    while let Some(pkt) = demuxer.read_packet().map_err(|e| format!("read: {e}"))? {
        if pkt.stream_index != audio_idx {
            continue;
        }
        decoder
            .submit_packet(&pkt)
            .map_err(|e| format!("submit: {e}"))?;
        frames_in += drain(
            &mut decoder,
            &mut new_out,
            &mut legacy_out,
            &mut center,
            &mut scratch,
        )?;
    }
    decoder.send_eof().map_err(|e| format!("eof: {e}"))?;
    frames_in += drain(
        &mut decoder,
        &mut new_out,
        &mut legacy_out,
        &mut center,
        &mut scratch,
    )?;

    if frames_in == 0 {
        return Err("没有解出任何音频帧".into());
    }

    // 三路对齐到共同长度（下混器有固定的右侧上下文延迟，末尾会短几十个采样）
    let n = new_out.len().min(legacy_out.len()) / 2;
    let new_l: Vec<f32> = (0..n).map(|i| new_out[i * 2]).collect();
    let legacy_l: Vec<f32> = (0..n).map(|i| legacy_out[i * 2]).collect();
    let c = &center[..n];

    println!(
        "\n解码 {frames_in} 源帧 ≈ {:.2} s",
        frames_in as f64 / info.sample_rate as f64
    );
    println!("{:<28}{:>10}{:>14}", "", "RMS", "与中置相关性");
    println!(
        "{:<28}{:>10.5}{:>14.4}",
        "当前实现（左声道）",
        rms(&new_l),
        corr(&new_l, c)
    );
    println!(
        "{:<28}{:>10.5}{:>14.4}",
        "改造前（取 FL/FR）",
        rms(&legacy_l),
        corr(&legacy_l, c)
    );
    println!("{:<28}{:>10.5}{:>14.4}", "中置真值", rms(c), 1.0);
    println!(
        "{:<28}{:>10.4}",
        "中置折入增益（期望 0.707）",
        gain(&new_l, c)
    );

    write_wav(
        &format!("{out_dir}/downmix_new.wav"),
        &new_out[..n * 2],
        info.sample_rate,
        2,
    )?;
    write_wav(
        &format!("{out_dir}/downmix_legacy.wav"),
        &legacy_out[..n * 2],
        info.sample_rate,
        2,
    )?;
    write_wav(
        &format!("{out_dir}/center_truth.wav"),
        c,
        info.sample_rate,
        1,
    )?;
    println!("\n已写出 downmix_new.wav / downmix_legacy.wav / center_truth.wav 到 {out_dir}");
    Ok(())
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Pearson 相关系数：输出里有多少"中置的内容"
fn corr(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let (ma, mb) = (
        a.iter().map(|v| *v as f64).sum::<f64>() / n as f64,
        b.iter().map(|v| *v as f64).sum::<f64>() / n as f64,
    );
    let mut num = 0.0;
    let mut da = 0.0;
    let mut db = 0.0;
    for i in 0..n {
        let (x, y) = (a[i] as f64 - ma, b[i] as f64 - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da <= 0.0 || db <= 0.0 {
        0.0
    } else {
        num / (da.sqrt() * db.sqrt())
    }
}

/// 输出相对中置的线性增益（最小二乘意义）
fn gain(out: &[f32], center: &[f32]) -> f64 {
    let n = out.len().min(center.len());
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..n {
        num += out[i] as f64 * center[i] as f64;
        den += center[i] as f64 * center[i] as f64;
    }
    if den <= 0.0 {
        0.0
    } else {
        num / den
    }
}

/// 16-bit PCM WAV
fn write_wav(
    path: &str,
    interleaved: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    let data_len = interleaved.len() * 2;
    let mut f = std::fs::File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    let byte_rate = sample_rate * channels as u32 * 2;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&(channels * 2).to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_len as u32).to_le_bytes());
    f.write_all(&header).map_err(|e| e.to_string())?;
    let mut body = Vec::with_capacity(data_len);
    for s in interleaved {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        body.extend_from_slice(&v.to_le_bytes());
    }
    f.write_all(&body).map_err(|e| e.to_string())?;
    Ok(())
}
