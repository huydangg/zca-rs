//! Sending images, files and GIFs (zca-js `uploadAttachment` + the attachment half of
//! `sendMessage`).
//!
//! Every request carries the plain JSON params AES-encrypted in the query; the answers are the
//! usual encrypted `{error_code, data}`. `{file}` is the first `zpw_service_map_v3.file` host.
//!
//! * **Image** (`jpg` / `jpeg` / `png` / `webp`): the file is cut into `chunk_size_file` chunks
//!   and each is POSTed as the multipart field `chunkContent` to
//!   `{file}/api/message/photo_original/upload` (group: `{file}/api/group/...`), query `type=2`
//!   (group `11`), params `{toid | grid, totalChunk, fileName, clientId, totalSize, imei,
//!   isE2EE: 0, jxl: 0, chunkId}` (`chunkId` counts from 1). The last answer carries `photoId`,
//!   `normalUrl`, `hdUrl`, `thumbUrl`. Then `{file}/api/message/photo_original/send?nretry=0` (group
//!   `{file}/api/group/...`) with `{photoId, clientId, desc, width, height, toid | grid, rawUrl,
//!   hdUrl, thumbUrl, oriUrl (group) | normalUrl (DM), hdSize, zsource: -1, ttl, jcp}`.
//! * **File** (anything else, `mp4` too): the same chunks to `{file}/api/message/asyncfile/upload`
//!   (group `{file}/api/group/...`). The last answer has a `fileId`, but not the file's URL: Zalo
//!   says that later, on the websocket, as a control frame (`cmd 601`, `act_type: "file_done"`,
//!   `fileId`, `data.url`). The upload waits for that frame (see [`FileWaiters`]), then sends
//!   `{file}/api/message/asyncfile/msg?nretry=0` (group `{file}/api/group/...`) with `{fileId,
//!   checksum (md5), checksumSha: "", extention, totalSize, fileName, clientId (the answer's
//!   clientFileId), fType: 1, fileCount: 0, fdata: "{}", toid | grid, fileUrl, zsource: -1, ttl}`.
//! * **GIF** (`gif`): `{file}/api/message/upthumb` (multipart field `fileContent`, file name `blob`,
//!   type `image/png`, params `{clientId, imei}`; the answer's `url` is the thumbnail), then one
//!   POST of the whole file as `chunkContent` to `{file}/api/message/gif?nretry=0&type=1` (group
//!   `{file}/api/group/gif`) with `{clientId, fileName, totalSize, width, height, msg, type: 1, ttl,
//!   visibility: 0 (group), toid | grid, thumb, checksum (md5), totalChunk: 1, chunkId: 1}`.
//!   zca-js uploads the GIF file itself as the thumbnail; so does this.
//! * A caption goes along as the image's `desc`; for a file or a GIF zca-js sends it as a text
//!   message of its own, before the attachment; so does this.
//!
//! Sizes and the extension blacklist come from the `sharefile` block of the login settings
//! ([`ShareLimits`]): a file over the limit is refused before anything is uploaded.
//!
//! Derived from zca-js (MIT, Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208,
//! JustKemForFun); see `THIRD_PARTY_NOTICES.md` at the crate root.

use crate::api::{self, Planned, PlannedUpload};
use crate::client::{now_ms, SentMessage};
use crate::error::{Error, Result};
use crate::http::Http;
use crate::message::{id_of, ThreadType};
use crate::session::Session;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long an upload waits for Zalo's "file done" frame (zca-js keeps its callback 5 minutes).
pub(crate) const FILE_DONE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// What a thing to send is: the way zca-js picks the upload by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UploadKind {
    /// `jpg`, `jpeg`, `png`, `webp`: a photo.
    Image,
    /// `gif`: an animated picture.
    Gif,
    /// Anything else: a file.
    File,
}

impl UploadKind {
    /// The kind of a file with this extension (any case, no dot).
    pub fn of_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "webp" => UploadKind::Image,
            "gif" => UploadKind::Gif,
            _ => UploadKind::File,
        }
    }
}

/// The thing to upload: a file on disk, or bytes with the name they are sent under.
#[derive(Clone, Debug)]
pub enum Upload {
    /// A file on disk (read when it is sent; its name is the file name).
    Path(PathBuf),
    /// Bytes in memory and the file name Zalo shows (its extension picks the kind).
    Bytes {
        /// The file name.
        name: String,
        /// The content.
        data: Vec<u8>,
    },
}

