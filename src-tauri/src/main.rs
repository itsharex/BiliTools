// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use lazy_static::lazy_static;
use serde_json::Value;
use reqwest::{Client, header, header::{HeaderMap, HeaderValue}, Url, cookie::{CookieStore, Jar}};
use warp::{Filter, Reply, http::Response, path::FullPath, hyper::Method, hyper::body::Bytes};
use std::{env, fs, path::{Path, PathBuf}, sync::{Arc, RwLock}, convert::Infallible, time::Instant, process::Stdio, collections::{VecDeque, HashSet, HashMap}};
use tokio::{fs::File, sync::Mutex, io::{AsyncWriteExt, AsyncBufReadExt, AsyncSeekExt, SeekFrom, BufReader}, process::Command, time::{sleep, Duration}};
use futures::stream::StreamExt;

lazy_static! {
    static ref GLOBAL_COOKIE_JAR: Arc<RwLock<Jar>> = Arc::new(RwLock::new(Jar::default()));
    static ref STOP_LOGIN: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    static ref DOWNLOAD_INFO_MAP: Mutex<HashMap<String, VideoDownloadInfo>> = Mutex::new(HashMap::new());
    static ref DOWNLOAD_DIRECTORY: PathBuf = {
        PathBuf::from(env::var("USERPROFILE").expect("USERPROFILE environment variable not found"))
        .join("Desktop")
    };
    static ref TEMP_DIRECTORY: PathBuf = {
        PathBuf::from(env::var("APPDATA").expect("USERPROFILE environment variable not found"))
        .join("com.btjawa.biliget").join("Temp")
    };
}

fn init_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/118.0.0.0 Safari/537.36"));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.5"));
    headers.insert("Range", HeaderValue::from_static("bytes=0-"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert("Referer", HeaderValue::from_static("https://www.bilibili.com"));
    headers
}

fn init_client() -> Client {
    Client::builder()
        .default_headers(init_headers())
        .cookie_provider(Arc::new(ThreadSafeCookieStore(Arc::clone(&GLOBAL_COOKIE_JAR))))
        .build()
        .unwrap()
}

fn extract_filename(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed_url| {
            parsed_url.path_segments()
                .and_then(|segments| segments.last())
                .map(|last_segment| last_segment.to_string())
        })
        .unwrap_or_else(|| "default_filename".to_string())
}

struct ThreadSafeCookieStore(Arc<RwLock<Jar>>);

#[derive(Debug, Clone)]
struct VideoDownloadInfo {
    cid: String,
    video_path: PathBuf,
    audio_path: PathBuf,
    video_downloaded: bool,
    audio_downloaded: bool,
    finished: bool
}

#[derive(Debug, Clone)]
struct DownloadTask {
    cid: String,
    display_name: String,
    url: String,
    path: PathBuf,
    file_type: String,
}

type DownloadQueue = Vec<DownloadTask>;

impl CookieStore for ThreadSafeCookieStore {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let jar = self.0.write().unwrap();
        jar.set_cookies(cookie_headers, url);
    }
    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let jar = self.0.read().unwrap();
        jar.cookies(url)
    }
}

#[tauri::command]
async fn init_download_multi(
    window: tauri::Window, 
    video_url: String, audio_url: String,
    cid: String, display_name: String,
) {
    println!("{}, {}, {}, {}", video_url, audio_url, cid, display_name);
    let video_filename = extract_filename(&video_url);
    let audio_filename = extract_filename(&audio_url);
    let video_path = TEMP_DIRECTORY.join(&video_filename);
    let audio_path = TEMP_DIRECTORY.join(&audio_filename);
    println!("{}, {}, {:?}, {:?}", video_filename, audio_filename, video_path, audio_path);
    let download_info = VideoDownloadInfo {
        cid: cid.clone(),
        video_path: video_path.clone(),
        audio_path: audio_path.clone(),
        video_downloaded: false,
        audio_downloaded: false,
        finished: false
    };
    println!("{:?}", download_info);
    DOWNLOAD_INFO_MAP.lock().await.insert(display_name.clone(), download_info); // 插入新info
    let video_task = DownloadTask { cid: cid.clone(), display_name: display_name.clone(), url: video_url, path: video_path, file_type: "video".to_string() };
    let audio_task = DownloadTask { cid: cid.clone(), display_name: display_name.clone(), url: audio_url, path: audio_path, file_type: "audio".to_string() };
    process_download_queue(window, video_task, audio_task, "multi".to_string()).await;
}

