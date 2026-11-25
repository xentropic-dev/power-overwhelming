use anyhow::{Context as _, Result, bail};
use domain::artifact::Target;
use gpui::{prelude::*, *};
use gpui_video_player::{
    Video,
    gst::{self, prelude::*},
    video,
};
use image::{Frame, RgbaImage};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tiny_http::{Header, Response, Server, StatusCode};
use url::Url;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/power-webkit"));

/// Keyboard actions on the active preview. Each applies to the preview types it fits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PreviewAction {
    TogglePlay,
    /// Seek by this many seconds.
    Seek(i32),
    /// Scroll by this many lines; positive scrolls down.
    Scroll(f32),
    Reload,
    /// Send keyboard input to a web page.
    Interact,
}

pub enum Preview {
    Image(Arc<RenderImage>),
    Video(Entity<VideoView>),
    Web(Entity<WebView>),
    Error(String),
}

impl Preview {
    pub fn open(target: &Target, window: &mut Window, cx: &mut App) -> Self {
        let opened = (|| -> Result<Self> {
            Ok(match target {
                Target::Image(path) => {
                    let image = image::ImageReader::open(path)
                        .with_context(|| format!("Image is unavailable at {}", path.display()))?
                        .with_guessed_format()?
                        .decode()
                        .with_context(|| format!("Could not decode {}", path.display()))?
                        .into_rgba8();
                    let (width, height) = image.dimensions();
                    let mut data = image.into_raw();
                    for pixel in data.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                    Self::Image(render_image(width, height, data).context("Invalid image size")?)
                }
                Target::Video(path) => {
                    if !path.is_file() {
                        bail!("Video is unavailable at {}", path.display());
                    }
                    let uri = Url::from_file_path(path)
                        .map_err(|_| anyhow::anyhow!("Invalid video path"))?;
                    let playback = Video::new(&uri)?;
                    playback.set_muted(false);
                    Self::Video(cx.new(|cx| VideoView::new(playback, window, cx)))
                }
                Target::Html(path) => {
                    let host = LocalPage::serve(path)?;
                    Self::Web(WebView::open(host.url.clone(), Some(host), window, cx)?)
                }
                Target::Loopback(url) => Self::Web(WebView::open(url.clone(), None, window, cx)?),
            })
        })();
        opened.unwrap_or_else(|error| Self::Error(format!("{error:#}")))
    }

    pub fn render(&self, colors: &infrastructure::theme::Colors) -> AnyElement {
        match self {
            Self::Image(image) => img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            Self::Video(view) => view.clone().into_any_element(),
            Self::Web(view) => view.clone().into_any_element(),
            Self::Error(message) => div()
                .p_4()
                .text_color(rgb(colors.error))
                .child(message.clone())
                .into_any_element(),
        }
    }

    /// Applies a keyboard action. Returns false when it does not fit this preview.
    pub fn act(&self, action: PreviewAction, window: &mut Window, cx: &mut App) -> bool {
        match (self, action) {
            (Self::Video(view), PreviewAction::TogglePlay) => {
                view.update(cx, |view, cx| view.toggle_play(cx));
            }
            (Self::Video(view), PreviewAction::Seek(seconds)) => {
                view.update(cx, |view, cx| view.seek_by(seconds, cx));
            }
            (Self::Web(view), PreviewAction::Scroll(lines)) => view.update(cx, |view, _| {
                let center = view.bounds.size / 2.;
                view.send(json!({"cmd":"scroll","x":f32::from(center.width),
                    "y":f32::from(center.height),"dx":0.,"dy":lines,"mods":0}));
            }),
            (Self::Web(view), PreviewAction::Reload) => {
                view.update(cx, |view, _| view.send(json!({"cmd":"reload"})));
            }
            (Self::Web(view), PreviewAction::Interact) => view.read(cx).focus.focus(window),
            _ => return false,
        }
        true
    }

    pub fn set_active(&self, active: bool, cx: &mut App) {
        match self {
            Self::Video(view) if !active => view.update(cx, |this, _| this.video.set_paused(true)),
            Self::Web(view) => view.update(cx, |this, _| {
                this.send(json!({"cmd":"visible", "value": usize::from(active)}));
            }),
            _ => {}
        }
    }

    pub fn close(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Image(image) => {
                let _ = window.drop_image(image.clone());
            }
            Self::Web(view) => view.update(cx, |view, _| {
                if let Some(frame) = view.frame.take() {
                    let _ = window.drop_image(frame);
                }
            }),
            _ => self.set_active(false, cx),
        }
    }
}