impl Upload {
    /// A file on disk.
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Upload::Path(path.into())
    }

    /// Bytes in memory, sent under `name`.
    pub fn bytes(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Upload::Bytes { name: name.into(), data: data.into() }
    }

    /// The file name and its lowercase extension (none: ""), as zca-js `getFileName` / `getFileExtension`.
    pub fn name_and_extension(&self) -> Result<(String, String)> {
        let raw = match self {
            Upload::Path(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
            Upload::Bytes { name, .. } => Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned()),
        };
        let name = raw.filter(|n| !n.is_empty()).ok_or_else(|| Error::invalid("Missing filename"))?;
        let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        Ok((name, ext))
    }

    /// The size in bytes (a path is only looked at, not read).
    fn size(&self) -> Result<u64> {
        match self {
            Upload::Bytes { data, .. } => Ok(data.len() as u64),
            Upload::Path(p) => match std::fs::metadata(p) {
                Ok(m) if m.is_file() => Ok(m.len()),
                Ok(_) => Err(Error::invalid(format!("{} is not a file", p.display()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::invalid("File not found")),
                Err(e) => Err(Error::Io(e)),
            },
        }
    }

    async fn read(self) -> Result<Vec<u8>> {
        match self {
            Upload::Bytes { data, .. } => Ok(data),
            Upload::Path(p) => tokio::task::spawn_blocking(move || std::fs::read(p)).await.map_err(|e| Error::protocol(format!("reading the file: {e}")))?.map_err(Error::Io),
        }
    }
}

/// What Zalo lets an account share: the `sharefile` block of the login settings
/// (`max_file`, `max_size_share_file_v3`, `chunk_size_file`, `restricted_ext_file`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareLimits {
    /// Most files in one message (`max_file`; 0 when the settings do not say). One is sent at a time here.
    pub max_files: u64,
    /// Largest file in MB (`max_size_share_file_v3`); images, GIFs and files alike (as zca-js).
    pub max_size_mb: u64,
    /// Bytes per uploaded chunk (`chunk_size_file`).
    pub chunk_size: usize,
    /// Extensions Zalo refuses (`restricted_ext_file`), as the settings spell them.
    pub restricted_ext: Vec<String>,
}

/// A JSON number, or a string of digits, as a number.
fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl ShareLimits {
    /// Read the limits out of the login settings (`features.sharefile`).
    pub(crate) fn from_settings(settings: &Value) -> Result<Self> {
        let s = &settings["features"]["sharefile"];
        let need = |key: &str| number(&s[key]).filter(|n| *n > 0).ok_or_else(|| Error::protocol(format!("login settings carry no usable sharefile.{key}")));
        Ok(ShareLimits {
            max_files: number(&s["max_file"]).unwrap_or(0),
            max_size_mb: need("max_size_share_file_v3")?,
            chunk_size: usize::try_from(need("chunk_size_file")?).map_err(|_| Error::protocol("sharefile.chunk_size_file is too big"))?,
            restricted_ext: s["restricted_ext_file"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect(),
        })
    }

    /// The size limit in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_size_mb.saturating_mul(1024 * 1024)
    }

    /// zca-js `isExceedMaxFileSize`, as an error that says it.
    fn check_size(&self, name: &str, size: u64) -> Result<()> {
        if size == 0 {
            return Err(Error::invalid(format!("{name} is empty")));
        }
        if size > self.max_bytes() {
            return Err(Error::invalid(format!("File {name} size exceed maximum size of {}MB", self.max_size_mb)));
        }
        Ok(())
    }

    /// zca-js `isExtensionValid`.
    fn check_extension(&self, ext: &str) -> Result<()> {
        if self.restricted_ext.iter().any(|r| r.eq_ignore_ascii_case(ext)) {
            return Err(Error::invalid(format!("File extension \"{ext}\" is not allowed")));
        }
        Ok(())
    }
}

/// An upload that passed the checks and is read.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub name: String,
    pub data: Vec<u8>,
}

/// Check `up` against `limits` and read it. `want` is what the caller sends it as: an image must
/// have an image extension, a GIF `.gif`; a file may be anything. The extension blacklist does not
/// apply to GIFs (zca-js does not check it on that path).
pub(crate) async fn prepare(up: Upload, want: UploadKind, limits: &ShareLimits) -> Result<Prepared> {
    let (name, ext) = up.name_and_extension()?;
    match (want, UploadKind::of_extension(&ext)) {
        (UploadKind::Image, k) if k != UploadKind::Image => return Err(Error::invalid(format!("{name} is not a jpg, png or webp image"))),
        (UploadKind::Gif, k) if k != UploadKind::Gif => return Err(Error::invalid(format!("{name} is not a gif"))),
        _ => {}
    }
    if want != UploadKind::Gif {
        limits.check_extension(&ext)?;
    }
    limits.check_size(&name, up.size()?)?;
    let data = up.read().await?;
    // the file may have changed since it was looked at
    limits.check_size(&name, data.len() as u64)?;
    Ok(Prepared { name, data })
}

/// The width and height in pixels, from the header only.
pub(crate) fn dimensions(data: &[u8]) -> Result<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()?
        .into_dimensions()
        .map_err(|e| Error::invalid(format!("can't read the picture's size: {e}")))
}

/// The md5 of the whole content in lowercase hex (zca-js `getMd5LargeFileObject`).
pub(crate) fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

/// How many chunks of `chunk_size` the content takes (zca-js `Math.ceil(totalSize / chunkSize)`).
pub(crate) fn chunk_count(len: usize, chunk_size: usize) -> usize {
    len.div_ceil(chunk_size)
}

/// `toid` for a DM, `grid` for a group.
fn target_key(tt: ThreadType) -> &'static str {
    if tt.is_group() {
        "grid"
    } else {
        "toid"
    }
}

/// Drop the null members (what `JSON.stringify` does with `undefined`).
fn strip_nulls(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
}

// ------------------------------------------------------------------ planning

/// One chunk of the upload of an image (`image`) or of any other file.
pub(crate) fn plan_chunk(image: bool, imei: &str, thread_id: &str, tt: ThreadType, name: &str, total_size: u64, total_chunk: usize, chunk_id: usize, client_id: i64) -> PlannedUpload {
    let group = tt.is_group();
    let path = match (image, group) {
        (true, false) => "/api/message/photo_original/upload",
        (true, true) => "/api/group/photo_original/upload",
        (false, false) => "/api/message/asyncfile/upload",
        (false, true) => "/api/group/asyncfile/upload",
    };
    let mut params = json!({"totalChunk": total_chunk, "fileName": name, "clientId": client_id, "totalSize": total_size, "imei": imei, "isE2EE": 0, "jxl": 0, "chunkId": chunk_id});
    params[target_key(tt)] = json!(thread_id);
    PlannedUpload {
        service: "file",
        path,
        nretry: false,
        query: vec![("type", if group { "11" } else { "2" }.into())],
        params,
        field: "chunkContent",
        file_name: name.to_string(),
        content_type: "application/octet-stream",
    }
}