async fn process_download_queue(window: tauri::Window, video_task: DownloadTask, audio_task: DownloadTask, action: String) {
    let mut queue = DownloadQueue::new();
    queue.push(audio_task);
    queue.push(video_task);
    while let Some(task) = queue.pop() {
        download_file(window.clone(), task.clone(), action.clone()).await;
        let mut info = DOWNLOAD_INFO_MAP.lock().await;
        if let Some(download_info) = info.get_mut(&task.display_name) {
            if task.file_type == "video" {
                if download_info.cid == task.cid {
                    download_info.video_downloaded = true;
                }
            } else if task.file_type == "audio" {
                if download_info.cid == task.cid {
                    download_info.audio_downloaded = true;
                }
            println!("{:?}", *download_info);
            }
            if download_info.video_downloaded && download_info.audio_downloaded {
                let _ = merge_video_audio(window.clone(), &download_info.audio_path, &download_info.video_path, &task.display_name).await;
                download_info.finished = true;
                println!("{:?}", *download_info);
            }
        }
    }
}

async fn download_file(window: tauri::Window, task: DownloadTask, action: String) {
    let client = init_client();
    let response = match client.get(&task.url).send().await {
        Ok(res) => res,
        Err(e) => {
            eprintln!("Error sending request: {}", e);
            return;
        }
    };
    let total_size = response.headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    let mut file = match File::create(&task.path).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error creating file: {}", e);
            return;
        }
    };
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let start_time = Instant::now();
    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                downloaded += chunk.len() as u64;
                let elapsed_time = start_time.elapsed().as_millis();
                let speed = if elapsed_time > 0 {
                    (downloaded as f64 / elapsed_time as f64) * 1000.0 / 1048576.0
                } else { 0.0 };
                let remaining_time = if speed > 0.0 {
                    (total_size - downloaded) as f64 / (speed * 1048576.0)
                } else { 0.0 };
                if let Err(e) = file.write_all(&chunk).await {
                    eprintln!("Error writing to file: {}", e);
                    break;
                }
                let progress = downloaded as f64 / total_size as f64 * 100.0;
                let downloaded_mb = downloaded as f64 / 1048576.0;
                let formatted_values = vec![
                    format!("{}", task.cid),
                    format!("{:.2}%", progress),
                    format!("{:.2} s", remaining_time),
                    format!("{:.2} MB", downloaded_mb),
                    format!("{:.2} MB/s", speed),
                    format!("{:.2} ms", elapsed_time),
                    format!("{}", task.display_name),
                    format!("{}", task.file_type),
                    format!("{}", action)
                ];
                println!("{:?}", formatted_values);
                let _ = window.emit("download-progress", formatted_values);
            },
            Err(e) => {
                eprintln!("Error downloading chunk: {}", e);
                break;
            }
        }
    }
}

