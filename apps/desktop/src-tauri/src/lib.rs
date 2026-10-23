//! Full Recovery app: a window around the recovery engine. The same code
//! runs on desktop (main.rs) and on Android (`run` is the mobile entry point).

mod session;

use std::path::PathBuf;
use std::sync::Arc;

use fullrecovery_core::devices::{self, Device};
use session::{Emit, Item, Shared, SourceInfo, TextPreview};
use tauri::http::{header, Response as HttpResponse, StatusCode};
use tauri::ipc::Response;
use tauri::{AppHandle, Emitter, Manager, State};

fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |name, payload| {
        let _ = app.emit(name, payload);
    })
}

#[tauri::command]
fn list_drives() -> Vec<Device> {
    devices::list()
}

#[tauri::command]
fn open_source(state: State<'_, Shared>, path: String) -> Result<SourceInfo, String> {
    session::open(&state, &path)
}

/// Opens a USB stick or card reader that the Android side of the app
/// opened (window.FullRecoveryAndroid.openUsb).
#[tauri::command]
async fn open_usb_source(
    state: State<'_, Shared>,
    usb: session::UsbDevice,
) -> Result<SourceInfo, String> {
    session::open_usb(&state, usb)
}

#[tauri::command]
fn open_folder_source(state: State<'_, Shared>, path: String) -> Result<SourceInfo, String> {
    session::open_folder(&state, &path)
}

/// "android", "windows", "macos", "linux", ...
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

/// Where recovered files are saved on systems without a folder picker.
#[tauri::command]
fn default_output_dir() -> PathBuf {
    session::default_output_dir()
}

#[tauri::command]
fn start_quick_scan(
    app: AppHandle,
    state: State<'_, Shared>,
    search_lost: bool,
) -> Result<(), String> {
    session::start_quick_scan(&state, search_lost, emitter(&app))
}

#[tauri::command]
fn start_deep_scan(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    session::start_deep_scan(&state, emitter(&app))
}

#[tauri::command]
fn cancel(state: State<'_, Shared>) {
    session::cancel(&state);
}

#[tauri::command]
fn get_items(state: State<'_, Shared>, from: usize) -> Vec<Item> {
    session::items(&state, from)
}

#[tauri::command]
async fn text_preview(state: State<'_, Shared>, id: u32) -> Result<TextPreview, String> {
    session::text_preview(&state, id)
}

/// The bytes of a file, for playing videos and audio in the preview. Kept
/// in memory: writing a temporary file could overwrite deleted data.
#[tauri::command]
async fn preview_bytes(state: State<'_, Shared>, id: u32) -> Result<Response, String> {
    session::read_item(&state, id, session::MAX_PREVIEW).map(|(_, data)| Response::new(data))
}

#[tauri::command]
fn recover(
    app: AppHandle,
    state: State<'_, Shared>,
    ids: Vec<u32>,
    out: PathBuf,
) -> Result<(), String> {
    session::start_recover(&state, ids, out, emitter(&app))
}

#[tauri::command]
fn create_image(app: AppHandle, state: State<'_, Shared>, dest: PathBuf) -> Result<(), String> {
    session::start_image(&state, dest, emitter(&app))
}

#[tauri::command]
fn open_folder(path: PathBuf) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| e.to_string())
}

/// Serves file contents for the preview pane at `preview://localhost/<id>`.
fn preview_response(app: &AppHandle, path: &str) -> HttpResponse<Vec<u8>> {
    let state = app.state::<Shared>();
    let id = path.trim_start_matches('/').parse::<u32>();
    let result = id
        .map_err(|_| "bad file id".to_owned())
        .and_then(|id| session::read_item(&state, id, session::MAX_PREVIEW));
    let builder = HttpResponse::builder().header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    match result {
        Ok((item, data)) => builder
            .header(header::CONTENT_TYPE, session::mime_type(&item.ext))
            .body(data),
        Err(e) => builder.status(StatusCode::NOT_FOUND).body(e.into_bytes()),
    }
    .expect("valid response")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Shared::default())
        .register_asynchronous_uri_scheme_protocol("preview", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_owned();
            // Reading a large file must not block the window.
            std::thread::spawn(move || responder.respond(preview_response(&app, &path)));
        })
        .invoke_handler(tauri::generate_handler![
            list_drives,
            open_source,
            open_folder_source,
            open_usb_source,
            platform,
            default_output_dir,
            start_quick_scan,
            start_deep_scan,
            cancel,
            get_items,
            text_preview,
            preview_bytes,
            recover,
            create_image,
            open_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Full Recovery");
}