/// What the last answer of an image upload gives.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ImageUploaded {
    pub photo_id: Value,
    pub normal_url: Value,
    pub hd_url: Value,
    pub thumb_url: Value,
}

/// An id Zalo uses for "none yet": the answer of a chunk that did not finish the upload.
fn real_id(v: &Value) -> Option<Value> {
    id_of(v).filter(|s| s != "-1").map(|_| v.clone())
}

pub(crate) fn parse_image_upload(resp: &Value) -> Result<ImageUploaded> {
    Ok(ImageUploaded {
        photo_id: real_id(&resp["photoId"]).ok_or_else(|| Error::protocol("the image upload did not finish: no photoId in the last answer"))?,
        normal_url: resp["normalUrl"].clone(),
        hd_url: resp["hdUrl"].clone(),
        thumb_url: resp["thumbUrl"].clone(),
    })
}

/// What the last answer of a file upload gives (its URL comes later, see [`FileWaiters`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileUploaded {
    pub file_id: String,
    /// `clientFileId`: the `clientId` of the send (`Null` when the answer has none).
    pub client_file_id: Value,
}

pub(crate) fn parse_file_upload(resp: &Value) -> Result<FileUploaded> {
    let file_id = real_id(&resp["fileId"]).and_then(|v| id_of(&v)).ok_or_else(|| Error::protocol("the file upload did not finish: no fileId in the last answer"))?;
    Ok(FileUploaded { file_id, client_file_id: resp["clientFileId"].clone() })
}

/// The message that shows an uploaded image (`photo_original/send`).
pub(crate) fn plan_image_send(thread_id: &str, tt: ThreadType, img: &ImageUploaded, width: u32, height: u32, total_size: u64, caption: &str, client_id: i64) -> Planned {
    let group = tt.is_group();
    let mut params = json!({
        "photoId": img.photo_id,
        "clientId": client_id.to_string(),
        "desc": caption,
        "width": width,
        "height": height,
        "rawUrl": img.normal_url,
        "hdUrl": img.hd_url,
        "thumbUrl": img.thumb_url,
        "hdSize": total_size.to_string(),
        "zsource": -1,
        "ttl": 0,
        "jcp": "{\"convertible\":\"jxl\"}",
    });
    params[target_key(tt)] = json!(thread_id);
    params[if group { "oriUrl" } else { "normalUrl" }] = img.normal_url.clone();
    strip_nulls(&mut params);
    Planned { service: "file", path: if group { "/api/group/photo_original/send" } else { "/api/message/photo_original/send" }, nretry: true, params }
}

/// The message that shows an uploaded file (`asyncfile/msg`).
pub(crate) fn plan_file_send(thread_id: &str, tt: ThreadType, up: &FileUploaded, done_url: &str, name: &str, total_size: u64, checksum: &str, fallback_client_id: i64) -> Planned {
    let group = tt.is_group();
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let mut params = json!({
        "fileId": up.file_id,
        "checksum": checksum,
        "checksumSha": "",
        "extention": ext,
        "totalSize": total_size,
        "fileName": name,
        "clientId": if up.client_file_id.is_null() { json!(fallback_client_id) } else { up.client_file_id.clone() },
        "fType": 1,
        "fileCount": 0,
        "fdata": "{}",
        "fileUrl": done_url,
        "zsource": -1,
        "ttl": 0,
    });
    params[target_key(tt)] = json!(thread_id);
    Planned { service: "file", path: if group { "/api/group/asyncfile/msg" } else { "/api/message/asyncfile/msg" }, nretry: true, params }
}

/// The upload of a GIF's thumbnail (`upthumb`; always the DM host path, as zca-js).
pub(crate) fn plan_upthumb(imei: &str, client_id: i64) -> PlannedUpload {
    PlannedUpload {
        service: "file",
        path: "/api/message/upthumb",
        nretry: false,
        query: vec![],
        params: json!({"clientId": client_id, "imei": imei}),
        field: "fileContent",
        file_name: "blob".into(),
        content_type: "image/png",
    }
}

/// The GIF message itself: the whole file in one chunk.
pub(crate) fn plan_gif(thread_id: &str, tt: ThreadType, name: &str, total_size: u64, width: u32, height: u32, thumb: &str, checksum: &str, client_id: i64) -> PlannedUpload {
    let group = tt.is_group();
    let mut params = json!({
        "clientId": client_id.to_string(),
        "fileName": name,
        "totalSize": total_size,
        "width": width,
        "height": height,
        "msg": "",
        "type": 1,
        "ttl": 0,
        "thumb": thumb,
        "checksum": checksum,
        "totalChunk": 1,
        "chunkId": 1,
    });
    if group {
        params["visibility"] = json!(0);
    }
    params[target_key(tt)] = json!(thread_id);
    PlannedUpload {
        service: "file",
        path: if group { "/api/group/gif" } else { "/api/message/gif" },
        nretry: true,
        query: vec![("type", "1".into())],
        params,
        field: "chunkContent",
        file_name: name.to_string(),
        content_type: "application/octet-stream",
    }
}

