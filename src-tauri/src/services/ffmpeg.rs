use tokio::{fs, io::{self, AsyncBufReadExt, AsyncSeekExt}, time::sleep};
use std::{io::SeekFrom, path::PathBuf, sync::Arc, time::Duration};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tauri::{ipc::Channel, Manager};
use tauri_plugin_shell::ShellExt;
use regex::Regex;

use super::aria2c::{
    Task,
    TaskType,
    QueueInfo,
    DownloadEvent,
};

use crate::{shared::{get_app_handle, process_err, get_ts}, TauriError, TauriResult};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FFmpegLog {
    frame: u64,
    fps: f64,
    stream_0_0_q: f64,
    bitrate: String,
    total_size: u64,
    out_time_us: u64,
    out_time_ms: u64,
    out_time: String,
    dup_frames: u32,
    drop_frames: u32,
    speed: String,
    progress: String,
}

pub async fn get_stream_info(video: PathBuf, audio: PathBuf) -> Result<(u64, String)> {
    let app = get_app_handle();
    let meta_output = app.shell().sidecar("ffmpeg")?
        .args([
            "-i", video.to_str().unwrap(),
            "-i", audio.to_str().unwrap(),
            "-map", "0:v:0",
            "-map", "1:a:0",
            "-c", "copy",
            "-f", "null", "-",
        ])
        .output()
        .await?;

    let stderr = String::from_utf8_lossy(&meta_output.stderr);
    log::info!("FFmpeg stderr:\n{}", &stderr);

    let video_frames = Regex::new(r"frame=\s*(\d+)")?
        .captures_iter(&stderr)
        .filter_map(|caps| caps.get(1)?.as_str().trim().parse::<u64>().ok())
        .max().ok_or(anyhow!("Failed to parse video frame count"))?;

    let audio_codec = Regex::new(r"Audio:\s*([^,]+)")?
        .captures_iter(&stderr)
        .filter_map(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_string())
        .next().ok_or(anyhow!("Failed to parse audio codec"))?;

    Ok((video_frames, audio_codec))
}

pub async fn merge(info: Arc<QueueInfo>, event: &Channel<DownloadEvent>) -> TauriResult<()> {
    if info.tasks.len() < 2 {
        return Err(anyhow!("Insufficient number of input paths, {}", info.tasks.len()).into());
    }
    let paths = {
        let _paths: Vec<_> = info.tasks.iter().map(|v| v.path.clone()).collect();
        Arc::new(_paths)
    };
    let output = info.output.clone();
    let video_path = info.tasks.iter()
        .find(|v| v.task_type == TaskType::Video)
        .map(|v| v.path.clone()).unwrap().unwrap();

    let audio_path = info.tasks.iter()
        .find(|v| v.task_type == TaskType::Audio)
        .map(|v| v.path.clone()).unwrap().unwrap();

    let app = get_app_handle();
    let progress_path = app.path().app_log_dir()?
        .join("ffmpeg").join(format!("{}_{}.log", info.id, get_ts(true)));

    let progress_path_clone = progress_path.clone();
    fs::create_dir_all(progress_path.parent().unwrap()).await
        .context("Failed to create FFmpeg progress Folder")?;

    let stream_info = get_stream_info(video_path, audio_path).await
        .context("Failed to get stream info")
        .map_err(|e| process_err(TauriError::from(e), "ffmpeg"))?;

    let ext = output.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let codec = match ext {
        "mp4" | "flv" => match stream_info.1.as_str() {
            "aac" | "mp3" => "copy",
            _ => "aac",
        },
        _ => "copy",
    };
    let ffmpeg = async {
        let result = app.shell().sidecar("ffmpeg")?
            .args([
                "-i", paths[0].clone().unwrap().to_str().unwrap(),
                "-i", paths[1].clone().unwrap().to_str().unwrap(),
                "-c:v", "copy",
                "-c:a", codec,
                "-shortest",
                &output.to_str().unwrap(), "-progress",
                progress_path_clone.to_str().unwrap(), "-y"
            ]).output().await?;

        log::info!("STDOUT: {:?}", String::from_utf8_lossy(&result.stdout));
        log::info!("STDERR: {:?}", String::from_utf8_lossy(&result.stderr));
        if result.status.success() {
            Ok::<(), anyhow::Error>(())
        } else {
            Err(anyhow!("FFmpeg exited with status: {}", result.status.code().unwrap_or(-1)))
        }
    };
    let monitor = async {
        let task = info.tasks.iter().find(|v| v.task_type == TaskType::Video).unwrap();
        monitor(info.id.clone(), task, progress_path, stream_info.0, event).await?;
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::try_join!(ffmpeg, monitor);
    result.context("Failed to merge")?;
    Ok(())
}

pub async fn raw_flac(info: Arc<QueueInfo>) -> Result<()> {
    let output = info.output.clone();
    let input_path = info.tasks.iter()
        .find(|task| task.task_type == TaskType::Audio)
        .map(|task| task.path.clone()).unwrap();

    let app = get_app_handle();
    let status = app.shell().sidecar("ffmpeg")?
        .args([
            "-i", input_path.unwrap().to_str().unwrap(),
            "-vn", "-acodec", "flac",
            &output.to_str().unwrap()
        ])
        .status().await?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("FFmpeg exited with status: {}", status.code().unwrap_or(-1)))
    }
}

