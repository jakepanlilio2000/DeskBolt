use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deskbolt_proto::system_audio::{SystemAudioControls, SystemAudioPlayer};
use deskbolt_proto::video_pipeline::{VideoCapabilities, platform_video_decoders};

use crate::recording::{Record, Recording, Track};
use crate::recording_export::{self, Format};
use crate::session::{DecodedFrame, MonitorDecoder, SessionCommand, ViewerSession, video_metadata};

pub(crate) fn recording_path() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    crate::db::default_phonebook_path().with_file_name(format!("Session-{now}.dbrec"))
        .to_string_lossy().into_owned()
}

#[derive(Default)]
struct View {
    monitors: Vec<u8>,
    duration: u64,
    position: u64,
    frame: Option<Arc<DecodedFrame>>,
    playing: bool,
    status: String,
}

enum Command {
    Playing(bool),
    Seek { ms: u64, monitor: u8 },
    Speed(u32),
}

struct Playback {
    source: PathBuf,
    command: mpsc::Sender<Command>,
    view: Arc<Mutex<View>>,
}

impl Playback {
    fn open(path: PathBuf, context: egui::Context) -> io::Result<Self> {
        let (command, receive) = mpsc::channel();
        let view = Arc::new(Mutex::new(View::default()));
        let worker_view = view.clone();
        let source = path.clone();
        std::thread::Builder::new().name("session-replay".into()).spawn(move || {
            if let Err(error) = play(path, receive, &worker_view, &context) {
                let mut view = worker_view.lock().unwrap();
                view.playing = false;
                view.status = error.to_string();
            }
            context.request_repaint();
        })?;
        Ok(Self { source, command, view })
    }
}

pub(crate) fn decode_record(
    record: Record, display: u8, decoder: &mut MonitorDecoder, capabilities: &mut VideoCapabilities,
) -> io::Result<bool> {
    if record.track != Track::Video || record.monitor != display {
        return Ok(false);
    }
    let msg = record.video().map_err(io::Error::other)?;
    let Some((id, _, codec, _)) = video_metadata(&msg) else { return Ok(false) };
    if id != display {
        return Err(io::Error::other("The recorded display does not match its index."));
    }
    Ok(decoder.decode(&msg, codec, capabilities).is_some())
}