// ------------------------------------------------------------------ waiting for "file done"

/// Zalo's word that an uploaded file is stored: its `fileId` and the URL it lives at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileDone {
    pub file_id: String,
    pub url: String,
}

/// The `file_done` controls of a control frame (`cmd 601`); `data` is the decoded event, so the
/// controls are in `data.data.controls` (zca-js `listen`: `control.content.act_type == "file_done"`,
/// `fileId` and `data.url`).
pub(crate) fn file_done_events(data: &Value) -> Vec<FileDone> {
    let mut out = Vec::new();
    for control in data["data"]["controls"].as_array().into_iter().flatten() {
        let content = &control["content"];
        if content["act_type"].as_str() != Some("file_done") {
            continue;
        }
        // `data` is an object; tolerate a JSON string of one
        let inner = match &content["data"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        };
        if let (Some(file_id), Some(url)) = (id_of(&content["fileId"]), inner["url"].as_str().filter(|u| !u.is_empty())) {
            out.push(FileDone { file_id, url: url.to_string() });
        }
    }
    out
}

/// The uploads waiting for their "file done" frame: the stand-in for zca-js's
/// `ctx.uploadCallbacks`. The listener calls [`FileWaiters::done`] for every such frame; an
/// upload calls [`FileWaiters::expect`] with the `fileId` its last chunk answered.
///
/// A frame that arrives before its upload asks (the answer and the frame race) is kept
/// (the newest [`EARLY_MAX`]) and handed over at once.
#[derive(Default)]
pub(crate) struct FileWaiters {
    state: Mutex<WaitState>,
}

#[derive(Default)]
struct WaitState {
    waiting: HashMap<String, oneshot::Sender<FileDone>>,
    early: VecDeque<FileDone>,
}

/// Frames kept for uploads that have not asked yet.
const EARLY_MAX: usize = 32;

impl FileWaiters {
    fn state(&self) -> std::sync::MutexGuard<'_, WaitState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for `file_id`'s frame.
    pub fn expect(&self, file_id: &str) -> oneshot::Receiver<FileDone> {
        let (tx, rx) = oneshot::channel();
        let mut st = self.state();
        match st.early.iter().position(|d| d.file_id == file_id).and_then(|i| st.early.remove(i)) {
            Some(done) => {
                let _ = tx.send(done);
            }
            None => {
                st.waiting.insert(file_id.to_string(), tx);
            }
        }
        rx
    }

    /// A "file done" frame arrived.
    pub fn done(&self, done: FileDone) {
        let mut st = self.state();
        match st.waiting.remove(&done.file_id) {
            Some(tx) => {
                let _ = tx.send(done);
            }
            None => {
                if st.early.len() >= EARLY_MAX {
                    st.early.pop_front();
                }
                st.early.push_back(done);
            }
        }
    }

    /// Stop waiting for `file_id`.
    pub fn forget(&self, file_id: &str) {
        self.state().waiting.remove(file_id);
    }

    /// The URL of the file `file_id`, once Zalo says it is stored; Err when it takes longer than `timeout`.
    pub async fn wait(&self, file_id: &str, timeout: Duration) -> Result<FileDone> {
        let rx = self.expect(file_id);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(_)) => Err(Error::protocol("the upload's wait was cancelled")),
            Err(_) => {
                self.forget(file_id);
                Err(Error::protocol(format!("Zalo did not confirm the upload within {} s", timeout.as_secs())))
            }
        }
    }
}

// ------------------------------------------------------------------ sending

/// A sent image, file or GIF.
#[derive(Clone, Debug, PartialEq)]
pub struct SentAttachment {
    /// What was sent.
    pub kind: UploadKind,
    /// The file name it was sent under.
    pub file_name: String,
    /// Its size in bytes.
    pub total_size: u64,
    /// Pixel width (images and GIFs).
    pub width: Option<u32>,
    /// Pixel height (images and GIFs).
    pub height: Option<u32>,
    /// Where Zalo keeps it: the image's `normalUrl`, the file's `fileUrl`. `None` for a GIF (Zalo does not say).
    pub url: Option<String>,
    /// The thumbnail Zalo made (an image's `thumbUrl`) or took (a GIF's `upthumb` URL).
    pub thumb_url: Option<String>,
    /// The attachment's message: `msg_id`, the `client_id` it was sent with, Zalo's raw answer.
    pub message: SentMessage,
    /// The caption's own message, sent before a file or a GIF (an image carries its caption as
    /// the message itself); `None` when there was no caption or it is the image's.
    pub caption: Option<SentMessage>,
}

fn sent(data: Value, client_id: i64) -> SentMessage {
    SentMessage { msg_id: id_of(&data["msgId"]), client_id, raw: data }
}

fn check_thread(thread_id: &str) -> Result<()> {
    if thread_id.is_empty() {
        return Err(Error::invalid("Missing threadId"));
    }
    Ok(())
}

/// What the senders need from the client.
pub(crate) struct Ctx<'a> {
    pub http: &'a Http,
    pub session: &'a Session,
    pub imei: &'a str,
    pub limits: &'a ShareLimits,
    pub waiters: &'a FileWaiters,
}