fn render_image(width: u32, height: u32, pixels: Vec<u8>) -> Option<Arc<RenderImage>> {
    let frame = Frame::new(RgbaImage::from_raw(width, height, pixels)?);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

pub struct VideoView {
    video: Video,
    error: Arc<Mutex<Option<String>>>,
    seek_error: Option<String>,
}

impl VideoView {
    fn new(video: Video, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let error = Arc::new(Mutex::new(None));
        if let Some(bus) = video.pipeline().bus() {
            let error = error.clone();
            bus.set_sync_handler(move |_, message| {
                if let gst::MessageView::Error(details) = message.view() {
                    *error.lock().unwrap() = Some(details.error().to_string());
                }
                gst::BusSyncReply::Pass
            });
        }
        cx.spawn_in(window, async |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if view.update_in(cx, |_, _, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            video,
            error,
            seek_error: None,
        }
    }
}

impl VideoView {
    fn seek_by(&mut self, seconds: i32, cx: &mut Context<Self>) {
        let position = self.video.position().as_secs_f64();
        let target = (position + f64::from(seconds)).clamp(0., self.video.duration().as_secs_f64());
        self.seek_error = self
            .video
            .seek(Duration::from_secs_f64(target), true)
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let ended = self.video.eos();
        let paused = self.video.paused();
        if ended {
            self.seek_error = self
                .video
                .seek(Duration::ZERO, true)
                .err()
                .map(|error| error.to_string());
        }
        self.video.set_paused(!paused && !ended);
        cx.notify();
    }
}

impl Render for VideoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = crate::gui::colors(cx);
        let paused = self.video.paused() || self.video.eos();
        let position = self.video.position();
        let duration = self.video.duration();
        let seek = |seconds: i32| {
            cx.listener(move |view: &mut Self, _: &ClickEvent, _, cx| view.seek_by(seconds, cx))
        };
        let error = self
            .error
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.seek_error.clone());
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(video(self.video.clone()).id("preview-video")),
            )
            .when_some(error, |this, message| {
                this.child(div().px_2().text_color(rgb(colors.error)).child(message))
            })
            .child(
                div()
                    .p_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::gui::button(colors, "back", "−5 s").on_click(seek(-5)))
                    .child(
                        crate::gui::button(colors, "play", if paused { "Play" } else { "Pause" })
                            .on_click(
                                cx.listener(|view, _: &ClickEvent, _, cx| view.toggle_play(cx)),
                            ),
                    )
                    .child(crate::gui::button(colors, "forward", "+5 s").on_click(seek(5)))
                    .child(format!("{} / {}", clock(position), clock(duration))),
            )
    }
}

fn clock(duration: Duration) -> String {
    format!("{}:{:02}", duration.as_secs() / 60, duration.as_secs() % 60)
}

struct LocalPage {
    url: String,
    stop: Arc<AtomicBool>,
}

impl LocalPage {
    fn serve(entry: &Path) -> Result<Self> {
        let entry = entry
            .canonicalize()
            .context("HTML artifact is unavailable")?;
        anyhow::ensure!(entry.is_file(), "HTML artifact is unavailable");
        let root = entry
            .parent()
            .context("HTML file requires a directory")?
            .to_owned();
        let file = entry
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid HTML filename")?;
        let server = Server::http("127.0.0.1:0").map_err(|error| anyhow::anyhow!("{error}"))?;
        let authority = server.server_addr().to_string();
        let mut url = Url::parse(&format!("http://{authority}"))?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid preview address"))?
            .push(file);
        let stop = Arc::new(AtomicBool::new(false));
        let worker = stop.clone();
        thread::spawn(move || {
            while !worker.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(150)) {
                    Ok(Some(request)) => serve_asset(request, &root, &authority),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            url: url.into(),
            stop,
        })
    }
}