async fn merge_video_audio(window: tauri::Window, audio_path: &PathBuf, video_path: &PathBuf, output: &String) -> Result<(), String> {
    println!("Starting merge process for audio");
    let current_dir = env::current_dir().map_err(|e| e.to_string())?;
    let ffmpeg_path = current_dir.join("ffmpeg").join("ffmpeg.exe");
    let output_path = DOWNLOAD_DIRECTORY.join(&output);
    let output_clone = output.clone();
    let video_filename = Path::new(&output_path)
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| "无法提取视频文件名".to_string())?;

    let progress_path = current_dir.join("ffmpeg")
        .join(format!("{}.progress", video_filename));

    // let _ = window.emit("merge-start", output);
    println!("{:?} -i {:?} -i {:?} -c:v copy -c:a aac {:?} -progress {:?} -y", ffmpeg_path, video_path, audio_path, &output_path, &progress_path);
    let mut child = Command::new(ffmpeg_path)
        .arg("-i").arg(video_path)
        .arg("-i").arg(audio_path)
        .arg("-c:v").arg("copy")
        .arg("-c:a").arg("aac")
        .arg(&output_path).arg("-progress")
        .arg(&progress_path).arg("-y")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    let audio_path_clone = audio_path.clone();
    let video_path_clone = video_path.clone();
    let window_clone = window.clone();
    let progress_path_clone = &progress_path.clone();
    let progress_handle = tokio::spawn(async move {
        while !progress_path.exists() {
            sleep(Duration::from_millis(100)).await;
        }
        let mut progress_lines = VecDeque::new();
        let mut last_size: u64 = 0;
        loop {
            let mut printed_keys = HashSet::new();
            let metadata = tokio::fs::metadata(&progress_path).await.unwrap();
            if metadata.len() > last_size {
                let mut file = File::open(&progress_path).await.unwrap();
                file.seek(SeekFrom::Start(last_size)).await.unwrap();
                let mut reader = BufReader::new(file);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap() != 0 {
                    if progress_lines.len() >= 12 {
                        progress_lines.pop_front();
                    }
                    progress_lines.push_back(line.clone());
                    line.clear();
                }
                last_size = metadata.len();
            }
            let mut messages = Vec::new();
            for l in &progress_lines {
                let parts: Vec<&str> = l.split('=').collect();
                if parts.len() == 2 {
                    let key = parts[0].trim();
                    let value = parts[1].trim();
                    if !printed_keys.contains(key) {
                        match key {
                            "frame" | "fps" | "out_time" | "speed" => {
                                messages.push(value);
                            },
                            _ => continue,
                        };
                        printed_keys.insert(key.to_string());
                    }
                }
            }
            messages.push(&output_clone);
            println!("{:?}", messages);
            let _ = window_clone.emit("merge-progress", &messages).map_err(|e| e.to_string());
            if progress_lines.iter().any(|l| l.starts_with("progress=end")) {
                println!("FFmpeg process completed.");
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
    });    
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let _ = progress_handle.await.map_err(|e| e.to_string())?;
    if let Err(e) = tokio::fs::remove_file(audio_path_clone.clone()).await {
        eprintln!("无法删除原始音频文件: {}", e);
    }
    if let Err(e) = tokio::fs::remove_file(video_path_clone.clone()).await {
        eprintln!("无法删除原始视频文件: {}", e);
    }
    if let Err(e) = tokio::fs::remove_file(progress_path_clone).await {
        eprintln!("无法删除进度文件: {}", e);
    }
    if status.success() {
        let _ = window.emit("merge-success", output);
        Ok(())
    } else {
        if let Err(e) = tokio::fs::remove_file(output_path.clone()).await {
            eprintln!("无法删除合并失败视频文件: {}", e);
        }
        let _ = window.emit("merge-failed", output);
        Err("FFmpeg command failed".to_string())
    }
}

async fn update_cookies(sessdata: &str) -> Result<String, String> {
    let appdata_path = match env::var("APPDATA") {
        Ok(path) => path,
        Err(_) => {
            eprintln!("无法获取APPDATA路径");
            return Err("无法获取APPDATA路径".to_string());
        }
    };
    let working_dir = PathBuf::from(appdata_path).join("com.btjawa.biliget");
    let sessdata_path = working_dir.join("Cookies");
    if let Some(dir_path) = sessdata_path.parent() {
        if let Err(_) = fs::create_dir_all(dir_path) {
            eprintln!("无法创建目录");
            return Err("无法创建目录".to_string());
        }
    }
    if let Err(_) = fs::write(&sessdata_path, sessdata) {
        eprintln!("无法写入SESSDATA文件");
        return Err("无法写入SESSDATA文件".to_string());
    }
    println!("SESSDATA写入成功");
    let url = Url::parse("https://www.bilibili.com").unwrap();
    let cookie_str = format!("{}; Domain=.bilibili.com; Path=/", sessdata);
    let jar = GLOBAL_COOKIE_JAR.write().unwrap();
    jar.add_cookie_str(&cookie_str, &url);
    return Ok("Updated Cookies".to_string());
}

// Learn more about Tauri commands at https://tauri.app/v1/guides/features/command
#[tauri::command]
async fn init(window: tauri::Window) -> Result<i64, String> {
    let appdata_path = env::var("APPDATA").map_err(|e| e.to_string())?;
    let working_dir = PathBuf::from(appdata_path).join("com.btjawa.biliget");
    let sessdata_path = working_dir.join("Cookies");
    if !working_dir.exists() {
        fs::create_dir_all(&working_dir).map_err(|e| e.to_string())?;
        println!("成功创建com.btjawa.biliget");
    }
    if !&TEMP_DIRECTORY.exists() {
        fs::create_dir_all(&*TEMP_DIRECTORY).map_err(|e| e.to_string())?;
        println!("成功创建TEMP_DIRECTORY");
    }
    if !sessdata_path.exists() {
        fs::write(&sessdata_path, "").map_err(|e| e.to_string())?;
        println!("成功创建Cookies");
        window.emit("user-mid", vec![0.to_string(), "init".to_string()]).unwrap();
        return Ok(0);
    }
    let sessdata = fs::read_to_string(&sessdata_path).map_err(|e| e.to_string())?;
    if sessdata.trim().is_empty() {
        window.emit("user-mid", vec![0.to_string(), "init".to_string()]).unwrap();
        return Ok(0);
    }
    update_cookies(&sessdata).await.map_err(|e| e.to_string())?;
    let mid = init_mid().await.map_err(|e| e.to_string())?;
    window.emit("user-mid", vec![mid.to_string(), "init".to_string()]).unwrap();
    return Ok(mid);
}

async fn init_mid() -> Result<i64, String> {
    let client = init_client();
    let mid_response = client
        .get("https://api.bilibili.com/x/member/web/account")
        .send()
        .await
        .map_err(|e| e.to_string())?;    
    if mid_response.status().is_success() {
        let json: serde_json::Value = mid_response.json().await.map_err(|e| e.to_string())?;
        if let Some(mid) = json["data"]["mid"].as_i64() {
            return Ok(mid);
        } else {
            eprint!("找不到Mid");
            return Err("找不到Mid".to_string());
        }    
    } else {
        eprintln!("请求失败");
        return Err("请求失败".into());
    }    
}   

#[tauri::command]
async fn exit(window: tauri::Window) -> Result<i64, String> {
    {
        let mut cookie_jar = GLOBAL_COOKIE_JAR.write().unwrap();
        *cookie_jar = Jar::default();
    }
    let appdata_path = env::var("APPDATA").map_err(|e| e.to_string())?;
    let working_dir = PathBuf::from(appdata_path).join("com.btjawa.biliget");
    let sessdata_path = working_dir.join("Cookies");
    if let Err(e) = fs::remove_file(sessdata_path) {
        return Err(format!("Failed to delete store directory: {}", e));
    }
    window.emit("exit-success", 0).unwrap();
    return Ok(0)
}

#[tauri::command]
async fn stop_login() {
    let mut stop = STOP_LOGIN.lock().await;
    *stop = true;
}

#[tauri::command]
async fn login(window: tauri::Window, qrcode_key: String) -> Result<String, String> {
    let client = init_client();
    let mut cloned_key = qrcode_key.clone();
    let mask_range = 8..cloned_key.len()-8;
    let mask = "*".repeat(mask_range.end - mask_range.start);
    cloned_key.replace_range(mask_range, &mask);
    loop {
        let stop = {
            let lock = STOP_LOGIN.lock().await;
            *lock
        };
        if stop {
            let mut lock = STOP_LOGIN.lock().await;
            *lock = false;
            eprintln!("{}: \"登录轮询被前端截断\"", cloned_key);
            return Ok("登录过程被终止".to_string());
        }
        let response = client
            .get(format!(
                "https://passport.bilibili.com/x/passport-login/web/qrcode/poll?qrcode_key={}",
                qrcode_key
            )).send().await.map_err(|e| e.to_string())?;

        if response.status() != reqwest::StatusCode::OK {
            if response.status().to_string() != "412 Precondition Failed" {
                eprintln!("检查登录状态失败");
                return Err("检查登录状态失败".to_string());
            }
        }
        let cookie_header = response.headers().clone().get(header::SET_COOKIE)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        let response_data: Value = response.json().await.map_err(|e| {
            eprintln!("解析响应JSON失败: {}", e);
            "解析响应JSON失败".to_string()}
        )?;
        match response_data["code"].as_i64() {
            Some(-412) => {
                eprintln!("{}", response_data["message"]);
                window.emit("login-status", response_data["message"].to_string()).map_err(|e| e.to_string())?;
                return Err(response_data["message"].to_string());
            }
            Some(0) => {
                match response_data["data"]["code"].as_i64() {
                    Some(0) => {
                        if let Some(cookie) = cookie_header {
                            let sessdata = cookie.split(';').find(|part| part.trim_start().starts_with("SESSDATA"))
                            .ok_or_else(|| {
                                eprintln!("找不到SESSDATA");
                                "找不到SESSDATA".to_string()
                            })?;
                            update_cookies(sessdata).await.map_err(|e| e.to_string())?;
                            let mid = init_mid().await.map_err(|e| e.to_string())?;
                            window.emit("user-mid", [mid.to_string(), "login".to_string()]).map_err(|e| e.to_string())?;
                            println!("{}: \"二维码已扫描\"", cloned_key);
                            return Ok("二维码已扫描".to_string());
                        } else {
                            eprintln!("Cookie响应头为空");
                            return Err("Cookie响应头为空".to_string());
                        }
                    }
                    Some(86038) => return Err("二维码已失效".to_string()),
                    Some(86101) | Some(86090) => {
                        window.emit("login-status", response_data["data"]["message"].to_string()).map_err(|e| e.to_string())?;
                        println!("{}: {}", cloned_key, response_data["data"]["message"]);
                    }
                    _ => {
                        eprintln!("未知的响应代码");
                        return Err("未知的响应代码".to_string())
                    },
                }
            }
            _ => {
                eprintln!("未知的响应代码");
                return Err("未知的响应代码".to_string())
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() {
    let api_route = warp::path("api")
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::query::raw().or_else(|_| async { Ok::<_, warp::Rejection>(("".to_string(),)) }))
        .and(warp::body::bytes())
        .map(|method, path, query, body| (method, path, query, body, "https://api.bilibili.com".to_string()))
        .and_then(proxy_request);

    let i0_route = warp::path("i0")
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::query::raw().or_else(|_| async { Ok::<_, warp::Rejection>(("".to_string(),)) }))
        .and(warp::body::bytes())
        .map(|method, path, query, body| (method, path, query, body, "https://i0.hdslb.com".to_string()))
        .and_then(proxy_request);

    let passport_route = warp::path("passport")
        .and(warp::method())
        .and(warp::path::full())
        .and(warp::query::raw().or_else(|_| async { Ok::<_, warp::Rejection>(("".to_string(),)) }))
        .and(warp::body::bytes())
        .map(|method, path, query, body| (method, path, query, body, "https://passport.bilibili.com".to_string()))
        .and_then(proxy_request);
    
    let routes = i0_route.or(api_route.or(passport_route));
    tokio::task::spawn(async move {
        warp::serve(routes).run(([127, 0, 0, 1], 50808)).await;
    });
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![init, login, stop_login, init_download_multi, exit])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn proxy_request(args: (Method, FullPath, String, Bytes, String)) -> Result<impl Reply, Infallible> {
    let (method, path, raw_query, body, base_url) = args;
    let path_str = path.as_str();
    let trimmed_path = path_str
        .strip_prefix("/api")
        .or_else(|| path_str.strip_prefix("/passport"))
        .or_else(|| path_str.strip_prefix("/i0"))
        .unwrap_or(path_str);
    let full_path = if !raw_query.is_empty() {
        format!("{}?{}", trimmed_path, raw_query)
    } else {
        trimmed_path.to_string()
    };
    let target_url = format!("{}{}", base_url, full_path);
    println!("Request: {}", target_url);
    let client = init_client();
    let res = client.request(method, &target_url).body(body).send().await;
    let mut response_builder = Response::builder();
    if let Ok(response) = res {
        for (key, value) in response.headers().iter() {
            response_builder = response_builder.header(key, value);
        }
        response_builder = response_builder.header("Access-Control-Allow-Origin", "*");
        let content_type = response.headers().get(warp::http::header::CONTENT_TYPE);
        let body = if let Some(content_type) = content_type {
            if content_type.to_str().unwrap_or_default().starts_with("text/") {
                response.text().await.unwrap_or_default().into()
            } else {
                response.bytes().await.unwrap_or_default()
            }
        } else {
            response.bytes().await.unwrap_or_default()
        };
        Ok(response_builder.body(body).unwrap())
    } else {
        Ok(response_builder
            .status(warp::http::StatusCode::BAD_GATEWAY)
            .body("Error processing the request".into())
            .unwrap())
    }
}