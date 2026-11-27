use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, SyncSender}};
use std::time::Instant;

use deskbolt_proto::{ChannelId, Frame, MAX_FRAME_PAYLOAD};

const MAGIC: &[u8; 8] = b"DBREC01\0";
const FOOTER: &[u8; 8] = b"DBINDEX\0";
const RECORD_HEADER: u64 = 15;
const INDEX_ENTRY: u64 = 17;
const MAX_INDEX: usize = 1_000_000;
const MAX_AUDIO_BYTES: usize = 1279;
pub(crate) const AUDIO_RESET: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Track {
    Video = 0,
    RemoteVoice = 1,
    LocalVoice = 2,
    SystemAudio = 3,
}
impl TryFrom<u8> for Track {
    type Error = io::Error;
    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Video),
            1 => Ok(Self::RemoteVoice),
            2 => Ok(Self::LocalVoice),
            3 => Ok(Self::SystemAudio),
            _ => Err(invalid("Unknown recording track.")),
        }
    }
}

pub(crate) struct Record {
    pub(crate) ms: u64,
    pub(crate) track: Track,
    pub(crate) monitor: u8,
    pub(crate) flags: u8,
    pub(crate) data: Vec<u8>,
}

impl Record {
    pub(crate) fn audio_reset(&self) -> bool { self.track != Track::Video && self.flags == AUDIO_RESET }

    pub(crate) fn video(self) -> Result<deskbolt_proto::VideoMsg, deskbolt_proto::ProtoError> {
        deskbolt_proto::unpack_message(&Frame {
            channel: ChannelId::VideoTiles, flags: self.flags, payload: self.data,
        })
    }
}

#[derive(Clone, Copy)]
struct Key {
    ms: u64,
    offset: u64,
    monitor: u8,
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

pub(crate) fn new_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

struct Writer<W> {
    output: W,
    keys: Vec<Key>,
    offset: u64,
    duration_ms: u64,
}

impl<W: Write> Writer<W> {
    fn new(mut output: W) -> io::Result<Self> {
        output.write_all(MAGIC)?;
        Ok(Self { output, keys: Vec::new(), offset: 8, duration_ms: 0 })
    }

    fn push(&mut self, record: &Record, key: bool) -> io::Result<()> {
        if key && self.keys.len() == MAX_INDEX {
            return Err(invalid("The recording reached its keyframe limit."));
        }
        if key {
            self.keys.push(Key { ms: record.ms, offset: self.offset, monitor: record.monitor });
        }
        self.output.write_all(&record.ms.to_le_bytes())?;
        self.output.write_all(&[record.track as u8, record.monitor, record.flags])?;
        self.output.write_all(&(record.data.len() as u32).to_le_bytes())?;
        self.output.write_all(&record.data)?;
        self.offset += RECORD_HEADER + record.data.len() as u64;
        self.duration_ms = record.ms;
        Ok(())
    }

    fn finish(mut self) -> io::Result<W> {
        for key in self.keys {
            self.output.write_all(&key.ms.to_le_bytes())?;
            self.output.write_all(&key.offset.to_le_bytes())?;
            self.output.write_all(&[key.monitor])?;
        }
        self.output.write_all(&self.offset.to_le_bytes())?;
        self.output.write_all(&self.duration_ms.to_le_bytes())?;
        self.output.write_all(FOOTER)?;
        self.output.flush()?;
        Ok(self.output)
    }
}

pub(crate) struct Recorder {
    tx: SyncSender<(Record, bool)>,
    start: Instant,
    overrun: Arc<AtomicBool>,
}

impl Recorder {
    pub(crate) fn start(
        path: &Path,
        finished: impl FnOnce(io::Result<()>) + Send + 'static,
    ) -> io::Result<Self> {
        let mut writer = Writer::new(BufWriter::new(new_private_file(path)?))?;
        let (tx, rx) = mpsc::sync_channel(8);
        let overrun = Arc::new(AtomicBool::new(false));
        let stopped = overrun.clone();
        std::thread::Builder::new().name("session-recording".into()).spawn(move || {
            let result = (|| {
                while let Ok((record, key)) = rx.recv() {
                    writer.push(&record, key)?;
                }
                writer.finish()?.get_ref().sync_all()?;
                if stopped.load(Ordering::Relaxed) {
                    return Err(io::Error::other("The disk could not keep up. The shorter recording was saved."));
                }
                Ok(())
            })();
            finished(result);
        })?;
        Ok(Self { tx, start: Instant::now(), overrun })
    }

    pub(crate) fn queue(&self, track: Track, monitor: u8, flags: u8, data: &[u8], key: bool) -> bool {
        if track != Track::Video
            && !(flags == AUDIO_RESET && data.is_empty())
            && (flags != 0 || !deskbolt_proto::valid_audio_packet(data))
        {
            return true;
        }
        let queued = self.tx.try_send((Record {
            ms: self.start.elapsed().as_millis() as u64,
            track, monitor, flags, data: data.to_vec(),
        }, key)).is_ok();
        if !queued { self.overrun.store(true, Ordering::Relaxed); }
        queued
    }
}

pub(crate) struct Recording<R = BufReader<File>> {
    input: R,
    keys: Vec<Key>,
    end: u64,
    pub(crate) duration_ms: u64,
}

impl Recording {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        Self::read(BufReader::new(File::open(path)?))
    }
}