fn play(
    path: PathBuf, commands: mpsc::Receiver<Command>, view: &Mutex<View>, ctx: &egui::Context,
) -> io::Result<()> {
    let mut file = Recording::open(&path)?;
    let mut display = file.monitors().first().copied()
        .ok_or_else(|| io::Error::other("This recording contains no display keyframes."))?;
    let duration = file.duration_ms;
    let mut position = file.seek(display, 0)?;
    let mut capabilities = platform_video_decoders();
    let mut decoder = MonitorDecoder::default();
    let mut pending = file.next()?;
    let mut origin = (Instant::now(), position);
    let mut playing = false;
    let mut speed = 2;
    let mut players: [Option<SystemAudioPlayer>; 3] = [None, None, None];
    let mut audio_attempted = [false; 3];
    let audio_controls = Arc::new(SystemAudioControls::default());
    let mut seq = 0;
    {
        let mut output = view.lock().unwrap();
        output.monitors = file.monitors();
        output.duration = duration;
    }

    loop {
        let mut changed = false;
        while pending.as_ref().is_some_and(|record| record.ms <= position) {
            let record = pending.take().unwrap();
            if record.track == Track::Video {
                changed |= decode_record(record, display, &mut decoder, &mut capabilities)?;
            } else if playing {
                let index = record.track as usize - 1;
                if !audio_attempted[index] {
                    audio_attempted[index] = true;
                    let mono = record.track != Track::SystemAudio;
                    match SystemAudioPlayer::start_track(None, audio_controls.clone(), mono, speed) {
                        Ok(player) => players[index] = Some(player),
                        Err(error) => view.lock().unwrap().status = format!("Audio playback unavailable. {error}"),
                    }
                }
                if let Some(player) = &mut players[index] {
                    player.push(&record.data);
                }
            }
            pending = file.next()?;
        }
        {
            let mut output = view.lock().unwrap();
            if changed {
                seq += 1;
                output.frame = Some(Arc::new(DecodedFrame {
                    seq, width: decoder.width, height: decoder.height, rgba: decoder.rgba.clone(),
                }));
            }
            if playing && position >= duration {
                playing = false;
            }
            output.position = position;
            output.playing = playing;
        }
        ctx.request_repaint();
        let next = if playing {
            commands.recv_timeout(Duration::from_millis(15))
        } else {
            commands.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match next {
            Ok(command) => match command {
                Command::Playing(on) => {
                    if !on {
                        players = [None, None, None];
                        audio_attempted = [false; 3];
                    } else if position >= duration {
                        position = file.seek(display, 0)?;
                        decoder = MonitorDecoder::default();
                        pending = file.next()?;
                        players = [None, None, None];
                        audio_attempted = [false; 3];
                    }
                    origin = (Instant::now(), position);
                    playing = on;
                }
                Command::Seek { ms, monitor } => {
                    display = monitor;
                    position = ms.min(duration).max(file.seek(display, ms)?);
                    pending = file.next()?;
                    // Decode the preroll silently, then resume audio at the chosen time.
                    decoder = MonitorDecoder::default();
                    while pending.as_ref().is_some_and(|record| record.ms < position) {
                        let record = pending.take().unwrap();
                        decode_record(record, display, &mut decoder, &mut capabilities)?;
                        pending = file.next()?;
                    }
                    if !decoder.rgba.is_empty() {
                        seq += 1;
                        view.lock().unwrap().frame = Some(Arc::new(DecodedFrame {
                            seq, width: decoder.width, height: decoder.height, rgba: decoder.rgba.clone(),
                        }));
                    }
                    players = [None, None, None];
                    audio_attempted = [false; 3];
                    origin = (Instant::now(), position);
                }
                Command::Speed(value) => {
                    speed = value;
                    players = [None, None, None];
                    audio_attempted = [false; 3];
                    origin = (Instant::now(), position);
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                position = origin.1.saturating_add((origin.0.elapsed().as_millis() as u64 * u64::from(speed)) / 2).min(duration);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

struct ExportJob {
    done: mpsc::Receiver<io::Result<()>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for ExportJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(crate) struct ReplayUi {
    pub(crate) open: bool,
    pub(crate) save: Option<(u64, String)>,
    path: String,
    error: String,
    playback: Option<Playback>,
    monitor: Option<u8>,
    speed: u32,
    texture: Option<(u64, egui::TextureHandle)>,
    export_path: Option<(Format, String)>,
    export: Option<ExportJob>,
    export_status: String,
}

impl ReplayUi {
    fn export_controls(&mut self, ui: &mut egui::Ui, source: PathBuf, monitor: u8) {
        if let Some(export) = &self.export {
            match export.done.try_recv() {
                Ok(result) => {
                    self.export_status = result.map_or_else(|error| error.to_string(), |()| "Export saved.".into());
                    self.export = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.export_status = "The export worker stopped.".into();
                    self.export = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if ui.button("Cancel Export").clicked() {
                        export.cancelled.store(true, Ordering::Relaxed);
                    }
                }
            }
        } else {
            ui.horizontal(|ui| {
                for (label, ext, format) in [("Export MP4", "mp4", Format::Mp4), ("Export WebM", "webm", Format::Webm)] {
                    if ui.button(label).clicked() {
                        self.export_path = Some((format, source.with_extension(ext).to_string_lossy().into_owned()));
                    }
                }
            });
            if let Some((format, path)) = &mut self.export_path {
                let mut close = false;
                ui.horizontal(|ui| {
                    ui.label("Export file");
                    ui.text_edit_singleline(path);
                    let target = PathBuf::from(path.trim());
                    let ext = match format { Format::Mp4 => "mp4", Format::Webm => "webm" };
                    if ui.add_enabled(target.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext)), egui::Button::new("Save")).clicked() {
                        let (send, done) = mpsc::channel();
                        let cancelled = Arc::new(AtomicBool::new(false));
                        let flag = cancelled.clone();
                        let context = ui.ctx().clone();
                        let format = *format;
                        match std::thread::Builder::new().name("recording-export".into()).spawn(move || {
                            let _ = send.send(recording_export::export(&source, &target, monitor, format, &flag));
                            context.request_repaint();
                        }) {
                            Ok(_) => {
                                self.export = Some(ExportJob { done, cancelled });
                                self.export_status = "Exporting the selected display.".into();
                            }
                            Err(error) => self.export_status = error.to_string(),
                        }
                        close = true;
                    }
                    close |= ui.button("Cancel").clicked();
                });
                if close {
                    self.export_path = None;
                }
            }
        }
        if !self.export_status.is_empty() {
            ui.label(&self.export_status);
        }
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context, sessions: &mut [ViewerSession]) {
        if let Some((id, path)) = &mut self.save {
            let mut close = false;
            egui::Window::new("Record Session").collapsible(false).show(ctx, |ui| {
                ui.label("Save the session video and active audio to this device.");
                ui.label("Recording file");
                ui.text_edit_singleline(path);
                ui.horizontal(|ui| {
                    let path = PathBuf::from(path.trim());
                    let valid = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("dbrec"));
                    if ui.add_enabled(valid, egui::Button::new("Start Recording")).clicked() {
                        if let Some(session) = sessions.iter_mut().find(|session| session.id == *id) {
                            session.recording_pending = session.cmd_tx.try_send(SessionCommand::StartRecording(path)).is_ok();
                        }
                        close = true;
                    }
                    close |= ui.button("Cancel").clicked();
                });
            });
            if close {
                self.save = None;
            }
        }
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Session Replay Player").open(&mut open).default_width(660.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Recording file");
                ui.text_edit_singleline(&mut self.path);
                if ui.button("Open Recording").clicked() {
                    self.playback = None;
                    self.export_path = None;
                    self.monitor = None;
                    self.texture = None;
                    self.speed = 2;
                    match Playback::open(PathBuf::from(self.path.trim()), ctx.clone()) {
                        Ok(playback) => { self.playback = Some(playback); self.error.clear(); }
                        Err(error) => self.error = error.to_string(),
                    }
                }
            });
            if !self.error.is_empty() {
                ui.label(&self.error);
            }
            let Some(playback) = &self.playback else { return };
            let input = playback.view.lock().unwrap();
            let (monitors, frame, playing, duration, mut ms, status) = (
                input.monitors.clone(), input.frame.clone(), input.playing, input.duration, input.position, input.status.clone(),
            );
            drop(input);
            if !status.is_empty() {
                ui.label(status);
            }
            let Some(initial) = monitors.first().copied() else { return };
            let monitor = self.monitor.get_or_insert(initial);
            let original = *monitor;
            ui.horizontal(|ui| {
                if ui.button(if playing { "Pause" } else { "Play" }).clicked() {
                    let _ = playback.command.send(Command::Playing(!playing));
                }
                egui::ComboBox::from_id_salt("replay_speed")
                    .selected_text(format!("{}×", f64::from(self.speed) / 2.0))
                    .show_ui(ui, |ui| {
                        for speed in [1, 2, 4, 8] {
                            if ui.selectable_value(&mut self.speed, speed, format!("{}×", f64::from(speed) / 2.0)).clicked() {
                                let _ = playback.command.send(Command::Speed(speed));
                            }
                        }
                    });
                egui::ComboBox::from_id_salt("replay_display").selected_text(format!("Display {}", u16::from(*monitor) + 1))
                    .show_ui(ui, |ui| for id in monitors {
                        ui.selectable_value(monitor, id, format!("Display {}", u16::from(id) + 1));
                    });
            });
            let seek = ui.add(egui::Slider::new(&mut ms, 0..=duration).show_value(false).text("Timeline"));
            if seek.changed() || *monitor != original {
                let _ = playback.command.send(Command::Seek { ms, monitor: *monitor });
            }
            ui.label(format!("{:.1} / {:.1} s", ms as f64 / 1000.0, duration as f64 / 1000.0));
            if let Some(frame) = frame {
                if self.texture.as_ref().is_none_or(|(seq, _)| *seq != frame.seq) {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize], &frame.rgba,
                    );
                    if let Some((seq, tex)) = &mut self.texture {
                        tex.set(image, egui::TextureOptions::LINEAR);
                        *seq = frame.seq;
                    } else {
                        self.texture = Some((frame.seq, ctx.load_texture("session-replay", image, egui::TextureOptions::LINEAR)));
                    }
                }
                if let Some((_, tex)) = &self.texture {
                    ui.add(egui::Image::new(tex).max_size(egui::vec2(ui.available_width(), 480.0)));
                }
            }
            let source = playback.source.clone();
            let monitor = *monitor;
            self.export_controls(ui, source, monitor);
        });
        self.open = open;
        if !open {
            self.playback = None;
            self.export_path = None;
            self.texture = None;
        }
    }
}