impl Ctx<'_> {
    /// All chunks of `p`, in order; the last answer is the upload's.
    async fn upload_chunks(&self, image: bool, thread_id: &str, tt: ThreadType, p: &Prepared) -> Result<Value> {
        let total = chunk_count(p.data.len(), self.limits.chunk_size);
        let client_id = now_ms();
        let mut last = Value::Null;
        for (i, chunk) in p.data.chunks(self.limits.chunk_size).enumerate() {
            let req = plan_chunk(image, self.imei, thread_id, tt, &p.name, p.data.len() as u64, total, i + 1, client_id);
            last = api::run_upload(self.http, self.session, &req, chunk.to_vec()).await?;
        }
        Ok(last)
    }

    /// A caption as a text message of its own (`None`: no caption).
    async fn send_caption(&self, thread_id: &str, tt: ThreadType, caption: &str) -> Result<Option<SentMessage>> {
        if caption.is_empty() {
            return Ok(None);
        }
        let client_id = now_ms();
        let planned = api::plan_text(self.imei, thread_id, tt, caption, client_id, None, &[]);
        Ok(Some(sent(api::run(self.http, self.session, &planned).await?, client_id)))
    }

    /// Send a photo: `caption` is its description.
    pub async fn send_image(&self, thread_id: &str, tt: ThreadType, up: Upload, caption: &str) -> Result<SentAttachment> {
        check_thread(thread_id)?;
        let p = prepare(up, UploadKind::Image, self.limits).await?;
        let (width, height) = dimensions(&p.data)?;
        let total_size = p.data.len() as u64;
        let img = parse_image_upload(&self.upload_chunks(true, thread_id, tt, &p).await?)?;
        let client_id = now_ms();
        let planned = plan_image_send(thread_id, tt, &img, width, height, total_size, caption, client_id);
        let answer = api::run(self.http, self.session, &planned).await?;
        let text = |v: &Value| v.as_str().map(String::from);
        Ok(SentAttachment {
            kind: UploadKind::Image,
            file_name: p.name,
            total_size,
            width: Some(width),
            height: Some(height),
            url: text(&img.normal_url),
            thumb_url: text(&img.thumb_url),
            message: sent(answer, client_id),
            caption: None,
        })
    }

    /// Send a file; waits for Zalo's "file done" frame, so the listener must be running.
    pub async fn send_file(&self, thread_id: &str, tt: ThreadType, up: Upload, caption: &str) -> Result<SentAttachment> {
        check_thread(thread_id)?;
        let p = prepare(up, UploadKind::File, self.limits).await?;
        let total_size = p.data.len() as u64;
        let uploaded = parse_file_upload(&self.upload_chunks(false, thread_id, tt, &p).await?)?;
        let done = self.waiters.wait(&uploaded.file_id, FILE_DONE_TIMEOUT).await?;
        let caption = self.send_caption(thread_id, tt, caption).await?;
        let client_id = now_ms();
        let planned = plan_file_send(thread_id, tt, &uploaded, &done.url, &p.name, total_size, &md5_hex(&p.data), client_id);
        let answer = api::run(self.http, self.session, &planned).await?;
        let client_id = number(&planned.params["clientId"]).and_then(|n| i64::try_from(n).ok()).unwrap_or(client_id);
        Ok(SentAttachment {
            kind: UploadKind::File,
            file_name: p.name,
            total_size,
            width: None,
            height: None,
            url: Some(done.url),
            thumb_url: None,
            message: sent(answer, client_id),
            caption,
        })
    }

    /// Send a GIF.
    pub async fn send_gif(&self, thread_id: &str, tt: ThreadType, up: Upload, caption: &str) -> Result<SentAttachment> {
        check_thread(thread_id)?;
        let p = prepare(up, UploadKind::Gif, self.limits).await?;
        let (width, height) = dimensions(&p.data)?;
        let total_size = p.data.len() as u64;
        let thumb = api::run_upload(self.http, self.session, &plan_upthumb(self.imei, now_ms()), p.data.clone()).await?;
        let thumb = thumb["url"].as_str().filter(|u| !u.is_empty()).ok_or_else(|| Error::protocol("the GIF thumbnail upload gave no url"))?.to_string();
        let caption = self.send_caption(thread_id, tt, caption).await?;
        let client_id = now_ms();
        let req = plan_gif(thread_id, tt, &p.name, total_size, width, height, &thumb, &md5_hex(&p.data), client_id);
        let answer = api::run_upload(self.http, self.session, &req, p.data).await?;
        Ok(SentAttachment {
            kind: UploadKind::Gif,
            file_name: p.name,
            total_size,
            width: Some(width),
            height: Some(height),
            url: None,
            thumb_url: Some(thumb),
            message: sent(answer, client_id),
            caption,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ShareLimits {
        ShareLimits { max_files: 20, max_size_mb: 1, chunk_size: 4, restricted_ext: vec!["exe".into(), "BAT".into()] }
    }

    fn settings() -> Value {
        json!({"features": {"sharefile": {
            "max_file": 20, "max_size_share_file_v3": "1024", "chunk_size_file": 3145728,
            "restricted_ext_file": ["exe", "bat"], "max_size_gif": 25
        }}})
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(f)
    }

    #[test]
    fn the_extension_picks_the_kind_like_zca_js() {
        for (ext, kind) in [("jpg", UploadKind::Image), ("JPEG", UploadKind::Image), ("png", UploadKind::Image), ("webp", UploadKind::Image), ("gif", UploadKind::Gif), ("GIF", UploadKind::Gif), ("mp4", UploadKind::File), ("pdf", UploadKind::File), ("", UploadKind::File), ("heic", UploadKind::File)] {
            assert_eq!(UploadKind::of_extension(ext), kind, "{ext}");
        }
    }

    #[test]
    fn names_and_extensions_come_from_the_path_or_the_given_name() {
        let n = |u: Upload| u.name_and_extension().unwrap();
        assert_eq!(n(Upload::path("/tmp/some dir/Photo.JPG")), ("Photo.JPG".into(), "jpg".into()));
        assert_eq!(n(Upload::bytes("report.final.PDF", vec![1])), ("report.final.PDF".into(), "pdf".into()));
        assert_eq!(n(Upload::bytes("Makefile", vec![1])), ("Makefile".into(), String::new()));
        assert_eq!(n(Upload::bytes("../../etc/a.txt", vec![1])).0, "a.txt", "only the file name is sent");
        assert!(matches!(Upload::bytes("", vec![1]).name_and_extension(), Err(Error::Invalid(m)) if m == "Missing filename"));
        assert!(Upload::path("/").name_and_extension().is_err());
    }

    #[test]
    fn the_share_limits_are_read_from_the_login_settings() {
        let l = ShareLimits::from_settings(&settings()).unwrap();
        assert_eq!((l.max_files, l.max_size_mb, l.chunk_size, l.max_bytes()), (20, 1024, 3_145_728, 1024 * 1024 * 1024));
        assert_eq!(l.restricted_ext, ["exe", "bat"]);
        // no block, or one without the two numbers the upload needs: an error, not a guess
        assert!(matches!(ShareLimits::from_settings(&Value::Null), Err(Error::Protocol(m)) if m.contains("max_size_share_file_v3")));
        let no_chunk = json!({"features": {"sharefile": {"max_size_share_file_v3": 100}}});
        assert!(matches!(ShareLimits::from_settings(&no_chunk), Err(Error::Protocol(m)) if m.contains("chunk_size_file")));
    }

    #[test]
    fn files_over_the_limit_or_on_the_blacklist_are_refused_before_any_upload() {
        let l = limits(); // 1 MB, exe / BAT refused
        let over = vec![0u8; 1024 * 1024 + 1];
        let err = block_on(prepare(Upload::bytes("big.zip", over), UploadKind::File, &l)).unwrap_err().to_string();
        assert_eq!(err, "File big.zip size exceed maximum size of 1MB");
        let exactly = vec![0u8; 1024 * 1024];
        assert!(block_on(prepare(Upload::bytes("ok.zip", exactly), UploadKind::File, &l)).is_ok(), "the limit itself is allowed");
        for name in ["run.exe", "RUN.EXE", "a.bat"] {
            let err = block_on(prepare(Upload::bytes(name, vec![1]), UploadKind::File, &l)).unwrap_err().to_string();
            assert!(err.contains("is not allowed"), "{name}: {err}");
        }
        assert!(block_on(prepare(Upload::bytes("e.txt", vec![]), UploadKind::File, &l)).unwrap_err().to_string().contains("is empty"));
        // an image / gif sender refuses what is not one
        assert!(block_on(prepare(Upload::bytes("a.pdf", vec![1]), UploadKind::Image, &l)).unwrap_err().to_string().contains("not a jpg, png or webp image"));
        assert!(block_on(prepare(Upload::bytes("a.png", vec![1]), UploadKind::Gif, &l)).unwrap_err().to_string().contains("not a gif"));
        // the blacklist is not applied to GIFs; the size limit is
        let l2 = ShareLimits { restricted_ext: vec!["gif".into()], ..limits() };
        assert!(block_on(prepare(Upload::bytes("a.gif", vec![1]), UploadKind::Gif, &l2)).is_ok());
        assert!(block_on(prepare(Upload::bytes("a.gif", vec![0; 2 * 1024 * 1024]), UploadKind::Gif, &l2)).is_err());
    }

    #[test]
    fn a_path_is_checked_by_its_metadata_and_then_read() {
        let l = limits();
        let dir = std::env::temp_dir().join(format!("zca-rust-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"hello").unwrap();
        let p = block_on(prepare(Upload::path(&file), UploadKind::File, &l)).unwrap();
        assert_eq!((p.name.as_str(), p.data.as_slice()), ("notes.txt", &b"hello"[..]));
        assert!(matches!(block_on(prepare(Upload::path(dir.join("missing.txt")), UploadKind::File, &l)), Err(Error::Invalid(m)) if m == "File not found"));
        assert!(matches!(block_on(prepare(Upload::path(dir.join("sub.txt")), UploadKind::File, &l)), Err(Error::Invalid(_))));
        std::fs::create_dir_all(dir.join("folder.txt")).unwrap();
        assert!(block_on(prepare(Upload::path(dir.join("folder.txt")), UploadKind::File, &l)).unwrap_err().to_string().contains("is not a file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_checksum_is_the_md5_of_the_whole_content_in_lowercase_hex() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        // bigger than zca-js's 2 MB reading step: the digest is of everything
        assert_eq!(md5_hex(&vec![b'a'; 3 * 1024 * 1024]).len(), 32);
    }

    #[test]
    fn chunks_are_counted_like_math_ceil() {
        assert_eq!([chunk_count(1, 4), chunk_count(4, 4), chunk_count(5, 4), chunk_count(8, 4), chunk_count(9, 4)], [1, 1, 2, 2, 3]);
        let data = [7u8; 9];
        let sizes: Vec<usize> = data.chunks(4).map(<[u8]>::len).collect();
        assert_eq!(sizes, [4, 4, 1]);
    }

    #[test]
    fn a_chunk_goes_to_the_photo_or_the_async_file_upload_with_zca_jss_params() {
        let r = plan_chunk(true, "IMEI", "1000000000000000001", ThreadType::User, "a b.png", 9, 3, 2, 1_700_000_000_000);
        assert_eq!((r.service, r.path, r.nretry, r.field, r.content_type), ("file", "/api/message/photo_original/upload", false, "chunkContent", "application/octet-stream"));
        assert_eq!(r.query, [("type", "2".to_string())]);
        assert_eq!(r.file_name, "a b.png");
        assert_eq!(
            r.params,
            json!({"toid": "1000000000000000001", "totalChunk": 3, "fileName": "a b.png", "clientId": 1_700_000_000_000i64, "totalSize": 9, "imei": "IMEI", "isE2EE": 0, "jxl": 0, "chunkId": 2})
        );
        let g = plan_chunk(false, "IMEI", "2000000000000000002", ThreadType::Group, "r.pdf", 5, 1, 1, 1);
        assert_eq!((g.path, g.query.as_slice()), ("/api/group/asyncfile/upload", &[("type", "11".to_string())][..]));
        assert_eq!((g.params["grid"].as_str(), g.params.get("toid")), (Some("2000000000000000002"), None));
        assert_eq!(plan_chunk(false, "I", "1", ThreadType::User, "r.pdf", 5, 1, 1, 1).path, "/api/message/asyncfile/upload");
        assert_eq!(plan_chunk(true, "I", "1", ThreadType::Group, "r.png", 5, 1, 1, 1).path, "/api/group/photo_original/upload");
    }

    fn uploaded_image() -> ImageUploaded {
        parse_image_upload(&json!({"photoId": 4000000000000000001i64, "finished": 1, "normalUrl": "https://f.example.test/n.jpg", "hdUrl": "https://f.example.test/h.jpg", "thumbUrl": "https://f.example.test/t.jpg", "chunkId": 3, "clientFileId": 5})).unwrap()
    }

    #[test]
    fn an_unfinished_upload_has_no_photo_or_file_id() {
        assert!(parse_image_upload(&json!({"photoId": "-1", "finished": 0, "chunkId": 1})).is_err());
        assert!(parse_image_upload(&json!({"chunkId": 1})).is_err());
        assert!(parse_file_upload(&json!({"fileId": "-1", "finished": 0})).is_err());
        assert!(parse_file_upload(&json!({"fileId": -1})).is_err());
        let f = parse_file_upload(&json!({"fileId": 4000000000000000002i64, "clientFileId": 1_700_000_000_001i64, "finished": 0})).unwrap();
        assert_eq!((f.file_id.as_str(), f.client_file_id.clone()), ("4000000000000000002", json!(1_700_000_000_001i64)));
    }

    #[test]
    fn the_image_message_carries_the_upload_urls_and_the_size() {
        let img = uploaded_image();
        let dm = plan_image_send("1000000000000000001", ThreadType::User, &img, 640, 480, 12345, "a caption", 1_700_000_000_002);
        assert_eq!((dm.service, dm.path, dm.nretry), ("file", "/api/message/photo_original/send", true));
        assert_eq!(
            dm.params,
            json!({"photoId": 4000000000000000001i64, "clientId": "1700000000002", "desc": "a caption", "width": 640, "height": 480, "toid": "1000000000000000001",
                "rawUrl": "https://f.example.test/n.jpg", "hdUrl": "https://f.example.test/h.jpg", "thumbUrl": "https://f.example.test/t.jpg",
                "normalUrl": "https://f.example.test/n.jpg", "hdSize": "12345", "zsource": -1, "ttl": 0, "jcp": "{\"convertible\":\"jxl\"}"})
        );
        let g = plan_image_send("2000000000000000002", ThreadType::Group, &img, 640, 480, 12345, "", 1);
        assert_eq!(g.path, "/api/group/photo_original/send");
        assert_eq!((g.params["grid"].as_str(), g.params["oriUrl"].as_str(), g.params.get("normalUrl"), g.params.get("toid"), g.params["desc"].as_str()), (Some("2000000000000000002"), Some("https://f.example.test/n.jpg"), None, None, Some("")));
        // a missing URL is left out, as an undefined key is in JS
        let bare = ImageUploaded { thumb_url: Value::Null, ..img };
        assert!(plan_image_send("1", ThreadType::User, &bare, 1, 1, 1, "", 1).params.get("thumbUrl").is_none());
    }

    #[test]
    fn the_file_message_carries_the_checksum_the_ws_url_and_the_clients_file_id() {
        let up = FileUploaded { file_id: "4000000000000000002".into(), client_file_id: json!(1_700_000_000_001i64) };
        let p = plan_file_send("2000000000000000002", ThreadType::Group, &up, "https://f.example.test/f/abc", "Report.PDF", 5, "900150983cd24fb0d6963f7d28e17f72", 9);
        assert_eq!((p.service, p.path, p.nretry), ("file", "/api/group/asyncfile/msg", true));
        assert_eq!(
            p.params,
            json!({"fileId": "4000000000000000002", "checksum": "900150983cd24fb0d6963f7d28e17f72", "checksumSha": "", "extention": "PDF", "totalSize": 5, "fileName": "Report.PDF",
                "clientId": 1_700_000_000_001i64, "fType": 1, "fileCount": 0, "fdata": "{}", "grid": "2000000000000000002", "fileUrl": "https://f.example.test/f/abc", "zsource": -1, "ttl": 0})
        );
        let dm = plan_file_send("1000000000000000001", ThreadType::User, &FileUploaded { client_file_id: Value::Null, ..up }, "u", "noext", 1, "c", 77);
        assert_eq!((dm.path, dm.params["toid"].as_str(), dm.params["extention"].as_str(), dm.params["clientId"].as_i64()), ("/api/message/asyncfile/msg", Some("1000000000000000001"), Some(""), Some(77)));
    }

    #[test]
    fn a_gif_has_its_thumbnail_upload_and_one_chunk_with_the_type_in_the_query() {
        let t = plan_upthumb("IMEI", 1_700_000_000_003);
        assert_eq!((t.service, t.path, t.nretry, t.field, t.file_name.as_str(), t.content_type), ("file", "/api/message/upthumb", false, "fileContent", "blob", "image/png"));
        assert!(t.query.is_empty());
        assert_eq!(t.params, json!({"clientId": 1_700_000_000_003i64, "imei": "IMEI"}));
        let g = plan_gif("1000000000000000001", ThreadType::User, "cat.gif", 300, 90, 60, "https://f.example.test/thumb", "abc", 1_700_000_000_004);
        assert_eq!((g.path, g.nretry, g.field, g.file_name.as_str(), g.content_type), ("/api/message/gif", true, "chunkContent", "cat.gif", "application/octet-stream"));
        assert_eq!(g.query, [("type", "1".to_string())]);
        assert_eq!(
            g.params,
            json!({"clientId": "1700000000004", "fileName": "cat.gif", "totalSize": 300, "width": 90, "height": 60, "msg": "", "type": 1, "ttl": 0, "toid": "1000000000000000001",
                "thumb": "https://f.example.test/thumb", "checksum": "abc", "totalChunk": 1, "chunkId": 1})
        );
        let gg = plan_gif("2000000000000000002", ThreadType::Group, "cat.gif", 300, 90, 60, "t", "abc", 1);
        assert_eq!((gg.path, gg.params["visibility"].as_i64(), gg.params["grid"].as_str(), gg.params.get("toid")), ("/api/group/gif", Some(0), Some("2000000000000000002"), None));
    }

    #[test]
    fn the_picture_size_is_read_from_the_header() {
        let img = image::RgbaImage::from_pixel(7, 5, image::Rgba([1, 2, 3, 255]));
        for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg, image::ImageFormat::Gif] {
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(img.clone()).to_rgb8().write_to(&mut std::io::Cursor::new(&mut bytes), format).unwrap();
            assert_eq!(dimensions(&bytes).unwrap(), (7, 5), "{format:?}");
        }
        assert!(matches!(dimensions(b"not a picture"), Err(Error::Invalid(_)) | Err(Error::Io(_))));
    }

    fn control(act: &str, file_id: Value, data: Value) -> Value {
        json!({"content": {"act_type": act, "fileId": file_id, "data": data}})
    }

    #[test]
    fn file_done_controls_are_picked_out_of_a_control_frame() {
        let frame = json!({"data": {"controls": [
            control("file_done", json!(4000000000000000002i64), json!({"url": "https://f.example.test/f/abc"})),
            control("group", json!(1), json!({"url": "https://x"})),
            control("file_done", json!("4000000000000000003"), json!("{\"url\":\"https://f.example.test/f/def\"}")),
            control("file_done", json!(9), json!({"nourl": 1})),
            {"content": {}}
        ]}});
        assert_eq!(
            file_done_events(&frame),
            [FileDone { file_id: "4000000000000000002".into(), url: "https://f.example.test/f/abc".into() }, FileDone { file_id: "4000000000000000003".into(), url: "https://f.example.test/f/def".into() }]
        );
        assert!(file_done_events(&json!({"data": {}})).is_empty());
    }

    fn done(id: &str) -> FileDone {
        FileDone { file_id: id.into(), url: format!("https://f.example.test/f/{id}") }
    }

    #[test]
    fn the_wait_ends_when_the_file_done_event_arrives_and_only_for_its_file() {
        block_on(async {
            let w = std::sync::Arc::new(FileWaiters::default());
            let (w2, w3) = (w.clone(), w.clone());
            // the upload waits for file 2; the listener sees other files first, then file 2
            let waiting = tokio::spawn(async move { w2.wait("2", Duration::from_secs(5)).await });
            let other = tokio::spawn(async move { w3.wait("3", Duration::from_millis(80)).await });
            tokio::time::sleep(Duration::from_millis(20)).await;
            w.done(done("1"));
            w.done(done("2"));
            assert_eq!(waiting.await.unwrap().unwrap(), done("2"));
            // file 3 never finishes: a timeout, and the waiter is gone
            let err = other.await.unwrap().unwrap_err().to_string();
            assert!(err.contains("did not confirm the upload"), "{err}");
            assert!(w.state().waiting.is_empty());
        });
    }

    #[test]
    fn an_event_that_beats_the_upload_answer_is_not_lost() {
        block_on(async {
            let w = FileWaiters::default();
            w.done(done("7"));
            assert_eq!(w.wait("7", Duration::from_millis(50)).await.unwrap(), done("7"));
            // handed over once
            assert!(w.wait("7", Duration::from_millis(20)).await.is_err());
            // the early list is bounded: the oldest are dropped
            for i in 0..(EARLY_MAX + 5) {
                w.done(done(&format!("e{i}")));
            }
            assert_eq!(w.state().early.len(), EARLY_MAX);
            assert!(w.wait("e0", Duration::from_millis(20)).await.is_err());
            assert!(w.wait(&format!("e{}", EARLY_MAX + 4), Duration::from_millis(20)).await.is_ok());
        });
    }
}