async fn monitor(id: String, task: &Task, progress_path: PathBuf, frames: u64, event: &Channel<DownloadEvent>) -> Result<()> {
    while !progress_path.exists() {
        sleep(Duration::from_millis(250)).await;
    }
    let gid = Arc::new(task.gid.as_ref().unwrap_or(&String::new()).clone());
    let id = Arc::new(id);
    event.send(DownloadEvent::Started { id: id.clone(), gid, task_type: TaskType::Merge })?;
    let mut last_size = 0u64;
    let mut map = Map::new();
    let mut keys = Vec::new();
    loop {
        let gid = Arc::new(task.gid.as_ref().unwrap_or(&String::new()).clone());
        let metadata = fs::metadata(&progress_path)
            .await.with_context(||
                format!("Failed to get FFmpeg progress metadata: {}", &progress_path.to_string_lossy())
            )?;
        let mut file = fs::File::open(&progress_path)
            .await.with_context(||
                format!("Failed to open FFmpeg progress: {}", &progress_path.to_string_lossy())
            )?;
        let _ = file.seek(SeekFrom::Start(last_size)).await;
        let mut reader = io::BufReader::new(file);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            if let Some((key, value)) = line.trim().split_once('=') {
                let key = key.trim().to_string();
                let value = value.trim();
                if keys.len() >= 12 {
                    if let Some(oldest_key) = keys.remove(0) {
                        map.remove(&oldest_key);
                    }
                }
                let value = if let Ok(number) = value.parse::<u64>() {
                    json!(number)
                } else if let Ok(number) = value.parse::<f64>() {
                    json!(number)
                } else {
                    json!(value)
                };
                map.insert(key.clone(), value);
                keys.push(Some(key));
            }
            line.clear();
        }
        last_size = metadata.len();
        if map.is_empty() { continue; }
        let log_data: FFmpegLog = serde_json::from_value(Value::Object(map.clone()))
            .context("Failed to parse FFmpeg Log")?;
        match log_data.progress.as_str() {
            "continue" => {
                event.send(DownloadEvent::Progress {
                    id: id.clone(),
                    gid: gid.clone(),
                    content_length: frames,
                    chunk_length: log_data.frame,
                })?;
            },
            "end" => {
                event.send(DownloadEvent::Finished {
                    id: id.clone(),
                    gid: gid.clone(),
                })?;
                return Ok(());
            }
            _ => {},
        }
        sleep(Duration::from_millis(200)).await;
    }
}