impl Drop for LocalPage {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve_asset(request: tiny_http::Request, root: &Path, authority: &str) {
    let expected_host = request
        .headers()
        .iter()
        .any(|header| header.field.equiv("Host") && header.value.as_str() == authority);
    if !expected_host
        || !matches!(
            request.method(),
            tiny_http::Method::Get | tiny_http::Method::Head
        )
    {
        let _ = request.respond(Response::empty(StatusCode(403)));
        return;
    }
    let path = request.url().split('?').next().unwrap_or("/");
    let asset = percent_decode_str(path.trim_start_matches('/'))
        .decode_utf8()
        .ok()
        .and_then(|path| root.join(path.as_ref()).canonicalize().ok())
        .filter(|path| path.starts_with(root) && path.is_file());
    let Some((path, file)) = asset.and_then(|path| File::open(&path).ok().map(|file| (path, file)))
    else {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return;
    };
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let content_type = Header::from_bytes("Content-Type", mime).unwrap();
    let _ = request.respond(Response::from_file(file).with_header(content_type));
}

#[derive(Default)]
struct WebUpdate {
    frame: Option<Arc<RenderImage>>,
    error: Option<Option<String>>,
}

pub struct WebView {
    child: Child,
    commands: async_channel::Sender<Value>,
    _helper: tempfile::TempDir,
    _page: Option<LocalPage>,
    frame: Option<Arc<RenderImage>>,
    error: Option<String>,
    focus: FocusHandle,
    bounds: Bounds<Pixels>,
    viewport: (u32, u32),
}

impl WebView {
    fn open(
        url: String,
        page: Option<LocalPage>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("power-webkit");
        fs::write(&executable, HELPER)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        let mut child = Command::new(executable)
            .env("GDK_BACKEND", "wayland")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("Start the WebKit preview helper")?;
        let stdout = child
            .stdout
            .take()
            .context("WebKit helper output unavailable")?;
        let mut stdin = child
            .stdin
            .take()
            .context("WebKit helper input unavailable")?;
        let (commands, requests) = async_channel::bounded::<Value>(128);
        thread::spawn(move || {
            while let Ok(request) = requests.recv_blocking() {
                if send_web(&mut stdin, &request).is_err() {
                    break;
                }
            }
        });
        let latest = Arc::new(Mutex::new(WebUpdate::default()));
        let (notify, updates) = async_channel::bounded(1);
        let frames = latest.clone();
        thread::spawn(move || {
            if let Err(error) = read_web(BufReader::new(stdout), &frames, &notify) {
                frames.lock().unwrap().error = Some(Some(format!("{error:#}")));
                let _ = notify.try_send(());
            }
        });

        Ok(cx.new(|cx| {
            cx.spawn_in(window, async move |view, cx| {
                while updates.recv().await.is_ok() {
                    let WebUpdate { frame, error } = std::mem::take(&mut *latest.lock().unwrap());
                    if view
                        .update_in(cx, |view: &mut Self, window, cx| {
                            if let Some(frame) = frame
                                && let Some(old) = view.frame.replace(frame)
                            {
                                let _ = window.drop_image(old);
                            }
                            if let Some(error) = error {
                                view.error = error;
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            let view = Self {
                child,
                commands,
                _helper: directory,
                _page: page,
                frame: None,
                error: None,
                focus: cx.focus_handle(),
                bounds: Bounds::default(),
                viewport: (0, 0),
            };
            view.send(json!({"cmd":"create"}));
            view.send(json!({"cmd":"load","url":url}));
            view
        }))
    }

    fn send(&self, mut value: Value) {
        value["id"] = 1.into();
        let _ = self.commands.try_send(value);
    }

    fn set_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;
        let viewport = (
            f32::from(bounds.size.width).clamp(1., 4096.) as u32,
            f32::from(bounds.size.height).clamp(1., 4096.) as u32,
        );
        if viewport != self.viewport {
            self.viewport = viewport;
            self.send(json!({"cmd":"resize","width":viewport.0,"height":viewport.1,"scale":1}));
        }
    }

    fn pointer(&self, command: &str, position: Point<Pixels>, button: u32, modifiers: Modifiers) {
        let position = position - self.bounds.origin;
        self.send(json!({"cmd":command, "x":f32::from(position.x), "y":f32::from(position.y),
            "button":button, "mods":mod_mask(modifiers) | if command == "move" && button == 1 { 256 } else { 0 }}));
    }

    fn key(&self, command: &str, keystroke: &Keystroke) {
        let key = match keystroke.key.as_str() {
            "enter" => "Return",
            "escape" => "Escape",
            "backspace" => "BackSpace",
            "tab" => "Tab",
            "up" => "Up",
            "down" => "Down",
            "left" => "Left",
            "right" => "Right",
            "delete" => "Delete",
            "home" => "Home",
            "end" => "End",
            "pageup" => "Page_Up",
            "pagedown" => "Page_Down",
            "space" => "space",
            other => other,
        };
        let key = keystroke
            .key_char
            .as_deref()
            .filter(|text| {
                text.chars().count() == 1
                    && !text.chars().any(char::is_control)
                    && !keystroke.modifiers.control
            })
            .unwrap_or(key);
        self.send(
            json!({"cmd":command,"key":key, "text":keystroke.key_char.as_deref().unwrap_or(""),
            "mods":mod_mask(keystroke.modifiers)}),
        );
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.commands.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Render for WebView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = crate::gui::colors(cx);
        let entity = cx.entity();
        let frame = self.frame.clone();
        div().id("web-preview").relative().size_full().overflow_hidden().bg(rgb(colors.background))
            .track_focus(&self.focus)
            .child(canvas(move |bounds, _, cx| entity.update(cx, |view, _| view.set_bounds(bounds)),
                move |bounds, (), window, _| {
                    if let Some(frame) = frame {
                        let _ = window.paint_image(bounds, Corners::default(), frame, 0, false);
                    }
                }).absolute().size_full())
            .when(self.frame.is_none() && self.error.is_none(), |this| this.child(div().p_3().child("Loading preview…")))
            .when_some(self.error.clone(), |this, error| this.child(div().absolute().top_0().left_0().right_0()
                .p_3().bg(rgb(colors.surface)).text_color(rgb(colors.error)).child(error)
                .child(crate::gui::button(colors, "retry", "Reload").on_click(cx.listener(|view, _: &ClickEvent, _, _| {
                    view.send(json!({"cmd":"reload"}));
                })))))
            .on_mouse_down(MouseButton::Left, cx.listener(|view, event: &MouseDownEvent, window, _| {
                view.focus.focus(window);
                view.pointer("down", event.position, 1, event.modifiers);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|view, event: &MouseUpEvent, _, _| {
                view.pointer("up", event.position, 1, event.modifiers);
            }))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, _| {
                view.pointer("move", event.position, u32::from(event.pressed_button == Some(MouseButton::Left)), event.modifiers);
            }))
            .on_scroll_wheel(cx.listener(|view, event: &ScrollWheelEvent, _, _| {
                let local = event.position - view.bounds.origin;
                let delta = event.delta.pixel_delta(px(45.));
                view.send(json!({"cmd":"scroll","x":f32::from(local.x),"y":f32::from(local.y),
                    "dx":-f32::from(delta.x)/45.,"dy":-f32::from(delta.y)/45., "mods":mod_mask(event.modifiers)}));
            }))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, _| view.key("key_down", &event.keystroke)))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, _, _| view.key("key_up", &event.keystroke)))
    }
}

fn mod_mask(modifiers: Modifiers) -> u32 {
    u32::from(modifiers.shift)
        | (u32::from(modifiers.control) << 2)
        | (u32::from(modifiers.alt) << 3)
}

fn send_web(mut destination: impl Write, request: &Value) -> Result<()> {
    let data = serde_json::to_vec(request)?;
    destination.write_all(&u32::try_from(data.len())?.to_le_bytes())?;
    destination.write_all(&data)?;
    Ok(())
}

fn read_web(
    mut source: impl BufRead,
    latest: &Mutex<WebUpdate>,
    notify: &async_channel::Sender<()>,
) -> Result<()> {
    loop {
        let mut header = [0; 9];
        source
            .read_exact(&mut header)
            .context("The browser preview process stopped")?;
        let length = u32::from_le_bytes(header[5..9].try_into()?) as usize;
        if length > 68 * 1024 * 1024 {
            bail!("The browser sent an oversized frame");
        }
        let mut bytes = vec![0; length];
        source.read_exact(&mut bytes)?;
        match header[0] {
            b'F' if bytes.len() >= 12 => {
                let width = u32::from_le_bytes(bytes[..4].try_into()?);
                let height = u32::from_le_bytes(bytes[4..8].try_into()?);
                latest.lock().unwrap().frame = render_image(width, height, bytes.split_off(12));
            }
            b'S' => {
                let state: Value = serde_json::from_slice(&bytes)?;
                latest.lock().unwrap().error = Some(state["error"].as_str().map(str::to_owned));
            }
            b'E' => {
                latest.lock().unwrap().error =
                    Some(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            _ => continue,
        }
        let _ = notify.try_send(());
    }
}