impl<R: Read + Seek> Recording<R> {
    fn read(mut input: R) -> io::Result<Self> {
        let length = input.seek(SeekFrom::End(0))?;
        if length < 32 {
            return Err(invalid("Recording is incomplete."));
        }
        input.rewind()?;
        if read::<8>(&mut input)? != *MAGIC {
            return Err(invalid("Unsupported recording format."));
        }
        input.seek(SeekFrom::End(-24))?;
        let end = u64::from_le_bytes(read(&mut input)?);
        let duration_ms = u64::from_le_bytes(read(&mut input)?);
        if read::<8>(&mut input)? != *FOOTER || !(8..=length - 24).contains(&end) {
            return Err(invalid("Recording index is missing."));
        }
        let index_bytes = length - 24 - end;
        if !index_bytes.is_multiple_of(INDEX_ENTRY) || index_bytes / INDEX_ENTRY > MAX_INDEX as u64 {
            return Err(invalid("Invalid recording index length."));
        }
        input.seek(SeekFrom::Start(end))?;
        let mut keys: Vec<Key> = Vec::with_capacity((index_bytes / INDEX_ENTRY) as usize);
        for _ in 0..index_bytes / INDEX_ENTRY {
            let ms = u64::from_le_bytes(read(&mut input)?);
            let offset = u64::from_le_bytes(read(&mut input)?);
            let [monitor] = read(&mut input)?;
            if offset < 8 || offset.saturating_add(RECORD_HEADER) > end || ms > duration_ms
                || keys.last().is_some_and(|last| last.offset >= offset || last.ms > ms)
            {
                return Err(invalid("Invalid recording keyframe index."));
            }
            keys.push(Key { ms, offset, monitor });
        }
        input.seek(SeekFrom::Start(8))?;
        Ok(Self { input, keys, end, duration_ms })
    }

    pub(crate) fn monitors(&self) -> Vec<u8> {
        let mut present = [false; 256];
        for key in &self.keys {
            present[usize::from(key.monitor)] = true;
        }
        present.iter().enumerate().filter_map(|(id, &yes)| yes.then_some(id as u8)).collect()
    }

    pub(crate) fn seek(&mut self, monitor: u8, ms: u64) -> io::Result<u64> {
        let partition = self.keys.partition_point(|key| key.ms <= ms);
        let key = self.keys[..partition].iter().rev().find(|key| key.monitor == monitor)
            .or_else(|| self.keys[partition..].iter().find(|key| key.monitor == monitor))
            .ok_or_else(|| invalid("The recording has no keyframe for this display."))?;
        self.input.seek(SeekFrom::Start(key.offset))?;
        Ok(key.ms)
    }

    pub(crate) fn next(&mut self) -> io::Result<Option<Record>> {
        let offset = self.input.stream_position()?;
        if offset == self.end {
            return Ok(None);
        }
        if offset.saturating_add(RECORD_HEADER) > self.end {
            return Err(invalid("Incomplete recording frame."));
        }
        let ms = u64::from_le_bytes(read(&mut self.input)?);
        let [kind, monitor, flags] = read(&mut self.input)?;
        let track = Track::try_from(kind)?;
        let bytes = u32::from_le_bytes(read(&mut self.input)?) as usize;
        let limit = if track == Track::Video { MAX_FRAME_PAYLOAD } else { MAX_AUDIO_BYTES };
        let reset = track != Track::Video && flags == AUDIO_RESET;
        if (reset && bytes != 0) || (!reset && bytes == 0) || (track != Track::Video && flags > AUDIO_RESET)
            || bytes > limit || offset + RECORD_HEADER + bytes as u64 > self.end || ms > self.duration_ms
        {
            return Err(invalid("Invalid recording frame length or timestamp."));
        }
        let mut data = vec![0; bytes];
        self.input.read_exact(&mut data)?;
        Ok(Some(Record { ms, track, monitor, flags, data }))
    }
}

fn read<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_uses_the_previous_keyframe_for_the_selected_display() {
        let mut writer = Writer::new(Vec::new()).unwrap();
        for (ms, monitor, key) in [(1, 0, true), (1, 1, true), (80, 0, false), (90, 1, true), (110, 0, true)] {
            writer.push(&Record { ms, track: Track::Video, monitor, flags: 0, data: vec![1] }, key).unwrap();
        }
        let bytes = writer.finish().unwrap();
        let mut reader = Recording::read(io::Cursor::new(&bytes)).unwrap();
        assert_eq!(reader.monitors(), [0, 1]);
        assert_eq!(reader.seek(0, 100).unwrap(), 1);
        assert_eq!(reader.next().unwrap().unwrap().monitor, 0);
        assert_eq!(reader.seek(1, 100).unwrap(), 90);
        assert_eq!(reader.next().unwrap().unwrap().ms, 90);
        assert_eq!(reader.seek(0, 500).unwrap(), 110);
        reader.next().unwrap().unwrap();
        assert!(reader.next().unwrap().is_none());
        assert!(Recording::read(io::Cursor::new(&bytes[..bytes.len() - 1])).is_err());
    }
}
